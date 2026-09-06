package herdr

import (
	"context"
	"fmt"
	"os/exec"
	"runtime"
	"time"

	"agent-notify/internal/proc"
)

// Windows polls herdr.exe sessions: natively on Windows, or the Windows
// side from WSL via interop (herdr.exe is reachable from PATH there).
type Windows struct {
	binary   string
	sessions []string
	timeout  time.Duration
	dir      string
}

// NewWindows resolves the herdr.exe binary. binarySpec may be "auto"
// (default), a name looked up in PATH, or an explicit path. The extra
// sessions are polled in addition to the default one.
func NewWindows(binarySpec string, sessions []string, timeout time.Duration) (*Windows, error) {
	w := &Windows{sessions: sessions, timeout: timeout, dir: proc.InteropDir()}
	candidates := []string{"herdr.exe"}
	if runtime.GOOS == "windows" {
		candidates = []string{"herdr", "herdr.exe"}
	}
	if binarySpec != "" && binarySpec != "auto" {
		candidates = []string{binarySpec}
	}
	for _, c := range candidates {
		if p, err := exec.LookPath(c); err == nil {
			w.binary = p
			return w, nil
		}
	}
	return nil, fmt.Errorf("herdr.exe not found in PATH (tried %v)", candidates)
}

// Available reports whether the backend can run (binary resolved).
func (w *Windows) Available() bool { return w != nil }

func (w *Windows) Name() string          { return "windows" }
func (w *Windows) Rescan() time.Duration { return 0 } // session list is static config

func (w *Windows) Discover(ctx context.Context) ([]Session, error) {
	names := []string{"default"}
	for _, s := range w.sessions {
		if s == "" || s == "default" {
			continue
		}
		names = append(names, s)
	}
	out := make([]Session, len(names))
	for i, n := range names {
		out[i] = Session{Host: "windows", Name: n, Ref: n}
	}
	return out, nil
}

func (w *Windows) Fetch(ctx context.Context, s Session) (*Snapshot, error) {
	argv := []string{w.binary}
	if s.Name != "default" {
		argv = append(argv, "--session", s.Name)
	}
	argv = append(argv, "api", "snapshot")
	fctx, cancel := context.WithTimeout(ctx, w.timeout)
	defer cancel()
	out, err := proc.Run(fctx, argv, nil, w.dir)
	if err != nil {
		return nil, err
	}
	return ParseSnapshot(out)
}
