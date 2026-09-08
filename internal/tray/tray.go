// Package tray renders the agent-notify system-tray UI: an icon whose
// color follows the most severe live state, a tooltip summary, and a
// per-session/per-agent menu. Attention popups are delegated to the popup
// sink, which the Tray forwards events to.
//
// Menu updates are in place: Windows destroys a popup menu whose items
// get reset while it is open, so titles are updated via SetTitle and a
// full rebuild happens only when the menu's structure (sessions, agent
// counts) changes.
package tray

import (
	"context"
	"fmt"
	"log/slog"
	"strconv"
	"strings"
	"sync/atomic"
	"time"

	"fyne.io/systray"

	"github.com/jaltez/agent-notify/internal/engine"
	"github.com/jaltez/agent-notify/internal/event"
	"github.com/jaltez/agent-notify/internal/icon"
	"github.com/jaltez/agent-notify/internal/sink"
)

const (
	iconSize   = 32
	debounce   = 300 * time.Millisecond
	blinkEvery = 500 * time.Millisecond
)

// menuTree mirrors the live menu so rows can be updated in place.
type menuTree struct {
	summary *systray.MenuItem
	headers []*systray.MenuItem // one per session, in view order
	agents  []*systray.MenuItem // flattened across sessions, in view order
}

// Tray is both a sink (event → popup) and a live view renderer.
type Tray struct {
	eng *engine.Engine
	pop *sink.Popup // nil = silent icon-only mode
	log *slog.Logger
	fly *flyout // detail panel on tray left click (nil where unsupported)

	// touched only from the single refresh goroutine.
	tree          *menuTree
	lastFP        string       // rendered content fingerprint
	lastStructure string       // structural key: sessions + agent counts
	curSev        atomic.Value // string: last rendered severity (blink loop reads)
}

// New builds the tray sink.
func New(eng *engine.Engine, pop *sink.Popup, log *slog.Logger) *Tray {
	return &Tray{eng: eng, pop: pop, log: log}
}

// Name implements sink.Sink.
func (t *Tray) Name() string { return "tray" }

// Deliver forwards events to the popup sink (if enabled).
func (t *Tray) Deliver(ctx context.Context, ev event.Event) error {
	if t.pop == nil {
		return nil
	}
	return t.pop.Deliver(ctx, ev)
}

// Run blocks running the tray UI; call from the main goroutine. It returns
// after the user quits (or ctx is done), via systray.Quit().
func (t *Tray) Run(ctx context.Context, onTest func()) {
	systray.Run(
		func() { t.onReady(ctx, onTest) },
		func() {},
	)
}

func (t *Tray) onReady(ctx context.Context, onTest func()) {
	systray.SetIcon(icon.Bytes("idle", iconSize))
	systray.SetTooltip("agent-notify — starting…")
	t.render(onTest)
	go t.refreshLoop(ctx, onTest)
	go t.blinkLoop(ctx)
}

// InitFlyout creates the flyout panel. It must run on the main OS thread
// BEFORE Run enters the message loop: window creation has thread
// affinity, and only that thread pumps messages. On success, tray left
// click toggles the panel; right click keeps the native menu.
func (t *Tray) InitFlyout() {
	fly, ferr := newFlyout(t.eng, t.log)
	if ferr != nil {
		t.log.Warn("flyout panel unavailable; left click will open the menu", "error", ferr)
		return
	}
	t.fly = fly
	systray.SetOnTapped(t.fly.toggle)
}

// refreshLoop renders the view on state changes, debouncing bursts.
func (t *Tray) refreshLoop(ctx context.Context, onTest func()) {
	for {
		select {
		case <-ctx.Done():
			systray.Quit()
			return
		case <-t.eng.StateChange():
		}
		timer := time.NewTimer(debounce)
	wait:
		for {
			select {
			case <-t.eng.StateChange():
				timer.Reset(debounce)
			case <-timer.C:
				break wait
			case <-ctx.Done():
				systray.Quit()
				return
			}
		}
		t.render(onTest)
		if t.fly != nil {
			t.fly.notify()
		}
	}
}

func (t *Tray) render(onTest func()) {
	v := t.eng.View()
	t.curSev.Store(v.Severity())
	fp := fingerprint(v)
	if fp == t.lastFP {
		return // nothing view-relevant changed; keep the menu untouched
	}

	systray.SetIcon(icon.Bytes(v.Severity(), iconSize))
	systray.SetTooltip("agent-notify — " + v.Summary())

	if structure := structuralKey(v); structure != t.lastStructure {
		t.lastStructure = structure
		t.rebuildMenu(v, onTest)
	} else if t.tree != nil {
		t.updateMenu(v)
	}
	t.lastFP = fp
}

