package render

import (
	"testing"
	"time"

	"github.com/jaltez/agent-notify/internal/event"
)

func TestDefaultTemplates(t *testing.T) {
	r, err := New("", "")
	if err != nil {
		t.Fatal(err)
	}
	title, body := r.Render(event.Event{
		Kind: event.KindAgentDone, Time: time.Now(),
		Host: "local", Session: "work", Agent: "claude",
		Title: "Refactor auth module", Project: "api",
	})
	if title != "claude finished" {
		t.Errorf("title = %q", title)
	}
	if body != "Refactor auth module" {
		t.Errorf("body = %q", body)
	}

	// session event: no agent → session name; no title → project; neither → host/session
	title, body = r.Render(event.Event{Kind: event.KindSessionDown, Session: "work", Host: "wsl"})
	if title != "work went offline" {
		t.Errorf("session title = %q", title)
	}
	if body != "wsl/work" {
		t.Errorf("session body = %q", body)
	}

	title, body = r.Render(event.Event{Kind: event.KindAgentBlocked, Session: "work", Agent: "codex", Project: "website"})
	if title != "codex is blocked" || body != "website" {
		t.Errorf("fallback body: %q / %q", title, body)
	}
}

func TestCustomTemplates(t *testing.T) {
	r, err := New("{{.Session}}: {{.Kind}}", "{{.Verb}} {{.Project}} {{.Focused}}")
	if err != nil {
		t.Fatal(err)
	}
	title, body := r.Render(event.Event{
		Kind: event.KindAgentIdle, Session: "work", Agent: "claude",
		Project: "api", Focused: true,
	})
	if title != "work: agent_idle" {
		t.Errorf("title = %q", title)
	}
	if body != "went idle api true" {
		t.Errorf("body = %q", body)
	}
}

func TestBadTemplateErrors(t *testing.T) {
	if _, err := New("{{.Nope", ""); err == nil {
		t.Error("bad title template accepted")
	}
	if _, err := New("", "{{.Missing"); err == nil {
		t.Error("bad body template accepted")
	}
}
