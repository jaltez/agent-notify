// Package config loads and validates agent-notify configuration.
//
// Precedence for the config file location: --config flag, then the
// AGENT_NOTIFY_CONFIG environment variable, then
// <os.UserConfigDir>/agent-notify/config.toml. A missing file means "use the
// defaults" — agent-notify works with zero configuration.
package config

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"time"

	_ "embed"

	"github.com/BurntSushi/toml"

	"github.com/jaltez/agent-notify/internal/event"
)

// Example is the fully annotated config written by `agent-notify init`.
//
//go:embed example.toml
var Example []byte

// Defaults for render templates.
const (
	DefaultTitleTemplate = `{{if .Agent}}{{.Agent}}{{else}}{{.Session}}{{end}} {{.Verb}}`
	DefaultBodyTemplate  = `{{if .Title}}{{.Title}}{{else if .Project}}{{.Project}}{{else}}{{.Host}}/{{.Session}}{{end}}`
)

// Config is the root configuration.
type Config struct {
	Events   []string `toml:"events"`   // event kinds that trigger sinks; default "attention"
	Cooldown Duration `toml:"cooldown"` // suppress identical (session,pane,kind) events within this window
	Render   Render   `toml:"render"`
	Herdr    Herdr    `toml:"herdr"`
	Sink     []Sink   `toml:"sink"`
}

// Render customizes how event titles and bodies are produced.
type Render struct {
	Title string `toml:"title"` // Go text/template over event.Event
	Body  string `toml:"body"`
}

// Herdr configures the herdr source and its backends.
type Herdr struct {
	Include []string `toml:"include"` // session-name globs to keep; empty = all
	Exclude []string `toml:"exclude"` // session-name globs to drop

	Local   Local   `toml:"local"`
	Windows Windows `toml:"windows"`
	WSL     WSL     `toml:"wsl"`
}

// Local is the native-socket backend (Linux, WSL, macOS).
type Local struct {
	Enabled   string   `toml:"enabled"`    // auto | on | off
	Poll      Duration `toml:"poll"`       // seconds between snapshots
	Timeout   Duration `toml:"timeout"`    // per-snapshot command timeout
	SocketDir string   `toml:"socket_dir"` // override ~/.config/herdr
	Binary    string   `toml:"binary"`     // herdr binary name/path
}

// Windows is the herdr.exe backend: Windows-native sessions, or Windows
// sessions polled from WSL via interop.
type Windows struct {
	Enabled  string   `toml:"enabled"`  // auto | on | off
	Sessions []string `toml:"sessions"` // extra named sessions besides the default one
	Poll     Duration `toml:"poll"`
	Timeout  Duration `toml:"timeout"`
	Binary   string   `toml:"binary"` // "auto" picks herdr/herdr.exe from PATH
}

// WSL polls sessions inside a WSL distro from a Windows-native process,
// via wsl.exe interop.
type WSL struct {
	Enabled   string   `toml:"enabled"`    // auto | on | off
	Distro    string   `toml:"distro"`     // WSL distro name; empty = default
	Sessions  []string `toml:"sessions"`   // restrict auto-discovery to these session names
	ExtraPath []string `toml:"extra_path"` // extra PATH entries when invoking herdr inside WSL
	Poll      Duration `toml:"poll"`
	Timeout   Duration `toml:"timeout"`
	Rescan    Duration `toml:"rescan"` // how often to re-discover WSL sessions
}

// Sink is one notifier. The Type field selects the flavor; the remaining
// fields are shared across flavors and documented per type in the example
// config. Multiple sinks of the same type are allowed.
type Sink struct {
	Type     string `toml:"type"` // tray | popup | bell | command | webhook | log
	Disabled bool   `toml:"disabled"`

	// tray & popup
	Popup  *bool    `toml:"popup"`  // tray: fire attention popups; default true
	AppID  string   `toml:"app_id"` // popup: Windows toast AppUserModelID
	Binary string   `toml:"binary"` // popup: override powershell.exe / notify-send path
	Expire Duration `toml:"expire"` // popup: notify-send expiration
	Image  *bool    `toml:"image"`  // popup: severity logo in toasts; default true

	// command
	Command []string `toml:"command"` // argv template per element
	Shell   bool     `toml:"shell"`   // run a single command string via the OS shell

	// webhook
	URL          string            `toml:"url"`
	Method       string            `toml:"method"`        // default POST
	Headers      map[string]string `toml:"headers"`       // extra HTTP headers
	ContentType  string            `toml:"content_type"`  // used with body_template
	BodyTemplate string            `toml:"body_template"` // custom body; default is the event as JSON
	Timeout      Duration          `toml:"timeout"`       // command & webhook; default 10s
}

// Sink types accepted in Sink.Type.
var SinkTypes = []string{"tray", "popup", "bell", "command", "webhook", "log"}

