//go:build !windows

package cli

import (
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

// acquireTrayLock prevents multiple tray instances via a non-blocking
// flock on a per-user lock file.
func acquireTrayLock() (func(), error) {
	path := filepath.Join(os.TempDir(), fmt.Sprintf("agent-notify-tray-%d.lock", os.Getuid()))
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		return nil, fmt.Errorf("another agent-notify tray is already running")
	}
	return func() { _ = f.Close() }, nil
}
