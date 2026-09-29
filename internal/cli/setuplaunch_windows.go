//go:build windows

package cli

import (
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
)

// launchSetupWizard opens the setup wizard in a fresh console window via
// the console twin — the tray exe is windowsgui and has no window to
// host an interactive prompt. cmd's `start` returns immediately; the
// marker env makes the wizard hold its window open when it finishes.
func launchSetupWizard() error {
	self, err := os.Executable()
	if err != nil {
		return err
	}
	target := filepath.Join(filepath.Dir(self), "agent-notify-console.exe")
	if _, err := os.Stat(target); err != nil {
		target = self // go install builds ship no twin: attach to the caller's console
	}
	// The empty "" is start's window-title argument; without it a quoted
	// target path would be taken as the title.
	cmd := exec.Command("cmd", "/C", "start", "", target, "setup")
	cmd.Env = append(os.Environ(), "AGENT_NOTIFY_SETUP_WINDOW=1")
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: false}
	return cmd.Start()
}
