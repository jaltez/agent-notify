// `agent-notify setup` — the first-run wizard. It walks a freshly
// downloaded binary through deploying itself: canonical install
// location, PATH, login autostart or the systemd service, the WSL daemon
// on Windows, config, and a test notification. `agent-notify uninstall`
// reverses all of it. The install scripts delegate here (`setup --yes`)
// so script installs and manual installs stay identical.
package cli

import (
	"bufio"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"

	"github.com/jaltez/agent-notify/internal/config"
	"github.com/jaltez/agent-notify/internal/setup"
)

func cmdSetup(configPath string, args []string, stdout, stderr io.Writer, log *slog.Logger) int {
	fs := flag.NewFlagSet("setup", flag.ContinueOnError)
	fs.SetOutput(stderr)
	statusOnly := fs.Bool("status", false, "print the installation state and exit")
	yes := fs.Bool("yes", false, "no prompts; take the defaults")
	noAutostart := fs.Bool("no-autostart", false, "skip login autostart / service registration")
	serviceMode := fs.Bool("service", false, "Linux: systemd user service instead of desktop autostart")
	withWSL := fs.Bool("with-wsl", false, "Windows: also install the daemon inside WSL")
	wslDistro := fs.String("wsl-distro", "", "WSL distro for --with-wsl (default: wsl.exe's default)")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *noAutostart && *serviceMode {
		fmt.Fprintln(stderr, "setup: --no-autostart and --service conflict")
		return 2
	}

	st := setup.Detect()
	printStatusLines(stdout, st)
	if *statusOnly {
		return 0
	}
	defer pauseIfWindowed(stdout)

	if !isTerminal() && !*yes {
		fmt.Fprintln(stdout, "setup: no terminal attached; re-run with --yes to apply the defaults")
		return 0
	}
	w := &wizard{in: bufio.NewReader(os.Stdin), out: stdout, yes: *yes}

	// 1 — install the binary into its canonical home
	stopped := false
	installedNow := false
	switch {
	case st.InstallDir == "":
		fmt.Fprintln(stdout, "setup: no canonical install location here; continuing in place")
	case st.Installed:
		fmt.Fprintf(stdout, "setup: already installed (%s)\n", st.InstallExe)
	case w.confirm("Install agent-notify to "+st.InstallDir+"?", true):
		var err error
		stopped, err = setup.Install()
		if err != nil {
			fmt.Fprintf(stdout, "setup: install failed: %v\n", err)
			return 1
		}
		fmt.Fprintf(stdout, "setup: installed %s\n", st.InstallExe)
		installedNow = true
		// From here the wizard speaks for the installed copy, not the
		// (possibly Downloads-folder) process that is running it.
		st = setup.Detect()
		st.Exe, st.Installed = st.InstallExe, true
	default:
		fmt.Fprintln(stdout, "setup: keeping the binary where it is (portable mode)")
	}

	// 2 — PATH
	pathFixed := false
	if st.Installed && !st.PathOK && w.confirm("Add "+st.InstallDir+" to your PATH?", true) {
		if err := setup.EnsurePATH(); err != nil {
			fmt.Fprintf(stdout, "setup: PATH update failed: %v\n", err)
		} else {
			pathFixed = true
			if runtime.GOOS == "windows" {
				fmt.Fprintln(stdout, "setup: added to the user PATH (new terminals pick it up)")
			} else {
				fmt.Fprintln(stdout, "setup: PATH export added to ~/.profile (new shells pick it up)")
			}
		}
	}

	// 3 — residency: start at login
	if *noAutostart {
		fmt.Fprintln(stdout, "setup: autostart skipped (--no-autostart)")
	} else if runtime.GOOS == "windows" {
		if st.Autostart {
			fmt.Fprintln(stdout, "setup: autostart already configured (Startup shortcut)")
		} else if w.confirm("Start agent-notify automatically at login?", true) {
			if err := setup.EnsureTrayAutostart(); err != nil {
				fmt.Fprintf(stdout, "setup: autostart failed: %v\n", err)
			} else {
				fmt.Fprintln(stdout, "setup: Startup shortcut created (see shell:startup)")
			}
		}
	} else {
		switch residencyMode(w, st, *serviceMode) {
		case "tray":
			if err := setup.EnsureTrayAutostart(); err != nil {
				fmt.Fprintf(stdout, "setup: autostart failed: %v\n", err)
			} else {
				fmt.Fprintln(stdout, "setup: tray will start at login (XDG autostart)")
			}
		case "service":
			if !st.Systemd {
				fmt.Fprintln(stdout, "setup: no systemd user manager here; service skipped")
				if st.ServiceHint != "" {
					fmt.Fprintf(stdout, "setup: %s\n", st.ServiceHint)
				}
			} else if err := setup.EnsureService(); err != nil {
				fmt.Fprintf(stdout, "setup: service install failed: %v\n", err)
			} else {
				fmt.Fprintf(stdout, "setup: systemd service enabled (logs: journalctl --user -u %s)\n", setup.ServiceName)
				if w.confirm("Enable lingering so the service runs without a login session?", false) {
					if err := setup.EnableLinger(); err != nil {
						fmt.Fprintf(stdout, "setup: linger failed: %v\n", err)
					} else {
						fmt.Fprintln(stdout, "setup: lingering enabled")
					}
				}
			}
		case "skip":
			fmt.Fprintln(stdout, "setup: no residency configured on this machine")
		case "done":
			fmt.Fprintln(stdout, "setup: residency already configured")
		}
	}

	// 4 — WSL daemon (Windows hosts)
	if runtime.GOOS == "windows" {
		runWSL := func() {
			fmt.Fprintln(stdout, "setup: installing the WSL daemon (this takes a moment)…")
			if err := setup.InstallWSL(*wslDistro, stdout); err != nil {
				fmt.Fprintf(stdout, "setup: WSL daemon install failed: %v\n", err)
				fmt.Fprintln(stdout, "setup: the Windows tray watches WSL sessions on its own either way")
			}
		}
		switch {
		case *withWSL:
			runWSL()
		case setup.WSLAvailable() && w.confirm("Also install the headless daemon inside WSL? (needs systemd in the distro)", false):
			runWSL()
		}
	}

	// 5 — bring the tray back up if the install replaced it
	if stopped && w.confirm("Start agent-notify now?", true) {
		if err := setup.StartInstalled(); err != nil {
			fmt.Fprintf(stdout, "setup: could not start: %v\n", err)
		} else {
			fmt.Fprintln(stdout, "setup: started")
		}
	}

	// 6 — config and a test popup
	p := config.Path(configPath)
	switch {
	case p == "":
		fmt.Fprintln(stdout, "setup: cannot determine a config path; pass --config")
	case fileExists(p):
		fmt.Fprintf(stdout, "setup: config exists (%s)\n", p)
	case w.confirm("Write an annotated config now? (zero config works too)", true):
		if err := writeExampleConfig(p); err != nil {
			fmt.Fprintf(stdout, "setup: config write failed: %v\n", err)
		} else {
			fmt.Fprintf(stdout, "setup: wrote %s\n", p)
		}
	}
	if w.confirm("Send a test notification through the sinks?", true) {
		cmdTest(configPath, stdout, log)
	}

	// 7 — summary
	fmt.Fprintln(stdout)
	st = setup.Detect()
	if installedNow {
		st.Exe, st.Installed = st.InstallExe, true
	}
	if pathFixed {
		st.PathOK = true // the live process PATH can't show it yet
	}
	printStatusLines(stdout, st)
	fmt.Fprintln(stdout, "setup: done — updates arrive automatically (daily check, `agent-notify update` to force)")
	fmt.Fprintln(stdout, "setup: everything is reversible with `agent-notify uninstall`")
	return 0
}

