//go:build windows

package setup

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"unsafe"

	"golang.org/x/sys/windows"
	"golang.org/x/sys/windows/registry"

	"github.com/jaltez/agent-notify/internal/buildinfo"
)

func platformInstallDir() (string, error) {
	base := os.Getenv("LOCALAPPDATA")
	if base == "" {
		return "", errors.New("%LOCALAPPDATA% is not set")
	}
	return filepath.Join(base, "Programs", ServiceName), nil
}

// samePath is case-insensitive: Windows paths are.
func samePath(a, b string) bool {
	return strings.EqualFold(filepath.Clean(a), filepath.Clean(b))
}

func platformAutostartDesc() string { return "Startup shortcut (" + startupShortcutPath() + ")" }

func startupShortcutPath() string {
	appData := os.Getenv("APPDATA")
	if appData == "" {
		return ""
	}
	return filepath.Join(appData, "Microsoft", "Windows", "Start Menu", "Programs", "Startup", ServiceName+".lnk")
}

func platformTrayAutostartInstalled() bool {
	return startupShortcutPath() != "" && fileExists(startupShortcutPath())
}

// platformCreateTrayAutostart writes the Startup shortcut. A .lnk is a
// COM-serialized object, not plain bytes, so it is built by a generated
// PowerShell script (same WScript.Shell flow install.ps1 used).
func platformCreateTrayAutostart(exe string) error {
	lnk := startupShortcutPath()
	if lnk == "" {
		return errors.New("%APPDATA% is not set")
	}
	script := fmt.Sprintf(`$s = (New-Object -ComObject WScript.Shell).CreateShortcut('%s')
$s.TargetPath = '%s'
$s.WorkingDirectory = '%s'
$s.Save()
`, psQuote(lnk), psQuote(exe), psQuote(filepath.Dir(exe)))
	_, err := runPowerShell(script)
	return err
}

func platformRemoveTrayAutostart() error {
	p := startupShortcutPath()
	if p == "" || !fileExists(p) {
		return nil
	}
	return os.Remove(p)
}

// The systemd integration is Linux/WSL-only.
func platformSystemd() (bool, string) { return false, "" }

func platformServiceEnabled() bool { return false }

func platformInstallService(exe string) error {
	return errors.New("the systemd service is Linux/WSL only; on Windows use autostart")
}

func platformRemoveService() error { return nil }

func platformRestartService() error { return nil }

func platformEnableLinger() error { return errors.New("linger is a systemd (Linux) concept") }

func platformPathOK(dir string) bool {
	cur, err := userPathValue()
	if err != nil {
		return false
	}
	for _, p := range strings.Split(cur, ";") {
		if samePath(strings.TrimSpace(os.ExpandEnv(p)), dir) {
			return true
		}
	}
	return false
}

func userPathValue() (string, error) {
	k, err := registry.OpenKey(registry.CURRENT_USER, `Environment`, registry.QUERY_VALUE)
	if err != nil {
		return "", err
	}
	defer k.Close()
	v, _, err := k.GetStringValue("Path")
	return v, err
}

// platformAddToPath appends the install dir to the user PATH and
// broadcasts the change so new terminals (children of Explorer) see it
// without re-logging.
func platformAddToPath(dir string) error {
	k, err := registry.OpenKey(registry.CURRENT_USER, `Environment`, registry.SET_VALUE|registry.QUERY_VALUE)
	if err != nil {
		return err
	}
	defer k.Close()
	cur, _, err := k.GetStringValue("Path")
	if err != nil && !errors.Is(err, registry.ErrNotExist) {
		return err
	}
	if cur != "" && platformPathOK(dir) {
		return nil
	}
	nv := strings.TrimSuffix(strings.TrimSpace(cur), ";")
	if nv == "" {
		nv = dir
	} else {
		nv += ";" + dir
	}
	// REG_EXPAND_SZ keeps any %VAR% references in the existing value
	// expanding; plain paths are unaffected.
	if err := k.SetExpandStringValue("Path", nv); err != nil {
		return err
	}
	broadcastEnvChange()
	return nil
}

// broadcastEnvChange notifies the shell about the PATH update — the
// SetEnvironmentVariable equivalent of .NET, which does this too.
func broadcastEnvChange() {
	const (
		hwndBroadcast   = 0xffff
		wmSettingChange = 0x001a
		smtoAbortIfHung = 0x0002
	)
	env, err := windows.UTF16PtrFromString("Environment")
	if err != nil {
		return
	}
	user32 := windows.NewLazySystemDLL("user32.dll")
	user32.NewProc("SendMessageTimeoutW").Call(
		hwndBroadcast, wmSettingChange, 0,
		uintptr(unsafe.Pointer(env)), smtoAbortIfHung, 5000, 0,
	)
}

