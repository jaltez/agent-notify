package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestResolveFlagAndEnv(t *testing.T) {
	if p, src := Resolve("/x.toml"); p != "/x.toml" || src != "flag" {
		t.Errorf("flag: %q %q", p, src)
	}
	t.Setenv("AGENT_NOTIFY_CONFIG", "/e.toml")
	if p, src := Resolve(""); p != "/e.toml" || src != "env" {
		t.Errorf("env: %q %q", p, src)
	}
}

func TestResolveDefaultWhenPresent(t *testing.T) {
	// Path() joins <UserConfigDir>/agent-notify/config.toml; on Linux
	// UserConfigDir honors XDG_CONFIG_HOME.
	dir := t.TempDir()
	def := filepath.Join(dir, "agent-notify", "config.toml")
	if err := os.MkdirAll(filepath.Dir(def), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(def, []byte("events = [\"none\"]\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("XDG_CONFIG_HOME", dir)
	if p, src := Resolve(""); src != "default" || p != def {
		t.Errorf("default: %q %q", p, src)
	}
}

func TestResolveWindowsSharedFallback(t *testing.T) {
	if os.PathSeparator != '/' {
		t.Skip("unix-only test")
	}
	mnt := t.TempDir()
	// simulate /mnt/c/Users/<user>/AppData/Roaming/agent-notify/config.toml
	shared := filepath.Join(mnt, "Users", "someone", "AppData", "Roaming", "agent-notify", "config.toml")
	if err := os.MkdirAll(filepath.Dir(shared), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(shared, []byte("# shared\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	empty := t.TempDir() // no OS-default config anywhere
	t.Setenv("XDG_CONFIG_HOME", empty)
	t.Setenv("AGENT_NOTIFY_CONFIG", "")
	defer func(old string) { mntC = old }(mntC)
	mntC = mnt

	p, src := Resolve("")
	if src != "windows-shared" || p != shared {
		t.Errorf("windows-shared: %q %q", p, src)
	}
}

func TestWindowsSharedPrefersOSDefault(t *testing.T) {
	if os.PathSeparator != '/' {
		t.Skip("unix-only test")
	}
	mnt := t.TempDir()
	shared := filepath.Join(mnt, "Users", "u", "AppData", "Roaming", "agent-notify", "config.toml")
	os.MkdirAll(filepath.Dir(shared), 0o755)
	os.WriteFile(shared, []byte("# shared\n"), 0o644)
	defer func(old string) { mntC = old }(mntC)
	mntC = mnt

	local := t.TempDir()
	def := filepath.Join(local, "agent-notify", "config.toml")
	os.MkdirAll(filepath.Dir(def), 0o755)
	os.WriteFile(def, []byte("# local\n"), 0o644)
	t.Setenv("XDG_CONFIG_HOME", local)
	t.Setenv("AGENT_NOTIFY_CONFIG", "")

	if p, src := Resolve(""); src != "default" || p != def {
		t.Errorf("local default should win: %q %q", p, src)
	}
}