// residencyMode picks how a posix install stays resident: tray autostart
// on desktop sessions, the systemd service on headless boxes, and an
// explicit choice when both would work.
func residencyMode(w *wizard, st setup.Status, serviceFlag bool) string {
	if serviceFlag {
		return "service"
	}
	if st.Autostart || st.ServiceEnabled {
		return "done"
	}
	gui, sysd := guiAvailable(), st.Systemd
	switch {
	case !gui && !sysd:
		return "skip"
	case !gui:
		return "service"
	case !sysd:
		return "tray"
	case !w.yes:
		if w.choose("Keep agent-notify resident how?", []string{
			"tray at login (XDG autostart)",
			"headless systemd service",
		}, 0) == 1 {
			return "service"
		}
	}
	return "tray"
}

func cmdUninstall(configPath string, args []string, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("uninstall", flag.ContinueOnError)
	fs.SetOutput(stderr)
	yes := fs.Bool("yes", false, "do not ask for confirmation")
	purge := fs.Bool("purge-config", false, "also remove the configuration file")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	defer pauseIfWindowed(stdout)

	st := setup.Detect()
	var items []string
	if st.ServiceEnabled {
		items = append(items, "the systemd user service")
	}
	if st.Autostart {
		items = append(items, "login autostart")
	}
	if fileExists(st.InstallExe) {
		items = append(items, "the binaries in "+st.InstallDir)
	}
	if *purge && st.ConfigExists {
		items = append(items, "the config "+st.ConfigPath)
	}
	if len(items) == 0 {
		fmt.Fprintln(stdout, "uninstall: nothing installed to remove")
		return 0
	}
	fmt.Fprintln(stdout, "uninstall: this will remove:")
	for _, it := range items {
		fmt.Fprintf(stdout, "  - %s\n", it)
	}
	if !*yes {
		if !isTerminal() {
			fmt.Fprintln(stdout, "uninstall: no terminal attached; re-run with --yes")
			return 0
		}
		w := &wizard{in: bufio.NewReader(os.Stdin), out: stdout}
		if !w.confirm("Proceed?", true) {
			fmt.Fprintln(stdout, "uninstall: aborted")
			return 0
		}
	}
	notes, err := setup.Uninstall(*purge, configPath)
	for _, n := range notes {
		fmt.Fprintf(stdout, "uninstall: %s\n", n)
	}
	if err != nil {
		fmt.Fprintf(stdout, "uninstall: error: %v\n", err)
		return 1
	}
	if setup.WSLAvailable() {
		fmt.Fprintln(stdout, "uninstall: a WSL-side daemon (if any) is untouched; inside the distro run:")
		fmt.Fprintf(stdout, "  systemctl --user disable --now %s && rm -f ~/.local/bin/%s\n", setup.ServiceName, setup.ServiceName)
	}
	fmt.Fprintln(stdout, "uninstall: done")
	return 0
}

