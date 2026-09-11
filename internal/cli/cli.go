// Package cli wires configuration, engine, and sinks into the
// agent-notify commands: tray, run, monitor, probe, test, init, version.
package cli

import (
	"context"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/jaltez/agent-notify/internal/buildinfo"
	"github.com/jaltez/agent-notify/internal/config"
	"github.com/jaltez/agent-notify/internal/engine"
	"github.com/jaltez/agent-notify/internal/event"
	"github.com/jaltez/agent-notify/internal/render"
	"github.com/jaltez/agent-notify/internal/sink"
	"github.com/jaltez/agent-notify/internal/tray"
	"github.com/jaltez/agent-notify/internal/update"
)

// version is sourced from buildinfo (injected at build time).
func version() string { return buildinfo.Version }

const usage = `agent-notify — tray notifier for AI coding agents (herdr today)

Usage:
  agent-notify [flags] [command]

Commands:
  (default)   tray icon + attention popups (the normal way to run it)
  run         headless daemon (no tray; configured sinks only)
  monitor     print events as they happen; no notifications
  probe       one pass: show sessions, agents and sink availability
  test        send a test notification through every configured sink
  init        write an annotated example config
  config      path | edit | validate the configuration
  update      self-update from GitHub releases (--check to only look)
  version     print version information
  flytest     diagnostics: drive the flyout show/hide cycle
  help        show this help

Flags:
  --config PATH   config file (default: $AGENT_NOTIFY_CONFIG or
                  <user config dir>/agent-notify/config.toml)
  -v, --verbose   debug logging
`

// Run executes the CLI and returns the process exit code.
func Run(argv []string, stdout, stderr io.Writer) int {
	attachParentConsole() // windowsgui builds: reattach for CLI subcommands
	var (
		configPath string
		verbose    bool
	)
	args := argv[1:]
	i := 0
	for i < len(args) {
		a := args[i]
		switch {
		case a == "--config" && i+1 < len(args):
			configPath = args[i+1]
			i += 2
		case strings.HasPrefix(a, "--config="):
			configPath = strings.TrimPrefix(a, "--config=")
			i++
		case a == "-v" || a == "--verbose":
			verbose = true
			i++
		case a == "-h" || a == "--help" || a == "help":
			fmt.Fprint(stdout, usage)
			return 0
		case a == "--version":
			return cmdVersion(stdout)
		default:
			goto subcommand
		}
	}
subcommand:
	rest := args[i:]
	cmd := "tray"
	if len(rest) > 0 {
		cmd = rest[0]
		rest = rest[1:]
	}

	log := newLogger(stderr, verbose)
	switch cmd {
	case "tray":
		return cmdTray(configPath, log, stderr)
	case "run":
		return cmdRun(configPath, log)
	case "monitor":
		return cmdMonitor(configPath, rest, stdout, log)
	case "probe":
		return cmdProbe(configPath, stdout, log)
	case "test":
		return cmdTest(configPath, stdout, log)
	case "init":
		return cmdInit(configPath, rest, stdout)
	case "version":
		return cmdVersion(stdout)
	case "flytest":
		return cmdFlytest(configPath, stdout, log)
	case "update":
		return cmdUpdate(rest, stdout, log)
	case "config":
		return cmdConfig(configPath, rest, stdout, log)
	default:
		fmt.Fprintf(stderr, "unknown command %q\n\n%s", cmd, usage)
		return 2
	}
}

func newLogger(w io.Writer, verbose bool) *slog.Logger {
	level := slog.LevelInfo
	if verbose {
		level = slog.LevelDebug
	}
	return slog.New(slog.NewTextHandler(w, &slog.HandlerOptions{Level: level}))
}