// Default returns the built-in configuration used when no file exists.
func Default() Config {
	on := true
	return Config{
		Events:   event.Names(event.Attention),
		Cooldown: Duration{500 * time.Millisecond},
		Herdr: Herdr{
			Local: Local{
				Enabled: "auto",
				Poll:    Duration{time.Second},
				Timeout: Duration{3 * time.Second},
				Binary:  "herdr",
			},
			Windows: Windows{
				Enabled: "auto",
				Poll:    Duration{2 * time.Second},
				Timeout: Duration{6 * time.Second},
				Binary:  "auto",
			},
			WSL: WSL{
				Enabled: "auto",
				Poll:    Duration{2 * time.Second},
				Timeout: Duration{10 * time.Second},
				Rescan:  Duration{30 * time.Second},
			},
		},
		Sink: []Sink{{Type: "tray", Popup: &on}},
	}
}

// Load reads path over the defaults and validates the result. Warnings
// cover keys that decoded into nothing (usually typos).
func Load(path string) (cfg Config, warnings []string, err error) {
	cfg = Default()
	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, nil, fmt.Errorf("read config: %w", err)
	}
	md, err := toml.Decode(string(data), &cfg)
	if err != nil {
		return cfg, nil, fmt.Errorf("parse config %s: %w", path, err)
	}
	for _, key := range md.Undecoded() {
		warnings = append(warnings, "unknown or misplaced config key: "+key.String())
	}
	if err := cfg.Validate(); err != nil {
		return cfg, warnings, fmt.Errorf("invalid config %s: %w", path, err)
	}
	return cfg, warnings, nil
}

// Validate checks the merged configuration for structural errors.
func (c Config) Validate() error {
	if _, err := event.ParseSet(c.Events); err != nil {
		return err
	}
	for _, s := range []struct{ name, val string }{
		{"herdr.local.enabled", c.Herdr.Local.Enabled},
		{"herdr.windows.enabled", c.Herdr.Windows.Enabled},
		{"herdr.wsl.enabled", c.Herdr.WSL.Enabled},
	} {
		if !validEnabled(s.val) {
			return fmt.Errorf("%s: must be auto, on or off (got %q)", s.name, s.val)
		}
	}
	for i, s := range c.Sink {
		if !contains(SinkTypes, s.Type) {
			return fmt.Errorf("sink[%d]: unknown type %q (known: %s)", i, s.Type, join(SinkTypes))
		}
		switch s.Type {
		case "webhook":
			if s.URL == "" {
				return fmt.Errorf("sink[%d] webhook: url is required", i)
			}
		case "command":
			if len(s.Command) == 0 {
				return fmt.Errorf("sink[%d] command: command is required", i)
			}
			if s.Shell && len(s.Command) != 1 {
				return fmt.Errorf("sink[%d] command: shell = true needs exactly one command string", i)
			}
		}
	}
	return nil
}

// Path resolves where `init` should write: explicit flag, environment,
// then the OS user config dir. It returns "" when nothing can be resolved.
func Path(flagValue string) string {
	if flagValue != "" {
		return flagValue
	}
	if env := os.Getenv("AGENT_NOTIFY_CONFIG"); env != "" {
		return env
	}
	if dir, err := os.UserConfigDir(); err == nil {
		return filepath.Join(dir, "agent-notify", "config.toml")
	}
	return ""
}

// mntC is the WSL mount point of the Windows drive (overridable in tests).
var mntC = "/mnt/c"

// Resolve picks the config to LOAD: explicit flag, environment, the OS
// default, and — on WSL — the Windows-side %APPDATA% config when it
// exists, so one file can drive both the Windows tray and a WSL daemon.
// The second return says which rule matched ("flag", "env", "default",
// "windows-shared", "").
func Resolve(flagValue string) (string, string) {
	if flagValue != "" {
		return flagValue, "flag"
	}
	if env := os.Getenv("AGENT_NOTIFY_CONFIG"); env != "" {
		return env, "env"
	}
	def := Path("")
	if def != "" && fileExists(def) {
		return def, "default"
	}
	if runtime.GOOS == "linux" {
		if p := windowsSharedConfig(); p != "" {
			return p, "windows-shared"
		}
	}
	if def != "" {
		return def, "default" // exists or not: the normal location
	}
	return "", ""
}

// windowsSharedConfig finds a Windows-side config under /mnt/c, if any.
func windowsSharedConfig() string {
	users := filepath.Join(mntC, "Users")
	entries, err := os.ReadDir(users)
	if err != nil {
		return ""
	}
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		p := filepath.Join(users, e.Name(), "AppData", "Roaming", "agent-notify", "config.toml")
		if fileExists(p) {
			return p
		}
	}
	return ""
}

func fileExists(path string) bool {
	fi, err := os.Stat(path)
	return err == nil && !fi.IsDir()
}

func validEnabled(v string) bool {
	switch v {
	case "", "auto", "on", "off":
		return true
	}
	return false
}

func contains(ss []string, v string) bool {
	for _, s := range ss {
		if s == v {
			return true
		}
	}
	return false
}

func join(ss []string) string {
	out := ""
	for i, s := range ss {
		if i > 0 {
			out += ", "
		}
		out += s
	}
	return out
}