// structuralKey captures the menu's row layout. Anything else (statuses,
// titles, projects) updates in place.
func structuralKey(v engine.View) string {
	var b strings.Builder
	b.WriteString(strconv.Itoa(len(v.Sessions)))
	for _, s := range v.Sessions {
		b.WriteByte('|')
		b.WriteString(s.Host)
		b.WriteByte('/')
		b.WriteString(s.Name)
		b.WriteByte(':')
		if s.Up {
			b.WriteByte('u')
		} else {
			b.WriteByte('d')
		}
		b.WriteString(strconv.Itoa(len(s.Agents)))
	}
	return b.String()
}

// fingerprint captures everything the tray renders; identical fingerprint
// ⇒ identical pixels, skip the redraw.
func fingerprint(v engine.View) string {
	var b strings.Builder
	b.WriteString(v.Severity())
	b.WriteByte('|')
	b.WriteString(v.Summary())
	b.WriteByte('|')
	b.WriteString(structuralKey(v))
	for _, s := range v.Sessions {
		for _, a := range s.Agents {
			b.WriteByte('|')
			b.WriteString(agentLabel(a))
			b.WriteByte(' ')
			b.WriteString(a.Title)
		}
	}
	return b.String()
}

// attentionSeverities make the tray icon blink: states that want a human.
var attentionSeverities = map[string]bool{"blocked": true, "waiting": true}

// blinkLoop alternates the tray icon between the filled severity disc and
// a hollow ring while the fleet is in an attention state (blocked, or
// agents stopped and waiting). Steady otherwise. Runs on its own
// goroutine; systray setters are internally synchronized.
func (t *Tray) blinkLoop(ctx context.Context) {
	phase := false
	tick := time.NewTicker(blinkEvery)
	defer tick.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
		}
		sev, _ := t.curSev.Load().(string)
		if !attentionSeverities[sev] {
			if phase { // attention ended: settle back to the steady icon
				phase = false
				systray.SetIcon(icon.Bytes(sev, iconSize))
			}
			continue
		}
		phase = !phase
		if phase {
			systray.SetIcon(icon.Bytes(sev, iconSize))
		} else {
			systray.SetIcon(icon.BytesHollow(sev, iconSize))
		}
	}
}

// rebuildMenu (re)creates the whole menu. ResetMenu closes removed items'
// ClickedCh channels, so the per-item handler goroutines exit cleanly.
// Only structural changes reach this.
func (t *Tray) rebuildMenu(v engine.View, onTest func()) {
	systray.ResetMenu()

	summary := systray.AddMenuItem(v.Summary(), "Live agent status")
	summary.Disable()
	systray.AddSeparator()

	tree := &menuTree{summary: summary}
	if len(v.Sessions) == 0 {
		m := systray.AddMenuItem("No sessions yet", "Waiting for herdr sessions to appear")
		m.Disable()
	}
	for _, s := range v.Sessions {
		hdr := systray.AddMenuItem(sessionLabel(s), "herdr session")
		hdr.Disable()
		tree.headers = append(tree.headers, hdr)
		for _, a := range s.Agents {
			item := hdr.AddSubMenuItem(agentLabel(a), a.Title)
			item.Disable()
			tree.agents = append(tree.agents, item)
		}
		if len(s.Agents) == 0 && s.Up {
			empty := hdr.AddSubMenuItem("(no agents)", "")
			empty.Disable()
		}
	}

	systray.AddSeparator()
	if onTest != nil {
		tm := systray.AddMenuItem("Test notification", "Send a test event through the sinks")
		go func() {
			for range tm.ClickedCh {
				onTest()
			}
		}()
	}
	quit := systray.AddMenuItem("Quit", "Stop agent-notify")
	go func() {
		for range quit.ClickedCh {
			systray.Quit()
		}
	}()
	t.tree = tree
}

// updateMenu rewrites titles/tooltips on the existing menu — safe while
// the menu is open (Windows paints the new strings in place).
func (t *Tray) updateMenu(v engine.View) {
	t.tree.summary.SetTitle(v.Summary())
	hi, row := 0, 0
	for _, s := range v.Sessions {
		if hi < len(t.tree.headers) {
			t.tree.headers[hi].SetTitle(sessionLabel(s))
			hi++
		}
		// agent rows are flattened in the order the menu was built with
		// (the engine's View is already sorted)
		for _, a := range s.Agents {
			if row < len(t.tree.agents) {
				t.tree.agents[row].SetTitle(agentLabel(a))
				t.tree.agents[row].SetTooltip(a.Title)
				row++
			}
		}
	}
}

func sessionLabel(s engine.SessionView) string {
	label := fmt.Sprintf("%s/%s", s.Host, s.Name)
	if !s.Up {
		label += " · offline"
	}
	return label
}

func agentLabel(a engine.AgentView) string {
	status := a.Status
	if status == "" {
		status = "unknown"
	}
	label := status + " · " + a.Name
	if a.Project != "" {
		label += " — " + a.Project
	}
	if a.Focused {
		label += " ✱"
	}
	return label
}