// loadConfig resolves and loads the config, logging hints on the way.
func loadConfig(path string, log *slog.Logger) config.Config {
	path, source := config.Resolve(path)
	if path == "" {
		log.Info("using built-in defaults (no config file)")
		return config.Default()
	}
	if !fileExists(path) {
		log.Info("config not found; using defaults", "path", path)
		return config.Default()
	}
	if source == "windows-shared" {
		log.Info("using Windows-side config (WSL auto-share)", "path", path)
	}
	cfg, warnings, err := config.Load(path)
	for _, w := range warnings {
		log.Warn(w)
	}
	if err != nil {
		log.Error("failed to load config; using defaults", "error", err)
		return config.Default()
	}
	return cfg
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// buildEngine assembles the engine from config.
func buildEngine(cfg config.Config, kindsOverride []string, log *slog.Logger) (*engine.Engine, error) {
	names := cfg.Events
	if kindsOverride != nil {
		names = kindsOverride
	}
	kinds, err := event.ParseSet(names)
	if err != nil {
		return nil, err
	}
	return engine.New(engine.Config{
		Kinds:    kindSet(kinds),
		Cooldown: cfg.Cooldown.D(),
		Include:  cfg.Herdr.Include,
		Exclude:  cfg.Herdr.Exclude,
		Herdr:    cfg.Herdr,
	}, engine.WithLogger(log))
}

func kindSet(ks []event.Kind) map[event.Kind]bool {
	m := make(map[event.Kind]bool, len(ks))
	for _, k := range ks {
		m[k] = true
	}
	return m
}

// buildSinks creates the configured sinks. withTray adds/keeps the tray
// sink; headless commands pass false and tray sinks are skipped with a
// note. Returns the sinks and notes to log.
// fleetExtra composes the toast's third line: space + live fleet summary.
func fleetExtra(eng *engine.Engine) func(event.Event) string {
	return func(ev event.Event) string {
		var parts []string
		if ev.Project != "" {
			parts = append(parts, ev.Project)
		}
		if ev.Host != "" || ev.Session != "" {
			parts = append(parts, ev.Host+"/"+ev.Session)
		}
		line := strings.Join(parts, " · ")
		summary := eng.View().Summary()
		if line == "" {
			return summary
		}
		return line + " — " + summary
	}
}

func buildSinks(cfg config.Config, eng *engine.Engine, log *slog.Logger, withTray bool) ([]sink.Sink, error) {
	r, err := render.New(cfg.Render.Title, cfg.Render.Body)
	if err != nil {
		return nil, fmt.Errorf("render templates: %w", err)
	}
	trayAdded := false
	var sinks []sink.Sink
	for _, sc := range cfg.Sink {
		if sc.Disabled {
			continue
		}
		switch sc.Type {
		case "tray":
			if !withTray {
				log.Info("skipping tray sink in headless mode")
				continue
			}
			if trayAdded {
				continue
			}
			trayAdded = true
			var pop *sink.Popup
			if sc.Popup == nil || *sc.Popup {
				pop, err = sink.NewPopup(sc, r)
				if err != nil {
					log.Warn("attention popups unavailable; tray will be icon-only", "error", err)
					pop = nil
				} else {
					pop.SetExtra(fleetExtra(eng))
				}
			}
			sinks = append(sinks, tray.New(eng, pop, log))
		case "popup":
			pop, err := sink.NewPopup(sc, r)
			if err != nil {
				return nil, err
			}
			pop.SetExtra(fleetExtra(eng))
			sinks = append(sinks, pop)
		case "bell":
			sinks = append(sinks, sink.NewBell(os.Stderr))
		case "command":
			c, err := sink.NewCommand(sc)
			if err != nil {
				return nil, err
			}
			sinks = append(sinks, c)
		case "webhook":
			wh, err := sink.NewWebhook(sc)
			if err != nil {
				return nil, err
			}
			sinks = append(sinks, wh)
		case "log":
			sinks = append(sinks, sink.NewLog(os.Stderr, false))
		}
	}
	if withTray && !trayAdded {
		// The tray command always shows the tray, config or not.
		pop, perr := sink.NewPopup(config.Sink{}, r)
		if perr != nil {
			log.Warn("attention popups unavailable; tray will be icon-only", "error", perr)
			pop = nil
		} else {
			pop.SetExtra(fleetExtra(eng))
		}
		sinks = append(sinks, tray.New(eng, pop, log))
	}
	return sinks, nil
}

func cmdTray(configPath string, log *slog.Logger, stderr io.Writer) int {
	if !guiAvailable() {
		fmt.Fprintln(stderr, "no display available for the tray icon; use `agent-notify run` or `agent-notify monitor`")
		return 1
	}
	release, err := acquireTrayLock()
	if err != nil {
		fmt.Fprintln(stderr, err)
		return 1
	}
	defer release()
	cfg := loadConfig(configPath, log)
	eng, err := buildEngine(cfg, nil, log)
	if err != nil {
		log.Error("cannot start", "error", err)
		return 1
	}
	sinks, err := buildSinks(cfg, eng, log, true)
	if err != nil {
		log.Error("invalid sink configuration", "error", err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	mgr := sink.NewManager(log)
	for _, s := range sinks {
		mgr.Add(s)
	}
	go eng.Run(ctx)
	go func() {
		mgr.Run(ctx)
	}()
	log.Info("agent-notify tray started", "version", version(), "backends", strings.Join(eng.Backends(), ","), "sinks", strings.Join(mgr.Names(), ","))

	var traySink *tray.Tray
	for _, s := range sinks {
		if t, ok := s.(*tray.Tray); ok {
			traySink = t
			break
		}
	}
	// Self-update lifecycle: silent check at startup + daily; toast and a
	// menu item when a release lands; one-click apply + restart.
	wireUpdates(ctx, traySink, mgr, log)

	// The flyout window must be created on this (locked, main) thread
	// before the message loop starts pumping.
	traySink.InitFlyout()
	traySink.Run(ctx, func() {
		mgr.Deliver(testEvent())
	})
	return 0
}

// wireUpdates wires the tray's update menu to the GitHub release check.
func wireUpdates(ctx context.Context, traySink *tray.Tray, mgr *sink.Manager, log *slog.Logger) {
	var (
		mu      sync.Mutex
		checker *update.Checker
		status  update.Status
	)

	checkOnce := func(quiet bool) {
		mu.Lock()
		if checker == nil {
			c, err := update.New()
			if err != nil {
				mu.Unlock()
				log.Warn("updater unavailable", "error", err)
				return
			}
			checker = c
		}
		c := checker
		mu.Unlock()

		st, err := c.Check(ctx)
		if err != nil {
			log.Warn("update check failed", "error", err)
			return
		}
		mu.Lock()
		status = st
		mu.Unlock()
		log.Info("update check", "result", st.Describe())
		if st.UpdateAvail {
			traySink.SetUpdateKnown(true)
			if !quiet {
				mgr.Deliver(updateEvent(st.Latest))
			}
		} else if !quiet {
			mgr.Deliver(statusToastEvent(st))
		}
	}

	traySink.SetUpdateHooks(
		func() { go checkOnce(false) },
		func() {
			go func() {
				mu.Lock()
				c, st := checker, status
				mu.Unlock()
				if c == nil || st.Release == nil {
					return
				}
				if err := c.Apply(ctx, st.Release); err != nil {
					log.Error("self-update failed", "error", err)
					return
				}
				log.Info("self-update applied; restarting", "version", st.Latest)
				if err := update.RestartSelf(); err != nil {
					log.Error("restart failed; start agent-notify again", "error", err)
				}
				traySink.QuitApp()
			}()
		},
	)

	go func() {
		select {
		case <-time.After(30 * time.Second):
			checkOnce(true)
		case <-ctx.Done():
			return
		}
		tick := time.NewTicker(24 * time.Hour)
		defer tick.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-tick.C:
				checkOnce(true)
			}
		}
	}()
}

func updateEvent(latest string) event.Event {
	return event.Event{
		Kind:    event.KindUpdateAvail,
		Time:    time.Now(),
		Session: "agent-notify",
		Title:   "v" + latest + " available — tray menu → Update & restart",
	}
}

func statusToastEvent(st update.Status) event.Event {
	return event.Event{
		Kind:    event.KindTest,
		Time:    time.Now(),
		Session: "agent-notify",
		Agent:   "update check",
		Title:   st.Describe(),
	}
}

func cmdRun(configPath string, log *slog.Logger) int {
	cfg := loadConfig(configPath, log)
	eng, err := buildEngine(cfg, nil, log)
	if err != nil {
		log.Error("cannot start", "error", err)
		return 1
	}
	sinks, err := buildSinks(cfg, eng, log, false)
	if err != nil {
		log.Error("invalid sink configuration", "error", err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	mgr := sink.NewManager(log)
	for _, s := range sinks {
		mgr.Add(s)
	}
	go eng.Run(ctx)
	go mgr.Run(ctx)
	log.Info("agent-notify running", "version", version(), "backends", strings.Join(eng.Backends(), ","), "sinks", strings.Join(mgr.Names(), ","))
	<-ctx.Done()
	return 0
}

func cmdMonitor(configPath string, args []string, stdout io.Writer, log *slog.Logger) int {
	fs := flag.NewFlagSet("monitor", flag.ContinueOnError)
	jsonMode := fs.Bool("json", false, "one JSON object per line")
	all := fs.Bool("all", false, "emit every event kind, not just the configured set")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	cfg := loadConfig(configPath, log)
	var kinds []string
	if *all {
		kinds = []string{"all"}
	}
	eng, err := buildEngine(cfg, kinds, log)
	if err != nil {
		log.Error("cannot start", "error", err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	mgr := sink.NewManager(log)
	mgr.Add(sink.NewLog(stdout, *jsonMode))
	go eng.Run(ctx)
	go mgr.Run(ctx)
	log.Info("monitoring", "backends", strings.Join(eng.Backends(), ","))
	<-ctx.Done()
	return 0
}

func cmdProbe(configPath string, stdout io.Writer, log *slog.Logger) int {
	cfg := loadConfig(configPath, log)
	eng, err := buildEngine(cfg, []string{"all"}, log)
	if err != nil {
		fmt.Fprintf(stdout, "no backends available: %v\n", err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	fmt.Fprintf(stdout, "backends: %s\n", strings.Join(eng.Backends(), ", "))
	errs := eng.ProbeOnce(ctx)
	v := eng.View()
	if len(v.Sessions) == 0 {
		fmt.Fprintln(stdout, "sessions: none seen (is a herdr server running?)")
	}
	for _, s := range v.Sessions {
		state := "up"
		if !s.Up {
			state = "DOWN"
		}
		fmt.Fprintf(stdout, "\n%s/%s  [%s]\n", s.Host, s.Name, state)
		if len(s.Agents) == 0 {
			fmt.Fprintln(stdout, "  (no agents)")
		}
		for _, a := range s.Agents {
			title := a.Title
			if len(title) > 48 {
				title = title[:48] + "…"
			}
			fmt.Fprintf(stdout, "  %-8s %-10s %-8s %s\n", a.Status, a.Name, a.PaneID, title)
		}
	}
	for _, e := range errs {
		fmt.Fprintf(stdout, "\nerror: %v\n", e)
	}
	printSinkAvailability(cfg, stdout)
	return 0
}

func printSinkAvailability(cfg config.Config, stdout io.Writer) {
	fmt.Fprintln(stdout, "\nsinks:")
	r, _ := render.New(cfg.Render.Title, cfg.Render.Body)
	if _, err := sink.NewPopup(config.Sink{}, r); err != nil {
		fmt.Fprintf(stdout, "  popup: unavailable (%v)\n", err)
	} else {
		fmt.Fprintln(stdout, "  popup: available")
	}
	fmt.Fprintf(stdout, "  tray: %v\n", map[bool]string{true: "available", false: "no display"}[guiAvailable()])
	fmt.Fprintln(stdout, "  bell, command, webhook: available (config-driven)")
}

func cmdTest(configPath string, stdout io.Writer, log *slog.Logger) int {
	cfg := loadConfig(configPath, log)
	eng, err := buildEngine(cfg, nil, log)
	if err != nil {
		log.Error("cannot start", "error", err)
		return 1
	}
	if _, err := render.New(cfg.Render.Title, cfg.Render.Body); err != nil {
		fmt.Fprintf(stdout, "render templates: %v\n", err)
		return 1
	}
	// withTray: the tray sink's Deliver forwards to its popup, which works
	// headless — testing it shows exactly what the tray would fire.
	sinks, err := buildSinks(cfg, eng, log, true)
	if err != nil {
		fmt.Fprintf(stdout, "invalid sink configuration: %v\n", err)
		return 1
	}
	if len(sinks) == 0 {
		fmt.Fprintln(stdout, "no sinks configured; nothing to test")
		return 0
	}
	ev := testEvent()
	failed := false
	for _, s := range sinks {
		ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
		err := s.Deliver(ctx, ev)
		cancel()
		if err != nil {
			failed = true
			fmt.Fprintf(stdout, "%-8s FAIL: %v\n", s.Name(), err)
			continue
		}
		fmt.Fprintf(stdout, "%-8s ok\n", s.Name())
	}
	if failed {
		return 1
	}
	return 0
}

func testEvent() event.Event {
	return event.Event{
		Kind:    event.KindTest,
		Time:    time.Now(),
		Source:  "herdr",
		Host:    "local",
		Session: "default",
		Agent:   "demo-agent",
		Title:   "agent-notify test notification",
	}
}

func cmdInit(configPath string, args []string, stdout io.Writer) int {
	fs := flag.NewFlagSet("init", flag.ContinueOnError)
	force := fs.Bool("force", false, "overwrite an existing config")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	path := config.Path(configPath)
	if path == "" {
		fmt.Fprintln(stdout, "cannot determine a config path; pass --config")
		return 1
	}
	if fileExists(path) && !*force {
		fmt.Fprintf(stdout, "%s already exists (use --force to overwrite)\n", path)
		return 1
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		fmt.Fprintf(stdout, "create config directory: %v\n", err)
		return 1
	}
	if err := os.WriteFile(path, config.Example, 0o644); err != nil {
		fmt.Fprintf(stdout, "write config: %v\n", err)
		return 1
	}
	fmt.Fprintf(stdout, "wrote %s\n", path)
	return 0
}

func cmdFlytest(configPath string, stdout io.Writer, log *slog.Logger) int {
	cfg := loadConfig(configPath, log)
	eng, err := buildEngine(cfg, nil, log)
	if err != nil {
		fmt.Fprintln(stdout, "engine:", err)
		return 1
	}
	fly, err := newTrayFlyout(eng, log)
	if err != nil {
		fmt.Fprintln(stdout, "flyout:", err)
		return 1
	}
	fly.SelfTest()
	return 0
}

func cmdVersion(stdout io.Writer) int {
	fmt.Fprintf(stdout, "agent-notify %s (%s/%s, %s)\n", version(), runtime.GOOS, runtime.GOARCH, runtime.Version())
	return 0
}

// cmdUpdate checks GitHub releases and optionally self-updates.
func cmdUpdate(args []string, stdout io.Writer, log *slog.Logger) int {
	fs := flag.NewFlagSet("update", flag.ContinueOnError)
	checkOnly := fs.Bool("check", false, "only report the latest release")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	checker, err := update.New()
	if err != nil {
		fmt.Fprintln(stdout, "updater:", err)
		return 1
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	st, err := checker.Check(ctx)
	if err != nil {
		fmt.Fprintf(stdout, "update check failed: %v\n", err)
		return 1
	}
	fmt.Fprintln(stdout, st.Describe())
	if !st.UpdateAvail || *checkOnly {
		return 0
	}
	if err := checker.Apply(ctx, st.Release); err != nil {
		fmt.Fprintf(stdout, "update failed: %v\n", err)
		return 1
	}
	fmt.Fprintf(stdout, "updated to %s — restart agent-notify to apply\n", st.Latest)
	return 0
}

// cmdConfig implements `config path|edit|validate`.
func cmdConfig(configPath string, args []string, stdout io.Writer, log *slog.Logger) int {
	sub := "path"
	if len(args) > 0 {
		sub = args[0]
	}
	switch sub {
	case "path":
		p, source := config.Resolve(configPath)
		if p == "" {
			fmt.Fprintln(stdout, "no config path resolvable")
			return 1
		}
		fmt.Fprintf(stdout, "%s (source: %s)\n", p, source)
	case "edit":
		p := config.Path(configPath)
		if p == "" {
			fmt.Fprintln(stdout, "cannot determine a config path")
			return 1
		}
		if !fileExists(p) {
			fmt.Fprintf(stdout, "%s does not exist; run `agent-notify init` first\n", p)
			return 1
		}
		editor := os.Getenv("VISUAL")
		if editor == "" {
			editor = os.Getenv("EDITOR")
		}
		if editor == "" {
			if runtime.GOOS == "windows" {
				editor = "notepad.exe"
			} else {
				editor = "vi"
			}
		}
		cmd := exec.Command(editor, p)
		cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
		if err := cmd.Run(); err != nil {
			fmt.Fprintf(stdout, "editor: %v\n", err)
			return 1
		}
	case "validate":
		p, _ := config.Resolve(configPath)
		if p == "" || !fileExists(p) {
			fmt.Fprintln(stdout, "config: none found (defaults in effect) — ok")
			return 0
		}
		if _, warnings, err := config.Load(p); err != nil {
			fmt.Fprintf(stdout, "INVALID: %v\n", err)
			return 1
		} else if len(warnings) > 0 {
			for _, w := range warnings {
				fmt.Fprintf(stdout, "warning: %s\n", w)
			}
		}
		fmt.Fprintf(stdout, "%s — ok\n", p)
	default:
		fmt.Fprintf(stdout, "unknown config subcommand %q (path|edit|validate)\n", sub)
		return 2
	}
	return 0
}

// guiAvailable reports whether a tray icon can be shown.
func guiAvailable() bool {
	switch runtime.GOOS {
	case "windows", "darwin":
		return true
	default:
		return os.Getenv("DISPLAY") != "" || os.Getenv("WAYLAND_DISPLAY") != ""
	}
}
