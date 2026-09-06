package herdr

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"time"

	"agent-notify/internal/proc"
)

// Local polls herdr sessions through their native Unix sockets:
// <dir>/herdr.sock (session "default") and <dir>/sessions/<name>/herdr.sock.
type Local struct {
	dir     string
	binary  string
	timeout time.Duration

	once   sync.Once
	binErr error
}

// NewLocal builds the backend; socketDir and binary fall back to defaults.
func NewLocal(socketDir, binary string, timeout time.Duration) *Local {
	if socketDir == "" {
		if home, err := os.UserHomeDir(); err == nil {
			socketDir = filepath.Join(home, ".config", "herdr")
		}
	}
	if binary == "" {
		binary = "herdr"
	}
	return &Local{dir: socketDir, binary: binary, timeout: timeout}
}

// Available reports whether the socket directory and binary exist, so the
// engine can skip the backend under "auto".
func (l *Local) Available() bool {
	if l.dir == "" {
		return false
	}
	if fi, err := os.Stat(l.dir); err != nil || !fi.IsDir() {
		return false
	}
	return l.lookupBinary() == nil
}

func (l *Local) lookupBinary() error {
	l.once.Do(func() {
		_, l.binErr = exec.LookPath(l.binary)
	})
	return l.binErr
}

func (l *Local) Name() string          { return "local" }
func (l *Local) Rescan() time.Duration { return 0 } // discovery is a cheap glob

func (l *Local) Discover(ctx context.Context) ([]Session, error) {
	if l.dir == "" {
		return nil, fmt.Errorf("no herdr socket directory (home unavailable)")
	}
	if err := l.lookupBinary(); err != nil {
		return nil, fmt.Errorf("herdr binary %q not found in PATH", l.binary)
	}
	var out []Session
	root := filepath.Join(l.dir, "herdr.sock")
	if isSocket(root) {
		out = append(out, Session{Host: "local", Name: "default", Ref: root})
	}
	sdir := filepath.Join(l.dir, "sessions")
	entries, err := os.ReadDir(sdir)
	if err == nil {
		for _, ent := range entries {
			if !ent.IsDir() {
				continue
			}
			sock := filepath.Join(sdir, ent.Name(), "herdr.sock")
			if isSocket(sock) {
				out = append(out, Session{Host: "local", Name: ent.Name(), Ref: sock})
			}
		}
	}
	return out, nil
}

func (l *Local) Fetch(ctx context.Context, s Session) (*Snapshot, error) {
	ref := s.Ref
	if ref == "" {
		ref = filepath.Join(l.dir, "herdr.sock") // the "default" session
	}
	fctx, cancel := context.WithTimeout(ctx, l.timeout)
	defer cancel()
	out, err := proc.Run(fctx,
		[]string{l.binary, "api", "snapshot"},
		[]string{"HERDR_SOCKET_PATH=" + ref},
		"")
	if err != nil {
		return nil, err
	}
	return ParseSnapshot(out)
}

func isSocket(path string) bool {
	fi, err := os.Stat(path)
	return err == nil && fi.Mode()&os.ModeSocket != 0
}