// wizard prompts on a terminal. With yes set it answers every question
// with the default, so the same flow drives the install scripts.
type wizard struct {
	in  *bufio.Reader
	out io.Writer
	yes bool
}

func (w *wizard) confirm(q string, def bool) bool {
	if w.yes {
		return def
	}
	for {
		hint := "[Y/n]"
		if !def {
			hint = "[y/N]"
		}
		fmt.Fprintf(w.out, "%s %s ", q, hint)
		line, err := w.in.ReadString('\n')
		if err != nil { // EOF (piped stdin): fall back to the default
			fmt.Fprintln(w.out)
			return def
		}
		switch strings.ToLower(strings.TrimSpace(line)) {
		case "":
			return def
		case "y", "yes":
			return true
		case "n", "no":
			return false
		}
		fmt.Fprintln(w.out, "please answer y or n")
	}
}

func (w *wizard) choose(q string, options []string, def int) int {
	if w.yes {
		return def
	}
	for i, o := range options {
		fmt.Fprintf(w.out, "  %d) %s\n", i+1, o)
	}
	for {
		fmt.Fprintf(w.out, "%s [1-%d, default %d] ", q, len(options), def+1)
		line, err := w.in.ReadString('\n')
		if err != nil {
			fmt.Fprintln(w.out)
			return def
		}
		if line = strings.TrimSpace(line); line == "" {
			return def
		}
		if n, err := strconv.Atoi(line); err == nil && n >= 1 && n <= len(options) {
			return n - 1
		}
		fmt.Fprintf(w.out, "enter a number 1-%d\n", len(options))
	}
}

func printStatusLines(w io.Writer, st setup.Status) {
	rows := st.Lines()
	width := 0
	for _, r := range rows {
		if len(r[0]) > width {
			width = len(r[0])
		}
	}
	for _, r := range rows {
		fmt.Fprintf(w, "%-*s  %s\n", width, r[0], r[1])
	}
}

// pauseIfWindowed keeps the console open when the wizard was spawned in
// its own window by the tray's "Set up…" item, so its output isn't lost
// the instant it finishes.
func pauseIfWindowed(stdout io.Writer) {
	if os.Getenv("AGENT_NOTIFY_SETUP_WINDOW") == "" || !isTerminal() {
		return
	}
	fmt.Fprint(stdout, "\nPress Enter to close this window…")
	_, _ = bufio.NewReader(os.Stdin).ReadString('\n')
}

// writeExampleConfig creates the annotated config (shared by `init` and
// the wizard's config step).
func writeExampleConfig(path string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, config.Example, 0o644)
}
