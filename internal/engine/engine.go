// Package engine polls source backends, diffs consecutive snapshots into
// events, applies filters, and exposes both an event stream and a live
// state view (used by the tray and `probe`).
package engine

import (
	"context"
	"fmt"
	"log/slog"
	"path"
	"runtime"
	"sync"
	"time"

	"github.com/jaltez/agent-notify/internal/config"
	"github.com/jaltez/agent-notify/internal/event"
	"github.com/jaltez/agent-notify/internal/herdr"
)

// Config parameterizes an Engine.
type Config struct {
	Kinds    map[event.Kind]bool // event kinds allowed through to sinks
	Cooldown time.Duration       // min spacing between identical (kind,session,pane) events
	Include  []string            // session-name globs to keep; empty = all
	Exclude  []string            // session-name globs to drop
	Herdr    config.Herdr
}

// Engine watches herdr sessions and emits filtered events.
type Engine struct {
	cfg      Config
	backends []*backendRun
	log      *slog.Logger

	mu       sync.Mutex
	sessions map[string]*tracker // key: host/name

	coolMu    sync.Mutex
	cooldowns map[string]time.Time

	events  chan event.Event
	stateCh chan struct{}
}

type backendRun struct {
	b       herdr.Backend
	poll    time.Duration
	timeout time.Duration
}

// Option customizes an Engine.
type Option func(*Engine)

// WithLogger attaches a logger for warnings and debug output.
func WithLogger(l *slog.Logger) Option {
	return func(e *Engine) { e.log = l }
}

// WithBackends overrides backend resolution (tests).
func WithBackends(bs ...herdr.Backend) Option {
	return func(e *Engine) {
		e.backends = nil
		for _, b := range bs {
			e.backends = append(e.backends, &backendRun{b: b, poll: 10 * time.Millisecond, timeout: time.Second})
		}
	}
}

// New resolves the configured backends and returns a ready engine. It
// errors when no backend could be enabled at all.
func New(cfg Config, opts ...Option) (*Engine, error) {
	e := &Engine{
		cfg:       cfg,
		sessions:  map[string]*tracker{},
		cooldowns: map[string]time.Time{},
		events:    make(chan event.Event, 64),
		stateCh:   make(chan struct{}, 1),
	}
	for _, o := range opts {
		o(e)
	}
	if len(e.backends) > 0 {
		// Backends were injected (tests); skip config-based resolution.
		return e, nil
	}
	h := cfg.Herdr
	if use, explicit := want(h.Local.Enabled, runtime.GOOS != "windows"); use {
		l := herdr.NewLocal(h.Local.SocketDir, h.Local.Binary, h.Local.Timeout.D())
		if explicit || l.Available() {
			e.backends = append(e.backends, &backendRun{b: l, poll: h.Local.Poll.D(), timeout: h.Local.Timeout.D()})
		}
	}
	// Auto: enabled whenever herdr.exe resolves — native Windows, or the
	// Windows side polled from WSL via interop. A resolve failure under
	// "auto" just disables the backend.
	if use, explicit := want(h.Windows.Enabled, true); use {
		w, err := herdr.NewWindows(h.Windows.Binary, h.Windows.Sessions, h.Windows.Timeout.D())
		if err != nil {
			if explicit {
				return nil, fmt.Errorf("herdr.windows backend: %w", err)
			}
		} else {
			e.backends = append(e.backends, &backendRun{b: w, poll: h.Windows.Poll.D(), timeout: h.Windows.Timeout.D()})
		}
	}
	if use, _ := want(h.WSL.Enabled, runtime.GOOS == "windows"); use {
		wsl := herdr.NewWSL(h.WSL.Distro, h.WSL.Sessions, h.WSL.ExtraPath, h.WSL.Timeout.D(), h.WSL.Rescan.D())
		e.backends = append(e.backends, &backendRun{b: wsl, poll: h.WSL.Poll.D(), timeout: h.WSL.Timeout.D()})
	}
	if len(e.backends) == 0 {
		return nil, fmt.Errorf("no herdr backends enabled (local: %q, windows: %q, wsl: %q)", h.Local.Enabled, h.Windows.Enabled, h.WSL.Enabled)
	}
	return e, nil
}

// Backends lists the enabled backend names.
func (e *Engine) Backends() []string {
	out := make([]string, len(e.backends))
	for i, br := range e.backends {
		out[i] = br.b.Name()
	}
	return out
}

// Events is the filtered event stream.
func (e *Engine) Events() <-chan event.Event { return e.events }

// StateChange signals (coalesced) that View() would return something new.
func (e *Engine) StateChange() <-chan struct{} { return e.stateCh }

// Run polls every backend until ctx is done.
func (e *Engine) Run(ctx context.Context) {
	var wg sync.WaitGroup
	for _, br := range e.backends {
		wg.Add(1)
		go func(br *backendRun) {
			defer wg.Done()
			e.loop(ctx, br)
		}(br)
	}
	<-ctx.Done()
	wg.Wait()
}