// platformStopInstances stops agent-notify processes that would lock the
// files being replaced — a tray running from the install dir — sparing
// the wizard itself. Returns whether anything was stopped.
func platformStopInstances(selfExe string) bool {
	script := fmt.Sprintf(`$p = Get-Process '%s','%s' -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -ne '%s' }
if ($p) { $p | Stop-Process -Force; Write-Output 1 } else { Write-Output 0 }
`, ServiceName, consoleExeName(), psQuote(selfExe))
	out, err := runPowerShell(script)
	return err == nil && strings.TrimSpace(out) == "1"
}

func platformWSLAvailable() bool {
	_, err := exec.LookPath("wsl.exe")
	return err == nil
}

// platformInstallWSL installs the headless daemon inside a WSL distro by
// piping the repository's install.sh through it with --service — which
// in turn runs `agent-notify setup --yes --service` inside the distro.
func platformInstallWSL(distro string, w io.Writer) error {
	// Point at the tree the running binary came from; dev builds use main.
	ref := "main"
	if buildinfo.IsRelease() {
		ref = buildinfo.Version
	}
	base := fmt.Sprintf("https://raw.githubusercontent.com/%s/%s/%s", buildinfo.Owner, buildinfo.Repo, ref)
	inner := fmt.Sprintf("curl -fsSL %s/scripts/install.sh | sh -s -- --service || wget -qO- %s/scripts/install.sh | sh -s -- --service", base, base)
	args := []string{}
	if distro != "" {
		args = append(args, "-d", distro)
	}
	args = append(args, "-e", "sh", "-c", inner)
	cmd := exec.Command("wsl.exe", args...)
	cmd.Stdout, cmd.Stderr = w, w
	return cmd.Run()
}

// platformRemoveInstalled deletes the installed exes. A running exe
// cannot be deleted but can be renamed aside; if even that fails, its
// deletion is scheduled for after the wizard exits.
func platformRemoveInstalled(st Status) []string {
	var notes []string
	removed := false
	for _, name := range []string{installExeName(), consoleExeName()} {
		p := filepath.Join(st.InstallDir, name)
		if !fileExists(p) {
			continue
		}
		if err := os.Remove(p); err != nil {
			moved := filepath.Join(os.TempDir(), "agent-notify-removed-"+name)
			if rerr := os.Rename(p, moved); rerr != nil {
				delayedDelete(p)
				notes = append(notes, fmt.Sprintf("%s is still running; it will be deleted when this window closes", p))
				removed = true
				continue
			}
			_ = os.Remove(moved)
		}
		removed = true
	}
	if removed {
		notes = append(notes, "removed the installed binaries from "+st.InstallDir)
	} else {
		notes = append(notes, "no installed binaries found (portable mode?)")
	}
	// Succeeds only when we were the dir's sole tenant — by design.
	_ = os.Remove(st.InstallDir)
	return notes
}

// delayedDelete removes path once this process has exited; ping is the
// always-available sub-second sleep.
func delayedDelete(path string) {
	script := fmt.Sprintf("ping -n 4 127.0.0.1 >nul & del /q \"%s\"", path)
	_ = exec.Command("cmd", "/C", script).Start()
}

// runPowerShell executes script through powershell.exe and returns its
// combined output. The script travels via a temp file: inline -Command
// quoting of arbitrary paths is a losing game.
func runPowerShell(script string) (string, error) {
	tmp, err := os.CreateTemp("", "agent-notify-*.ps1")
	if err != nil {
		return "", err
	}
	defer os.Remove(tmp.Name())
	if _, err := tmp.WriteString(script); err != nil {
		tmp.Close()
		return "", err
	}
	if err := tmp.Close(); err != nil {
		return "", err
	}
	out, err := exec.Command("powershell.exe", "-NoProfile", "-NonInteractive",
		"-ExecutionPolicy", "Bypass", "-File", tmp.Name()).CombinedOutput()
	if err != nil {
		return string(out), fmt.Errorf("powershell: %v: %s", err, strings.TrimSpace(string(out)))
	}
	return string(out), nil
}

// psQuote makes a PowerShell single-quoted literal.
func psQuote(s string) string {
	return strings.ReplaceAll(s, "'", "''")
}

func execCommandDetached(exe string) error {
	return exec.Command(exe).Start()
}
