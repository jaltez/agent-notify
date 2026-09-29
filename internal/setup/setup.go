// Package setup implements agent-notify's self-installation: detecting
// how the binary is currently deployed and moving it to the canonical
// per-user location, with OS-appropriate autostart — a Startup shortcut
// on Windows, XDG autostart on Linux desktops, a systemd user service for
// headless runs. Every action is idempotent; `agent-notify setup` is
// always safe to re-run.
package setup

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"time"

	"github.com/jaltez/agent-notify/internal/buildinfo"
	"github.com/jaltez/agent-notify/internal/config"
)

// ServiceName is the systemd user unit name on Linux/WSL.
const ServiceName = "agent-notify"

// Status is a point-in-time snapshot of the deployment, as shown by
// `agent-notify setup --status` and the wizard's banner.
type Status struct {
	Version        string
	Exe            string // running binary, symlinks resolved
	InstallDir     string // canonical per-user install directory
	InstallExe     string // canonical binary path ("" when unsupported)
	Installed      bool   // running from the canonical install directory
	PathOK         bool   // InstallDir is on PATH
	Autostart      bool   // tray/login autostart registered
	ServiceEnabled bool   // systemd user unit installed and enabled
	Systemd        bool   // systemd user manager usable here
	ServiceHint    string // why systemd is unusable, when it is not
	ConfigPath     string
	ConfigExists   bool
}

// Detect gathers the current state. It is cheap — filesystem and
// environment only, no network — so the tray can call it at startup.
func Detect() Status {
	st := Status{Version: buildinfo.Version, Exe: selfExe()}
	if dir, err := platformInstallDir(); err == nil && dir != "" {
		st.InstallDir = dir
		st.InstallExe = filepath.Join(dir, installExeName())
		st.Installed = samePath(filepath.Dir(st.Exe), dir)
		st.PathOK = platformPathOK(dir)
	}
	st.Autostart = platformTrayAutostartInstalled()
	st.Systemd, st.ServiceHint = platformSystemd()
	st.ServiceEnabled = st.Systemd && platformServiceEnabled()
	if p := config.Path(""); p != "" {
		st.ConfigPath = p
		st.ConfigExists = fileExists(p)
	}
	return st
}

// NeedsSetup reports whether the first-run nudge should fire: a config
// file means deliberate use however the binary is deployed (portable
// mode is a choice); otherwise a fresh download or an install missing
// both autostart and service looks unfinished.
func (s Status) NeedsSetup() bool {
	if s.ConfigExists {
		return false
	}
	if !s.Installed {
		return true
	}
	return !s.Autostart && !s.ServiceEnabled
}

// Lines renders the status as aligned key/value rows for the wizard and
// `setup --status`.
func (s Status) Lines() [][2]string {
	yn := func(b bool, yes string) string {
		if b {
			if yes == "" {
				return "yes"
			}
			return yes
		}
		return "no"
	}
	rows := [][2]string{
		{"version", s.Version},
		{"binary", s.Exe},
	}
	if s.InstallDir == "" {
		rows = append(rows, [2]string{"install", "unsupported on this platform"})
		return rows
	}
	state := "installed"
	if !s.Installed {
		state = "not installed"
	}
	rows = append(rows, [2]string{"install", s.InstallDir + " (" + state + ")"})
	if s.PathOK {
		rows = append(rows, [2]string{"PATH", "ok"})
	} else {
		rows = append(rows, [2]string{"PATH", s.InstallDir + " missing"})
	}
	if s.Autostart {
		rows = append(rows, [2]string{"autostart", platformAutostartDesc()})
	} else {
		rows = append(rows, [2]string{"autostart", "none"})
	}
	if s.ServiceEnabled {
		rows = append(rows, [2]string{"service", "enabled (systemd user unit)"})
	} else if s.Systemd {
		rows = append(rows, [2]string{"service", yn(false, "")})
	} else {
		rows = append(rows, [2]string{"service", "n/a (no systemd)"})
	}
	cfg := s.ConfigPath + " — missing"
	if s.ConfigExists {
		cfg = s.ConfigPath
	}
	rows = append(rows, [2]string{"config", cfg})
	return rows
}

// Install copies the running binary (and, on Windows, its console twin
// when shipped alongside) into the canonical install directory. A tray or
// daemon already running from the target is stopped first — Windows locks
// a running exe — and restarted by the caller if it wants; the return
// says whether anything was stopped.
func Install() (stopped bool, err error) {
	exe := selfExe()
	if exe == "" {
		return false, errors.New("cannot locate the running binary")
	}
	dir, err := platformInstallDir()
	if err != nil || dir == "" {
		return false, errors.New("no canonical install location on this platform")
	}
	stopped = platformStopInstances(exe)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return stopped, err
	}
	// Canonical name, even when the download was renamed — autostart,
	// the service unit and the upgrader all expect agent-notify[.exe].
	dstName := installExeName()
	if runtime.GOOS == "windows" && filepath.Base(exe) == consoleExeName() {
		dstName = consoleExeName()
	}
	if err := copyFile(exe, filepath.Join(dir, dstName)); err != nil {
		return stopped, err
	}
	for _, sibling := range siblingExes(exe) {
		if err := copyFile(sibling, filepath.Join(dir, filepath.Base(sibling))); err != nil {
			return stopped, err
		}
	}
	// A running service would keep serving from the old binary image.
	if platformServiceEnabled() {
		_ = platformRestartService()
	}
	return stopped, nil
}

