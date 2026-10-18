package tray

import (
	"strings"
	"testing"

	"github.com/jaltez/agent-notify/internal/icon"

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
			{
				Host: "wsl", Name: "justdone", Up: true,
				Agents: []engine.AgentView{
					{Name: "pi", Status: "done", Wants: true, Title: "freshly finished"},
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

func TestOnlyWorkingKeepsFullRows(t *testing.T) {
	rows := buildRows(view(), nil, nil)
	joined := strings.Join(texts(rows), "\n")
	for _, want := range []string{
		"▾ wsl/default — 4 agents · 1 working · 1 waiting",
		"Refine shapes",  // working block
		"pi · Nervefall", // block second line
		"▸ 1 finished",   // latched done collapses (green group)
		"▸ 2 resting",    // idle + unlatched done
		"▸ wsl/quiet — 2 agents",
		"▸ wsl/justdone — 1 agent · 1 waiting", // finished-only session collapses
		"▸ wsl/down — 1 agent · offline",
	} {
		if !strings.Contains(joined, want) {
			t.Errorf("missing %q in:\n%s", want, joined)
		}
	}
	for _, r := range rows {
		if r.kind == rowAgent && strings.Contains(r.text, "needs you") {
			t.Error("finished agents must not get full blocks")
		}
		if r.kind == rowAgentCompact {
			t.Errorf("compact rows must not appear while groups are closed: %q", r.text)
		}
	}
	s := find(t, rows, rowSession, "wsl/quiet")
	if s.open || !s.dim {
		t.Error("all-resting session must default to collapsed and dim")
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
		t.Errorf("groups start closed: expected 0 compact rows, got %d", compact)
	}

	d := find(t, rows, rowSession, "wsl/default")
	if d.open {
		t.Error("user override must collapse the active session")
	}
	for _, r := range rows {
		if r.kind == rowAgent && strings.Contains(r.text, "Refine shapes") {
			t.Error("collapsed session must hide its attention blocks")
		}
	}
}

func TestGroupToggleListsCompactRows(t *testing.T) {
	rows := buildRows(view(), nil, map[string]bool{
		"wsl/default/finished": true,
		"wsl/default/resting":  true,
	})
	var finished, resting int
	for _, r := range rows {
		switch {
		case r.kind == rowGroup && r.toggle == "wsl/default/finished":
			if !strings.HasPrefix(r.text, "▾") || !r.open {
				t.Errorf("finished group should read expanded, got %q", r.text)
			}
		case r.kind == rowAgentCompact:
			if r.dot == colorRef(icon.Color("waiting")) {
				finished++
			} else {
				resting++
			}
		}
	}
	if finished != 1 || resting != 2 {
		t.Errorf("compact rows: finished=%d resting=%d, want 1 and 2", finished, resting)
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

func TestGdiSafe(t *testing.T) {
	cases := []struct{ in, want string }{
		{"\U0001F3E0 README: basics", "README: basics"},                                     // leading emoji + space
		{"README \u2699\uFE0F", "README"},                                                   // emoji + variation selector
		{"▾ wsl/default \u00B7 pi \u00B7 Redise\u00F1ar", "▾ wsl/default · pi · Rediseñar"}, // keep chevrons, dots, accents
		{"a\u200Db", "ab"},        // zero-width joiner
		{"\u2B50 stars", "stars"}, // star
		{"日本語 タスク", "日本語 タスク"},    // CJK passes
		{"plain", "plain"},
	}
	for _, c := range cases {
		if got := gdiSafe(c.in); got != c.want {
			t.Errorf("gdiSafe(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}
