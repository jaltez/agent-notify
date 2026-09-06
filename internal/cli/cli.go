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
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/engine"
	"agent-notify/internal/event"
	"agent-notify/internal/render"
	"agent-notify/internal/sink"
	"agent-notify/internal/tray"
)

// Version is the agent-notify version.
const Version = "0.1.0"

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
  version     print version information
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
	path = config.Path(path)
	if path == "" || !fileExists(path) {
		if path != "" {
			log.Info("config not found; using defaults", "path", path)
		} else {
			log.Info("using built-in defaults (no config file)")
		}
		return config.Default()
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
	log.Info("agent-notify tray started", "version", Version, "backends", strings.Join(eng.Backends(), ","), "sinks", strings.Join(mgr.Names(), ","))

	var traySink *tray.Tray
	for _, s := range sinks {
		if t, ok := s.(*tray.Tray); ok {
			traySink = t
			break
		}
	}
	traySink.Run(ctx, func() {
		mgr.Deliver(testEvent())
	})
	return 0
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
	log.Info("agent-notify running", "version", Version, "backends", strings.Join(eng.Backends(), ","), "sinks", strings.Join(mgr.Names(), ","))
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

func cmdVersion(stdout io.Writer) int {
	fmt.Fprintf(stdout, "agent-notify %s (%s/%s, %s)\n", Version, runtime.GOOS, runtime.GOARCH, runtime.Version())
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
