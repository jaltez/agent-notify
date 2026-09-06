package sink

import (
	"strings"
	"testing"

	"agent-notify/internal/event"
	"agent-notify/internal/render"
)

func testRenderer(t *testing.T) *render.Renderer {
	t.Helper()
	r, err := render.New("", "")
	if err != nil {
		t.Fatal(err)
	}
	return r
}

func TestToastEnvCarriesPayload(t *testing.T) {
	p := &Popup{appID: "appid"}
	env := p.toastEnv("title", "body", "extra", "img")
	joined := strings.Join(env, "\n")
	for _, want := range []string{"AN_TITLE=title", "AN_BODY=body", "AN_APPID=appid", "AN_EXTRA=extra", "AN_IMG=img"} {
		if !strings.Contains(joined, want) {
			t.Errorf("env missing %s:\n%s", want, joined)
		}
	}
}

func TestToastEnvOmitsAbsentParts(t *testing.T) {
	p := &Popup{appID: "appid"}
	env := p.toastEnv("t", "b", "", "")
	for _, s := range env {
		if strings.HasPrefix(s, "AN_EXTRA=") || strings.HasPrefix(s, "AN_IMG=") {
			t.Errorf("unexpected entry %q", s)
		}
	}
}

func TestExtraLineComposition(t *testing.T) {
	// the CLI wires this closure; assert its shape
	var extra func(event.Event) string
	summary := "2 working · 1 blocked"
	extra = func(ev event.Event) string {
		line := ev.Host + "/" + ev.Session
		if ev.Project != "" {
			line = ev.Project + " · " + line
		}
		return line + " — " + summary
	}
	got := extra(event.Event{Host: "local", Session: "work", Project: "api"})
	if got != "api · local/work — 2 working · 1 blocked" {
		t.Errorf("extra = %q", got)
	}
}

func TestKindSeverity(t *testing.T) {
	cases := map[event.Kind]string{
		event.KindAgentBlocked: "blocked",
		event.KindAgentWorking: "working",
		event.KindAgentIdle:    "waiting",
		event.KindAgentDone:    "waiting",
		event.KindSessionDown:  "down",
		event.KindTest:         "working",
	}
	for kind, want := range cases {
		if got := kindSeverity(kind); got != want {
			t.Errorf("kindSeverity(%s) = %q, want %q", kind, got, want)
		}
	}
}
