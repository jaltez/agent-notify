package config

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestDefaultIsValid(t *testing.T) {
	if err := Default().Validate(); err != nil {
		t.Fatalf("default config invalid: %v", err)
	}
}

func TestLoadMergeAndWarnings(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.toml")
	content := `
poll_typo = "1s"
cooldown = "2s"
events = ["agent_blocked"]

[herdr.windows]
sessions = ["work"]

[[sink]]
type = "bell"
`
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, warnings, err := Load(path)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if cfg.Cooldown.D() != 2*time.Second {
		t.Errorf("cooldown = %v, want 2s", cfg.Cooldown.D())
	}
	if cfg.Herdr.Windows.Sessions[0] != "work" {
		t.Errorf("windows sessions = %v", cfg.Herdr.Windows.Sessions)
	}
	// defaults survive partial overrides
	if cfg.Herdr.Local.Poll.D() != time.Second {
		t.Errorf("local poll default lost: %v", cfg.Herdr.Local.Poll.D())
	}
	if len(warnings) == 0 {
		t.Error("expected a warning for the unknown key poll_typo")
	}
}

func TestLoadValidationErrors(t *testing.T) {
	cases := []struct {
		name, body string
	}{
		{"bad sink type", "[[sink]]\ntype = \"sms\"\n"},
		{"webhook without url", "[[sink]]\ntype = \"webhook\"\n"},
		{"command without argv", "[[sink]]\ntype = \"command\"\n"},
		{"shell multi-arg", "[[sink]]\ntype = \"command\"\nshell = true\ncommand = [\"a\", \"b\"]\n"},
		{"bad enabled", "[herdr.wsl]\nenabled = \"maybe\"\n"},
		{"bad event kind", "events = [\"agent_exploded\"]\n"},
		{"bad duration", `[herdr.local] poll = "fast"`},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "c.toml")
			if err := os.WriteFile(path, []byte(tc.body), 0o644); err != nil {
				t.Fatal(err)
			}
			if _, _, err := Load(path); err == nil {
				t.Fatal("expected validation error")
			}
		})
	}
}

func TestPathPrecedence(t *testing.T) {
	if p := Path("/explicit.toml"); p != "/explicit.toml" {
		t.Errorf("flag ignored: %q", p)
	}
	t.Setenv("AGENT_NOTIFY_CONFIG", "/env.toml")
	if p := Path(""); p != "/env.toml" {
		t.Errorf("env ignored: %q", p)
	}
	if p := Path("/flag.toml"); p != "/flag.toml" {
		t.Errorf("flag should beat env: %q", p)
	}
}

func TestExampleParses(t *testing.T) {
	path := filepath.Join(t.TempDir(), "example.toml")
	if err := os.WriteFile(path, Example, 0o644); err != nil {
		t.Fatal(err)
	}
	if _, warnings, err := Load(path); err != nil {
		t.Fatalf("embedded example does not load: %v", err)
	} else if len(warnings) != 0 {
		t.Fatalf("embedded example has warnings: %v", warnings)
	}
}
