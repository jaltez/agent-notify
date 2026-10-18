package tray

import (
	"strings"
	"testing"

	"github.com/jaltez/agent-notify/internal/engine"
)

func view() engine.View {
	return engine.View{
		Sessions: []engine.SessionView{
			{
				Host: "wsl", Name: "default", Up: true,
				Agents: []engine.AgentView{
					{Name: "pi", Status: "working", Title: "Refine shapes", Project: "Nervefall"},
					{Name: "omp", Status: "idle", Title: "old run"},
					{Name: "omp", Status: "done", Title: "finished task"},
					{Name: "pi", Status: "done", Wants: true, Title: "needs you"},
				},
			},
			{
				Host: "wsl", Name: "quiet", Up: true,
				Agents: []engine.AgentView{
					{Name: "pi", Status: "idle", Title: "resting one"},
					{Name: "pi", Status: "idle", Title: "resting two"},
				},
			},
			{Host: "wsl", Name: "down", Up: false, Agents: []engine.AgentView{
				{Name: "pi", Status: "idle", Title: "gone"},
			}},
		},
	}
}

func texts(rows []flyRow) []string {
	out := make([]string, 0, len(rows))
	for _, r := range rows {
		out = append(out, r.text)
		if r.sub != "" {
			out = append(out, r.sub)
		}
	}
	return out
}

func find(t *testing.T, rows []flyRow, kind rowKind, key string) flyRow {
	t.Helper()
	for _, r := range rows {
		if r.kind == kind && r.toggle == key {
			return r
		}
	}
	t.Fatalf("no %v row for %q in %v", kind, key, texts(rows))
	return flyRow{}
}

func TestAttentionSessionExpandsByDefault(t *testing.T) {
	rows := buildRows(view(), nil, nil)
	got := texts(rows)
	joined := strings.Join(got, "\n")
	for _, want := range []string{
		"▾ wsl/default — 4 agents · 1 working · 1 waiting",
		"Refine shapes",  // working block
		"needs you",      // latched done block
		"pi · Nervefall", // block second line
		"▸ 2 resting",    // idle+unlatched done collapse
		"▸ wsl/quiet — 2 agents",
		"▸ wsl/down — 1 agent · offline",
	} {
		if !strings.Contains(joined, want) {
			t.Errorf("missing %q in:\n%s", want, joined)
		}
	}
	for _, r := range rows {
		if r.kind == rowAgentCompact {
			t.Errorf("compact rows must not appear while the resting group is closed: %q", r.text)
		}
	}
	// quiet session collapses to its header alone
	s := find(t, rows, rowSession, "wsl/quiet")
	if s.open {
		t.Error("all-resting session must default to collapsed")
	}
	if !s.dim {
		t.Error("session without attention agents should paint dim")
	}
}

func TestOverridesFlipDefaults(t *testing.T) {
	openState := map[string]bool{"wsl/quiet": true, "wsl/default": false}
	rows := buildRows(view(), openState, nil)

	q := find(t, rows, rowSession, "wsl/quiet")
	if !q.open {
		t.Error("user override must expand the quiet session")
	}
	var compact int
	for _, r := range rows {
		if r.kind == rowAgentCompact {
			compact++
		}
	}
	if compact != 0 {
		t.Errorf("resting group starts closed: expected 0 compact rows, got %d", compact)
	}

	d := find(t, rows, rowSession, "wsl/default")
	if d.open {
		t.Error("user override must collapse the attention session")
	}
	for _, r := range rows {
		if r.kind == rowAgent && r.toggle == "" && strings.Contains(r.text, "Refine shapes") {
			t.Error("collapsed session must hide its attention blocks")
		}
	}
}

func TestRestingOverrideListsCompactRows(t *testing.T) {
	rows := buildRows(view(),
		map[string]bool{"wsl/quiet": true},
		map[string]bool{"wsl/quiet": true})
	var compact int
	for _, r := range rows {
		if r.kind == rowAgentCompact {
			compact++
		}
	}
	if compact != 2 {
		t.Errorf("open resting group should paint 2 compact rows, got %d", compact)
	}
}

func TestRestingGroupToggles(t *testing.T) {
	rows := buildRows(view(), nil, map[string]bool{"wsl/default": true})
	var sawCompact int
	for _, r := range rows {
		if r.kind == rowAgentCompact {
			sawCompact++
		}
		if r.kind == rowResting && r.toggle == "wsl/default" {
			if !strings.HasPrefix(r.text, "▾") || !r.open {
				t.Errorf("resting line should read expanded, got %q", r.text)
			}
		}
	}
	if sawCompact != 2 {
		t.Errorf("open resting group should paint 2 compact rows, got %d", sawCompact)
	}
}

func TestStateBarCoversAllAgents(t *testing.T) {
	rows := buildRows(view(), nil, nil)
	var px int32
	for _, r := range rows {
		if r.kind == rowSummarySub {
			for _, seg := range r.bar {
				px += seg.px
			}
			if len(r.bar) != 4 { // blocked, working, waiting, idle (no down agents in total)
				t.Errorf("bar segments = %d, want 4: %+v", len(r.bar), r.bar)
			}
		}
	}
	const inner = flyWidth - 2*flyPad
	if px != inner {
		t.Errorf("bar width = %d, want %d", px, inner)
	}
}

func TestEmptyView(t *testing.T) {
	rows := buildRows(engine.View{}, nil, nil)
	if len(rows) != 3 || rows[2].kind != rowEmpty {
		t.Errorf("empty view should end with the placeholder row, got %v", texts(rows))
	}
}
