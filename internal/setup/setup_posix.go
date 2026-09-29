//go:build !windows

package setup

import (
	_ "embed"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

// unitTemplate is rendered with the installed binary path by
// EnsureService. Keep in sync with contrib/agent-notify.service (the
// manually-installable copy).
//
//go:embed agent-notify.service
var unitTemplate string

// desktopTemplate is rendered into ~/.config/autostart by
// EnsureTrayAutostart.
//
//go:embed agent-notify.desktop
var desktopTemplate string

func platformInstallDir() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, ".local", "bin"), nil
}

func samePath(a, b string) bool {
	return filepath.Clean(a) == filepath.Clean(b)
}

func platformAutostartDesc() string { return "XDG autostart (" + desktopPath() + ")" }

func desktopPath() string {
	dir, err := os.UserConfigDir()
	if err != nil {
		return ""
	}
	return filepath.Join(dir, "autostart", ServiceName+".desktop")
}

func platformTrayAutostartInstalled() bool {
	return desktopPath() != "" && fileExists(desktopPath())
}

func platformCreateTrayAutostart(exe string) error {
	p := desktopPath()
	if p == "" {
		return errors.New("cannot resolve the XDG config directory")
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	// The Exec key needs quoting when the path contains spaces.
	execKey := exe
	if strings.ContainsAny(execKey, " \t") {
		execKey = `"` + strings.ReplaceAll(execKey, `"`, ``) + `"`
	}
	content := strings.Replace(desktopTemplate, "__EXEC__", execKey, 1)
	return os.WriteFile(p, []byte(content), 0o644)
}

func platformRemoveTrayAutostart() error {
	p := desktopPath()
	if p == "" || !fileExists(p) {
		return nil
	}
	return os.Remove(p)
}

func systemdUnitPath() string {
	dir, err := os.UserConfigDir()
	if err != nil {
		return ""
	}
	return filepath.Join(dir, "systemd", "user", ServiceName+".service")
}

// platformSystemd reports whether a systemd user manager is reachable.
// WSL without [boot] systemd=true is the common "no" case; the hint
// carries the exact fix.
func platformSystemd() (bool, string) {
	if _, err := exec.LookPath("systemctl"); err != nil {
		return false, ""
	}
	if fi, err := os.Stat("/run/systemd/system"); err != nil || !fi.IsDir() {
		return false, "systemd is not running here (WSL: set [boot] systemd=true in /etc/wsl.conf, run `wsl --shutdown` from Windows, then retry)"
	}
	return true, ""
}

func platformServiceEnabled() bool {
	p := systemdUnitPath()
	if p == "" || !fileExists(p) {
		return false
	}
	return exec.Command("systemctl", "--user", "is-enabled", ServiceName).Run() == nil
}

func platformInstallService(exe string) error {
	ok, hint := platformSystemd()
	if !ok {
		return fmt.Errorf("cannot install the service: %s", hint)
	}
	p := systemdUnitPath()
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	content := strings.Replace(unitTemplate, "__BIN__", exe, 1)
	if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
		return err
	}
	for _, args := range [][]string{
		{"--user", "daemon-reload"},
		{"--user", "enable", "--now", ServiceName},
	} {
		if out, err := exec.Command("systemctl", args...).CombinedOutput(); err != nil {
			return fmt.Errorf("systemctl %s: %v: %s", strings.Join(args, " "), err, strings.TrimSpace(string(out)))
		}
	}
	return nil
}

func platformRemoveService() error {
	// Best-effort: a dead systemd must not block uninstalling the files.
	_ = exec.Command("systemctl", "--user", "disable", "--now", ServiceName).Run()
	_ = exec.Command("systemctl", "--user", "daemon-reload").Run()
	p := systemdUnitPath()
	if p == "" || !fileExists(p) {
		return nil
	}
	return os.Remove(p)
}

func platformRestartService() error {
	return exec.Command("systemctl", "--user", "restart", ServiceName).Run()
}

func platformEnableLinger() error {
	return exec.Command("loginctl", "enable-linger").Run()
}

func platformPathOK(dir string) bool {
	for _, p := range filepath.SplitList(os.Getenv("PATH")) {
		if samePath(p, dir) {
			return true
		}
	}
	return false
}

// platformAddToPath appends a marked export to ~/.profile — the least
// surprising user shell file — only when the directory isn't already
// exported there.
func platformAddToPath(dir string) error {
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	profile := filepath.Join(home, ".profile")
	if data, err := os.ReadFile(profile); err == nil && strings.Contains(string(data), dir) {
		return nil
	}
	f, err := os.OpenFile(profile, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = fmt.Fprintf(f, "\n# agent-notify path\nexport PATH=%q:$PATH\n", dir)
	return err
}

// platformStopInstances terminates other agent-notify processes so an
// old copy never keeps running beside the installed one (Linux doesn't
// lock the file, but a stale tray would keep toasting). Returns whether
// anything was stopped.
func platformStopInstances(selfExe string) bool {
	out, err := exec.Command("pgrep", "-x", ServiceName).Output()
	if err != nil {
		return false
	}
	stopped := false
	for _, field := range strings.Fields(string(out)) {
		pid, err := strconv.Atoi(field)
		if err != nil || pid == os.Getpid() {
			continue
		}
		if syscall.Kill(pid, syscall.SIGTERM) == nil {
			stopped = true
		}
	}
	return stopped
}

// platformRemoveInstalled deletes the installed binary. Only the binary:
// ~/.local/bin is the user's directory and may hold anything.
func platformRemoveInstalled(st Status) []string {
	if !fileExists(st.InstallExe) {
		return []string{"no installed binary found (portable mode?)"}
	}
	if err := os.Remove(st.InstallExe); err != nil {
		return []string{fmt.Sprintf("could not remove %s: %v", st.InstallExe, err)}
	}
	return []string{"removed " + st.InstallExe}
}

func platformWSLAvailable() bool { return false }

func platformInstallWSL(distro string, w io.Writer) error {
	return errors.New("the WSL daemon is installed from Windows, not from here")
}

func execCommandDetached(exe string) error {
	return exec.Command(exe).Start()
}
