package herdr

import (
	"testing"

	"agent-notify/internal/proc"
)

const fixture = `{"id":"cli:api:snapshot","result":{"snapshot":{
  "version":"0.8.2","protocol":20,
  "agents":[
    {"agent":"claude","agent_status":"working","cwd":"/home/dev/projects/api-server",
     "foreground_cwd":"/home/dev/projects/api-server","focused":true,"pane_id":"w1:p1",
     "terminal_title":"π : Refactor auth module","terminal_title_stripped":"Refactor auth module"},
    {"agent":"codex","agent_status":"idle","cwd":"C:\\work\\website",
     "foreground_cwd":"","focused":false,"pane_id":"w2:p3",
     "terminal_title":"","terminal_title_stripped":""},
    {"agent":"aide","agent_status":"Blocked","cwd":"","focused":false,"pane_id":"w3:p1"}
  ]
}}}`

func TestParseSnapshot(t *testing.T) {
	snap, err := ParseSnapshot([]byte(fixture))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if len(snap.Agents) != 3 {
		t.Fatalf("agents = %d, want 3", len(snap.Agents))
	}
	a := snap.Agents[0]
	if a.Name != "claude" || a.Status != StatusWorking || a.Title != "Refactor auth module" ||
		a.Project != "api-server" || a.PaneID != "w1:p1" || !a.Focused {
		t.Errorf("agent[0] mis-normalized: %+v", a)
	}
	b := snap.Agents[1]
	if b.Project != "website" { // windows separator, foreground empty
		t.Errorf("agent[1] project = %q, want website", b.Project)
	}
	c := snap.Agents[2]
	if c.Status != StatusBlocked { // case-insensitive
		t.Errorf("agent[2] status = %q, want blocked", c.Status)
	}
	if c.Project != "" || c.Title != "" {
		t.Errorf("agent[2] should be empty: %+v", c)
	}
}

func TestParseSnapshotBare(t *testing.T) {
	// no envelope wrapping
	_, err := ParseSnapshot([]byte(`{"agents":[]}`))
	if err != nil {
		t.Fatalf("bare snapshot: %v", err)
	}
}

func TestSessionFromSocket(t *testing.T) {
	cases := []struct {
		sock, name, home string
	}{
		{"/home/dev/.config/herdr/herdr.sock", "default", "/home/dev"},
		{"/home/dev/.config/herdr/sessions/work/herdr.sock", "work", "/home/dev"},
		{"/tmp/other/sock", "", ""}, // unrecognized layout
	}
	for _, c := range cases {
		name, home := sessionFromSocket(c.sock)
		if name != c.name || home != c.home {
			t.Errorf("sessionFromSocket(%q) = %q,%q want %q,%q", c.sock, name, home, c.name, c.home)
		}
	}
}

func TestDecodeUTF16(t *testing.T) {
	ascii := []byte(`{"agents":[]}`)
	if got := proc.DecodeUTF16(ascii); string(got) != string(ascii) {
		t.Errorf("ascii mangled: %q", got)
	}
	// "OK" as UTF-16LE with BOM
	le := []byte{0xFF, 0xFE, 0x4F, 0x00, 0x4B, 0x00}
	if got := proc.DecodeUTF16(le); string(got) != "OK" {
		t.Errorf("BOM decode = %q", got)
	}
	// UTF-16LE without BOM (wsl.exe style)
	nb := []byte{'{', 0, '"', 0, 'a', 0, '"', 0}
	if got := proc.DecodeUTF16(nb); string(got) != `{"a"` {
		t.Errorf("no-BOM decode = %q", got)
	}
}