func (e *Engine) loop(ctx context.Context, br *backendRun) {
	poll := br.poll
	if poll <= 0 {
		poll = time.Second // guard zero-value configs
	}
	ticker := time.NewTicker(poll)
	defer ticker.Stop()
	var (
		sessions []herdr.Session
		lastErr  string
		nextScan time.Time
	)
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		now := time.Now()
		if now.After(nextScan) {
			ss, err := br.b.Discover(ctx)
			if err != nil {
				if msg := err.Error(); msg != lastErr {
					e.warnf("session discovery failed: %v", err)
					lastErr = msg
				}
			} else {
				lastErr = ""
				sessions = e.filter(ss)
			}
			nextScan = now.Add(br.b.Rescan())
		}
		for _, s := range sessions {
			if ctx.Err() != nil {
				return
			}
			e.pollSession(ctx, br, s)
		}
	}
}

// pollSession fetches one session and folds the result into its tracker.
func (e *Engine) pollSession(ctx context.Context, br *backendRun, s herdr.Session) {
	key := s.Host + "/" + s.Name
	fctx, cancel := context.WithTimeout(ctx, br.timeout)
	snap, err := br.b.Fetch(fctx, s)
	cancel()

	e.mu.Lock()
	t := e.sessions[key]
	if t == nil {
		t = newTracker(s.Host, s.Name)
		e.sessions[key] = t
	}
	var evs []event.Event
	var changed bool
	if err != nil {
		evs, changed = t.fail(time.Now())
	} else {
		evs, changed = t.apply(snap, time.Now())
	}
	e.mu.Unlock()

	if err != nil {
		e.debugf("snapshot %s failed: %v", key, err)
	}
	e.emit(evs, changed)
}

// ProbeOnce performs a single synchronous discovery+fetch pass per backend
// (for `agent-notify probe`); it returns per-session fetch errors.
func (e *Engine) ProbeOnce(ctx context.Context) []error {
	var errs []error
	for _, br := range e.backends {
		ss, err := br.b.Discover(ctx)
		if err != nil {
			errs = append(errs, fmt.Errorf("%s: discover: %w", br.b.Name(), err))
			continue
		}
		for _, s := range e.filter(ss) {
			fctx, cancel := context.WithTimeout(ctx, br.timeout)
			_, err := br.b.Fetch(fctx, s)
			cancel()
			if err != nil {
				errs = append(errs, fmt.Errorf("%s: %s: %w", br.b.Name(), s.Name, err))
			}
			e.pollSession(ctx, br, s)
		}
	}
	return errs
}

func (e *Engine) filter(ss []herdr.Session) []herdr.Session {
	out := make([]herdr.Session, 0, len(ss))
	for _, s := range ss {
		if len(e.cfg.Include) > 0 && !matchAny(e.cfg.Include, s.Name) {
			continue
		}
		if matchAny(e.cfg.Exclude, s.Name) {
			continue
		}
		out = append(out, s)
	}
	return out
}

func matchAny(pats []string, s string) bool {
	for _, p := range pats {
		if ok, err := path.Match(p, s); err == nil && ok {
			return true
		}
	}
	return false
}

func (e *Engine) emit(evs []event.Event, changed bool) {
	for _, ev := range evs {
		if !e.cfg.Kinds[ev.Kind] {
			continue
		}
		if !e.cooldownAllow(ev) {
			continue
		}
		select {
		case e.events <- ev:
		default:
			e.debugf("event queue full; dropping %s %s/%s", ev.Kind, ev.Host, ev.Session)
		}
	}
	if changed {
		select {
		case e.stateCh <- struct{}{}:
		default:
		}
	}
}

func (e *Engine) cooldownAllow(ev event.Event) bool {
	if e.cfg.Cooldown <= 0 {
		return true
	}
	key := fmt.Sprintf("%s|%s|%s|%s", ev.Kind, ev.Host, ev.Session, ev.PaneID)
	e.coolMu.Lock()
	defer e.coolMu.Unlock()
	if t, ok := e.cooldowns[key]; ok && time.Since(t) < e.cfg.Cooldown {
		return false
	}
	e.cooldowns[key] = time.Now()
	return true
}

func (e *Engine) warnf(format string, args ...any) {
	if e.log != nil {
		e.log.Warn(fmt.Sprintf(format, args...))
	}
}

func (e *Engine) debugf(format string, args ...any) {
	if e.log != nil {
		e.log.Debug(fmt.Sprintf(format, args...))
	}
}

// want resolves an enabled = auto|on|off value against its auto condition.
func want(enabled string, autoOn bool) (use, explicit bool) {
	switch enabled {
	case "on":
		return true, true
	case "off":
		return false, false
	default: // "", "auto"
		return autoOn, false
	}
}
