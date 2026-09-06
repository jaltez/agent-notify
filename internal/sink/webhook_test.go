package sink

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"agent-notify/internal/config"
	"agent-notify/internal/event"
)

func testEvent() event.Event {
	return event.Event{
		Kind: event.KindAgentBlocked, Time: time.Now(),
		Host: "local", Session: "work", Agent: "claude",
		From: "working", To: "blocked",
		Title: "Refactor auth module", Project: "api", PaneID: "w1:p1",
	}
}

func TestWebhookJSONBody(t *testing.T) {
	var got map[string]any
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			t.Errorf("method = %s", r.Method)
		}
		if ct := r.Header.Get("Content-Type"); ct != "application/json" {
			t.Errorf("content-type = %s", ct)
		}
		if ua := r.Header.Get("User-Agent"); ua != "agent-notify" {
			t.Errorf("user-agent = %s", ua)
		}
		data, _ := io.ReadAll(r.Body)
		json.Unmarshal(data, &got)
	}))
	defer srv.Close()

	wh, err := NewWebhook(config.Sink{URL: srv.URL, Headers: map[string]string{"X-Token": "s3cret"}})
	if err != nil {
		t.Fatal(err)
	}
	if err := wh.Deliver(context.Background(), testEvent()); err != nil {
		t.Fatalf("deliver: %v", err)
	}
	if got["kind"] != "agent_blocked" || got["agent"] != "claude" || got["from"] != "working" || got["to"] != "blocked" {
		t.Fatalf("payload: %v", got)
	}
	if got["project"] != "api" || got["session"] != "work" || got["host"] != "local" {
		t.Fatalf("payload fields: %v", got)
	}
}

func TestWebhookTemplateBodyAndHeaders(t *testing.T) {
	var body []byte
	var auth, ct string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ = io.ReadAll(r.Body)
		auth = r.Header.Get("Authorization")
		ct = r.Header.Get("Content-Type")
	}))
	defer srv.Close()

	wh, err := NewWebhook(config.Sink{
		URL:          srv.URL,
		Method:       "PUT",
		Headers:      map[string]string{"Authorization": "Bearer tok"},
		BodyTemplate: `{"text": "{{.Agent}} {{.Verb}}"}`,
		ContentType:  "application/json",
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := wh.Deliver(context.Background(), testEvent()); err != nil {
		t.Fatalf("deliver: %v", err)
	}
	if string(body) != `{"text": "claude is blocked"}` {
		t.Errorf("body = %s", body)
	}
	if auth != "Bearer tok" || ct != "application/json" {
		t.Errorf("headers: %q %q", auth, ct)
	}
}

func TestWebhookErrorStatus(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusForbidden)
	}))
	defer srv.Close()
	wh, err := NewWebhook(config.Sink{URL: srv.URL})
	if err != nil {
		t.Fatal(err)
	}
	if err := wh.Deliver(context.Background(), testEvent()); err == nil {
		t.Fatal("expected error on 403")
	}
}

func TestCommandSinkArgvTemplate(t *testing.T) {
	dir := t.TempDir()
	c, err := NewCommand(config.Sink{Command: []string{"touch", filepath.Join(dir, "{{.Kind}}.marker")}})
	if err != nil {
		t.Fatal(err)
	}
	if err := c.Deliver(context.Background(), testEvent()); err != nil {
		t.Fatalf("deliver: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dir, "agent_blocked.marker")); err != nil {
		t.Fatalf("templated argv not executed: %v", err)
	}
}

func TestCommandSinkShell(t *testing.T) {
	dir := t.TempDir()
	out := filepath.Join(dir, "out.txt")
	c, err := NewCommand(config.Sink{
		Shell:   true,
		Command: []string{"printf {{.Agent}} > " + out},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := c.Deliver(context.Background(), testEvent()); err != nil {
		t.Fatalf("deliver: %v", err)
	}
	data, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "claude" {
		t.Fatalf("shell output = %q", data)
	}
}

func TestCommandSinkFailure(t *testing.T) {
	c, err := NewCommand(config.Sink{Command: []string{"sh", "-c", "exit 3"}})
	if err != nil {
		t.Fatal(err)
	}
	if err := c.Deliver(context.Background(), testEvent()); err == nil {
		t.Fatal("expected failure surfaced")
	}
}
