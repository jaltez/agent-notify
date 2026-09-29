//go:build !windows

package cli

import (
	"fmt"
	"os"
	"os/exec"
)

// termLaunchers lists terminal emulators and how each runs a command.
var termLaunchers = []struct {
	bin string
	pre []string
}{
	{"x-terminal-emulator", []string{"-e"}},
	{"gnome-terminal", []string{"--"}},
	{"konsole", []string{"-e"}},
	{"xfce4-terminal", []string{"-x"}}, // -e is deprecated in newer releases
	{"kitty", nil},
	{"alacritty", []string{"-e"}},
	{"wezterm", []string{"start", "--"}},
	{"xterm", []string{"-e"}},
}

// launchSetupWizard opens a terminal running `setup`. Best effort —
// there is no single API for spawning terminals on Linux desktops; when
// nothing is found the caller falls back to a toast pointing at the
// command.
func launchSetupWizard() error {
	self, err := os.Executable()
	if err != nil {
		return err
	}
	candidates := termLaunchers
	if t := os.Getenv("TERMINAL"); t != "" {
		candidates = append([]struct {
			bin string
			pre []string
		}{{t, nil}}, candidates...)
	}
	for _, c := range candidates {
		path, err := exec.LookPath(c.bin)
		if err != nil {
			continue
		}
		args := append(append([]string{}, c.pre...), self, "setup")
		if err := exec.Command(path, args...).Start(); err != nil {
			continue
		}
		return nil
	}
	return fmt.Errorf("no terminal emulator found (set $TERMINAL); run `%s setup` in a terminal", self)
}
