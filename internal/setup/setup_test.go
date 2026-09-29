//go:build !windows

package setup

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// sandbox points HOME/XDG at a temp tree so Detect and the file-writing
// actions never touch the real user environment. Tests must not call
// Install/Uninstall: those stop live agent-notify processes, and a test
// run on a machine with the tray up would kill it.
func sandbox(t *testing.T) string {
	t.Helper()
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	return home
}

func TestDetectSandbox(t *testing.T) {
	home := sandbox(t)
	st := Detect()
	if want := filepath.Join(home, ".local", "bin"); st.InstallDir != want {
		t.Errorf("InstallDir = %q, want %q", st.InstallDir, want)
	}
	if st.Installed {
		t.Error("nothing installed in the sandbox, Installed should be false")
	}
	if st.PathOK {
		t.Error("sandbox dir is not on PATH, PathOK should be false")
	}
	if st.Autostart {
		t.Error("no autostart file exists in the sandbox")
	}
	if st.ServiceEnabled {
		t.Error("no unit file exists in the sandbox")
	}
	if st.ConfigExists {
		t.Error("no config exists in the sandbox")
	}
	if !st.NeedsSetup() {
		t.Error("a bare sandbox should need setup")
	}
	if st.ConfigPath == "" {
		t.Error("a config path should still resolve")
	}
}

func TestNeedsSetupMatrix(t *testing.T) {
	cases := []struct {
		name string
		st   Status
		want bool
	}{
		{"bare download", Status{}, true},
		{"installed only", Status{Installed: true}, true},
		{"installed with autostart", Status{Installed: true, Autostart: true}, false},
		{"installed with service", Status{Installed: true, ServiceEnabled: true}, false},
		{"installed with config", Status{Installed: true, ConfigExists: true}, false},
		{"portable with config", Status{ConfigExists: true}, false},
	}
	for _, c := range cases {
		if got := c.st.NeedsSetup(); got != c.want {
			t.Errorf("%s: NeedsSetup() = %v, want %v", c.name, got, c.want)
		}
	}
}

func TestXDGAutostartRoundTrip(t *testing.T) {
	sandbox(t)
	exe := "/home/u/.local/bin/agent-notify"
	if err := platformCreateTrayAutostart(exe); err != nil {
		t.Fatalf("create: %v", err)
	}
	p := desktopPath()
	data, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("autostart file missing: %v", err)
	}
	if want := "Exec=" + exe; !strings.Contains(string(data), want) {
		t.Errorf("desktop entry lacks %q:\n%s", want, data)
	}
	if !Detect().Autostart {
		t.Error("Detect should see the autostart entry")
	}
	// Idempotent: a second run must succeed and keep one file.
	if err := platformCreateTrayAutostart(exe); err != nil {
		t.Fatalf("recreate: %v", err)
	}
	if err := platformRemoveTrayAutostart(); err != nil {
		t.Fatalf("remove: %v", err)
	}
	if Detect().Autostart {
		t.Error("autostart should be gone after removal")
	}
}

func TestXDGAutostartQuotesSpaces(t *testing.T) {
	sandbox(t)
	exe := "/home/u/my binaries/agent-notify"
	if err := platformCreateTrayAutostart(exe); err != nil {
		t.Fatalf("create: %v", err)
	}
	data, err := os.ReadFile(desktopPath())
	if err != nil {
		t.Fatal(err)
	}
	if want := `Exec="` + exe + `"`; !strings.Contains(string(data), want) {
		t.Errorf("spacey path should be quoted as %q:\n%s", want, data)
	}
}

func TestUnitTemplate(t *testing.T) {
	if !strings.Contains(unitTemplate, "__BIN__") {
		t.Error("template lost its __BIN__ placeholder")
	}
	rendered := strings.Replace(unitTemplate, "__BIN__", "/home/u/.local/bin/agent-notify", 1)
	for _, want := range []string{
		"ExecStart=/home/u/.local/bin/agent-notify run",
		"Restart=always", // the daemon exits cleanly on self-update
	} {
		if !strings.Contains(rendered, want) {
			t.Errorf("rendered unit lacks %q:\n%s", want, rendered)
		}
	}
}

func TestProfilePathAppend(t *testing.T) {
	home := sandbox(t)
	dir := filepath.Join(home, ".local", "bin")
	if platformPathOK(dir) {
		t.Fatal("dir cannot already be on PATH in the sandbox")
	}
	if err := platformAddToPath(dir); err != nil {
		t.Fatalf("add: %v", err)
	}
	data, err := os.ReadFile(filepath.Join(home, ".profile"))
	if err != nil {
		t.Fatalf("profile missing: %v", err)
	}
	if got := strings.Count(string(data), "# agent-notify path"); got != 1 {
		t.Errorf("marker count = %d, want 1 (idempotent):\n%s", got, data)
	}
	if !strings.Contains(string(data), dir) {
		t.Errorf("profile does not export %q", dir)
	}
	// PATH detection against the real environment variable.
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
	if !platformPathOK(dir) {
		t.Error("platformPathOK should find the dir once PATH contains it")
	}
}

func TestNudgeStateRoundTrip(t *testing.T) {
	sandbox(t)
	if !NudgeDue() {
		t.Fatal("fresh sandbox: nudge should be due")
	}
	if err := MarkNudged(); err != nil {
		t.Fatalf("mark: %v", err)
	}
	if NudgeDue() {
		t.Error("nudge already shown, should not be due again")
	}
}

func TestCopyFile(t *testing.T) {
	src, err := os.Executable() // the test binary itself: a real executable file
	if err != nil {
		t.Fatal(err)
	}
	dst := filepath.Join(t.TempDir(), "agent-notify")
	if err := copyFile(src, dst); err != nil {
		t.Fatalf("copy: %v", err)
	}
	want, _ := os.ReadFile(src)
	got, err := os.ReadFile(dst)
	if err != nil {
		t.Fatalf("destination missing: %v", err)
	}
	if string(got) != string(want) {
		t.Error("copied bytes differ")
	}
	// Overwrite in place (the self-update path).
	if err := copyFile(src, dst); err != nil {
		t.Fatalf("re-copy: %v", err)
	}
	leftovers, err := filepath.Glob(dst + ".tmp")
	if err != nil {
		t.Fatal(err)
	}
	if len(leftovers) != 0 {
		t.Errorf("temp leftovers: %v", leftovers)
	}
}