// EnsureTrayAutostart registers login autostart for the tray: a Startup
// shortcut on Windows, an XDG autostart entry on Linux desktops.
func EnsureTrayAutostart() error {
	return platformCreateTrayAutostart(autostartTarget())
}

// EnsurePATH puts the install directory on the user PATH.
func EnsurePATH() error {
	st := Detect()
	if st.InstallDir == "" {
		return errors.New("no install directory on this platform")
	}
	return platformAddToPath(st.InstallDir)
}

// EnsureService installs the headless daemon as a systemd user service.
func EnsureService() error {
	return platformInstallService(autostartTarget())
}

// EnableLinger lets the systemd user service run without an open login
// session (servers, WSL). Offered, never forced: it needs the user's own
// polkit grant on some systems.
func EnableLinger() error { return platformEnableLinger() }

// Uninstall reverses setup: stops and removes the service and autostart,
// deletes the installed binaries, and — with purgeConfig — the config
// file. It returns human-readable notes about what was done and what was
// deliberately left behind.
func Uninstall(purgeConfig bool, configFlag string) ([]string, error) {
	var notes []string
	st := Detect()
	if st.ServiceEnabled {
		if err := platformRemoveService(); err != nil {
			return notes, err
		}
		notes = append(notes, "disabled and removed the systemd user service")
	}
	if st.Autostart {
		if err := platformRemoveTrayAutostart(); err != nil {
			return notes, err
		}
		notes = append(notes, "removed login autostart")
	}
	platformStopInstances(selfExe())
	if st.InstallDir != "" {
		notes = append(notes, platformRemoveInstalled(st)...)
	}
	if purgeConfig {
		if p := config.Path(configFlag); p != "" && fileExists(p) {
			if err := os.Remove(p); err != nil {
				return notes, err
			}
			notes = append(notes, "removed "+p)
		}
		if p, err := statePath(); err == nil {
			_ = os.Remove(p)
		}
	}
	notes = append(notes, "PATH entries were left in place; remove "+st.InstallDir+" from PATH manually if desired")
	return notes, nil
}

// WSLAvailable reports whether wsl.exe exists (Windows hosts only).
func WSLAvailable() bool { return platformWSLAvailable() }

// InstallWSL installs the headless daemon inside a WSL distro by piping
// the repository's install.sh through it with --service, streaming its
// output into w. Empty distro means wsl.exe's default.
func InstallWSL(distro string, w interface{ Write([]byte) (int, error) }) error {
	return platformInstallWSL(distro, w)
}

// StartInstalled launches the installed binary detached (used to bring
// the tray back after an in-place upgrade).
func StartInstalled() error {
	st := Detect()
	if !fileExists(st.InstallExe) {
		return errors.New("no installed binary to start")
	}
	return execCommandDetached(st.InstallExe)
}

// autostartTarget picks the binary autostart should launch: the canonical
// install when present, else the running binary (portable mode).
func autostartTarget() string {
	st := Detect()
	if fileExists(st.InstallExe) {
		return st.InstallExe
	}
	return st.Exe
}

func installExeName() string {
	if runtime.GOOS == "windows" {
		return ServiceName + ".exe"
	}
	return ServiceName
}

// consoleExeName is the console-subsystem twin shipped in the Windows
// zip (it hosts the setup wizard's window).
func consoleExeName() string {
	if runtime.GOOS == "windows" {
		return ServiceName + "-console.exe"
	}
	return ""
}

// siblingExes lists shipped companions to copy alongside the binary —
// the console twin on Windows (the tray menu's wizard window).
func siblingExes(exe string) []string {
	if runtime.GOOS != "windows" {
		return nil
	}
	twin := consoleExeName()
	if filepath.Base(exe) == twin {
		twin = installExeName()
	}
	p := filepath.Join(filepath.Dir(exe), twin)
	if fileExists(p) {
		return []string{p}
	}
	return nil
}

// copyFile places src at dst atomically: write beside the target, then
// rename over it. Rename-aside keeps a locked (running) destination on
// Windows replaceable.
func copyFile(src, dst string) error {
	data, err := os.ReadFile(src)
	if err != nil {
		return err
	}
	if err := os.WriteFile(dst+".tmp", data, 0o755); err != nil {
		return err
	}
	if err := os.Rename(dst+".tmp", dst); err != nil {
		if runtime.GOOS != "windows" {
			_ = os.Remove(dst + ".tmp")
			return err
		}
		// A running exe cannot be replaced in place, but it can be moved
		// out of the way first.
		_ = os.Rename(dst, dst+".old")
		if err2 := os.Rename(dst+".tmp", dst); err2 != nil {
			return err2
		}
	}
	return nil
}

func selfExe() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		return resolved
	}
	return exe
}

// nudgeState persists that the first-run toast already fired, so the
// tray nags exactly once per machine.
type nudgeState struct {
	NudgedAt time.Time `json:"nudged_at"`
}

func statePath() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, ServiceName, "setup-state.json"), nil
}

// NudgeDue reports whether the one-time setup toast should fire.
func NudgeDue() bool {
	p, err := statePath()
	if err != nil || fileExists(p) {
		return false
	}
	return Detect().NeedsSetup()
}

// MarkNudged records that the toast fired.
func MarkNudged() error {
	p, err := statePath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	data, err := json.Marshal(nudgeState{NudgedAt: time.Now()})
	if err != nil {
		return err
	}
	return os.WriteFile(p, data, 0o644)
}

func fileExists(path string) bool {
	fi, err := os.Stat(path)
	return err == nil && !fi.IsDir()
}
