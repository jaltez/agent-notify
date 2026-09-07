package herdr

import (
	"context"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/jaltez/agent-notify/internal/proc"
)

// defaultWSLPath is the base PATH used for herdr calls inside the distro.
// wsl.exe -e runs without a login shell, so ~/.local/bin and ~/.cargo/bin
// (the usual herdr install locations) must be re-added explicitly.
var defaultWSLPath = []string{
	"/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin",
}

// discoveryScript lists herdr sockets with absolute paths, one per line.
const discoveryScript = `for s in "$HOME"/.config/herdr/herdr.sock "$HOME"/.config/herdr/sessions/*/herdr.sock; do [ -S "$s" ] && printf '%s\n' "$s"; done`

// WSL polls sessions inside a WSL distro from a Windows-native process,
// through wsl.exe interop.
type WSL struct {
	base      []string // wsl.exe [-d distro]
	only      []string // restrict discovery to these session names
	extraPath []string
	timeout   time.Duration
	rescan    time.Duration
}

// NewWSL builds the backend. distro empty means the default distro;
// sessions empty means auto-discover everything.
func NewWSL(distro string, sessions, extraPath []string, timeout, rescan time.Duration) *WSL {
	base := []string{"wsl.exe"}
	if distro != "" {
		base = append(base, "-d", distro)
	}
	return &WSL{
		base:      base,
		only:      sessions,
		extraPath: extraPath,
		timeout:   timeout,
		rescan:    rescan,
	}
}

func (w *WSL) Name() string          { return "wsl" }
func (w *WSL) Rescan() time.Duration { return w.rescan }

func (w *WSL) Discover(ctx context.Context) ([]Session, error) {
	argv := append(append([]string{}, w.base...), "-e", "sh", "-c", discoveryScript)
	fctx, cancel := context.WithTimeout(ctx, w.timeout)
	defer cancel()
	out, err := proc.Run(fctx, argv, nil, "")
	if err != nil {
		return nil, err
	}
	var sessions []Session
	for _, line := range strings.Split(string(proc.DecodeUTF16(out)), "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		name, home := sessionFromSocket(line)
		if name == "" {
			continue
		}
		if len(w.only) > 0 && !contains(w.only, name) {
			continue
		}
		sessions = append(sessions, Session{Host: "wsl", Name: name, Ref: line, Home: home})
	}
	sort.Slice(sessions, func(i, j int) bool { return sessions[i].Name < sessions[j].Name })
	return sessions, nil
}

// sessionFromSocket maps a socket path to its session name and the distro
// $HOME it lives under: <home>/.config/herdr/herdr.sock is the default
// session, <home>/.config/herdr/sessions/<name>/herdr.sock a named one.
func sessionFromSocket(sock string) (name, home string) {
	const marker = "/.config/herdr/"
	if i := strings.Index(sock, marker); i > 0 {
		home = sock[:i]
	}
	if filepath.Base(sock) != "herdr.sock" {
		return "", home
	}
	parent := filepath.Dir(sock)
	switch {
	case filepath.Base(filepath.Dir(parent)) == "sessions":
		// <home>/.config/herdr/sessions/<name>/herdr.sock
		return filepath.Base(parent), home
	case filepath.Base(parent) == "herdr":
		// <home>/.config/herdr/herdr.sock
		return "default", home
	default:
		return "", home
	}
}

func (w *WSL) Fetch(ctx context.Context, s Session) (*Snapshot, error) {
	argv := append(append([]string{}, w.base...), "-e")
	if s.Home != "" {
		path := strings.Join(defaultWSLPath, ":")
		for _, p := range w.extraPath {
			path += ":" + p
		}
		argv = append(argv,
			"env",
			"PATH="+s.Home+"/.local/bin:"+s.Home+"/.cargo/bin:"+path,
			"HERDR_SOCKET_PATH="+s.Ref,
			"herdr", "api", "snapshot",
		)
	} else {
		argv = append(argv, "herdr", "api", "snapshot")
	}
	fctx, cancel := context.WithTimeout(ctx, w.timeout)
	defer cancel()
	out, err := proc.Run(fctx, argv, nil, "")
	if err != nil {
		return nil, err
	}
	return ParseSnapshot(out)
}

func contains(ss []string, v string) bool {
	for _, s := range ss {
		if s == v {
			return true
		}
	}
	return false
}
