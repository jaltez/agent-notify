package engine

import (
	"testing"
	"time"

	"agent-notify/internal/event"
	"agent-notify/internal/herdr"
)

func agent(status, pane, title string) herdr.Agent {
	return herdr.Agent{PaneID: pane, Name: "claude", Status: status, Title: title, Project: "api-server"}
}

func kinds(evs []event.Event) []event.Kind {
	out := make([]event.Kind, len(evs))
	for i, e := range evs {
		out[i] = e.Kind
	}
	return out
}

func TestFirstSnapshotIsBaseline(t *testing.T) {
	tr := newTracker("local", "work")
	evs, changed := tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "t")}}, time.Now())
	if len(evs) != 0 {
		t.Fatalf("baseline emitted events: %v", kinds(evs))
	}
	if !changed {
		t.Error("baseline should mark state changed (view appears)")
	}
}

func TestAttentionTransitions(t *testing.T) {
	cases := []struct {
		from, to string
		want     event.Kind
	}{
		{"working", "idle", event.KindAgentIdle},
		{"working", "done", event.KindAgentDone},
		{"idle", "blocked", event.KindAgentBlocked},
		{"working", "blocked", event.KindAgentBlocked},
		{"unknown", "blocked", event.KindAgentBlocked},
		{"idle", "working", event.KindAgentWorking},
		// noise: no event
		{from: "idle", to: "done"},
		{from: "unknown", to: "unknown"},
		{from: "blocked", to: "unknown"},
		{from: "idle", to: "idle"},
	}
	for _, tc := range cases {
		tr := newTracker("local", "work")
		pane := "w1:p1"
		tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent(tc.from, pane, "t")}}, time.Now())
		evs, _ := tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent(tc.to, pane, "t")}}, time.Now())
		if tc.want == "" {
			if len(evs) != 0 {
				t.Errorf("%s→%s: unexpected events %v", tc.from, tc.to, kinds(evs))
			}
			continue
		}
		if len(evs) != 1 || evs[0].Kind != tc.want {
			t.Errorf("%s→%s: got %v, want [%s]", tc.from, tc.to, kinds(evs), tc.want)
			continue
		}
		ev := evs[0]
		if ev.From != tc.from || ev.To != tc.to || ev.Agent != "claude" || ev.Session != "work" || ev.Host != "local" {
			t.Errorf("%s→%s: event fields wrong: %+v", tc.from, tc.to, ev)
		}
	}
}

func TestSpawnAndLeave(t *testing.T) {
	tr := newTracker("local", "work")
	base := &herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "one")}}
	tr.apply(base, time.Now())

	evs, _ := tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{
		agent("idle", "w1:p1", "one"),
		agent("working", "w2:p1", "two"),
	}}, time.Now())
	if len(evs) != 1 || evs[0].Kind != event.KindAgentSpawned || evs[0].PaneID != "w2:p1" {
		t.Fatalf("spawn: %v", kinds(evs))
	}

	evs, _ = tr.apply(base, time.Now())
	if len(evs) != 1 || evs[0].Kind != event.KindAgentLeft || evs[0].PaneID != "w2:p1" || evs[0].Agent != "claude" {
		t.Fatalf("leave: %v %+v", kinds(evs), evs)
	}
}

func TestSessionUpDownEdges(t *testing.T) {
	tr := newTracker("local", "work")
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "t")}}, time.Now())

	evs, changed := tr.fail(time.Now())
	if len(evs) != 1 || evs[0].Kind != event.KindSessionDown || !changed {
		t.Fatalf("down edge: %v changed=%v", kinds(evs), changed)
	}
	// repeated failures stay quiet
	evs, changed = tr.fail(time.Now())
	if len(evs) != 0 || changed {
		t.Fatalf("repeat fail should be quiet: %v", kinds(evs))
	}
	evs, _ = tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "t")}}, time.Now())
	if len(evs) != 1 || evs[0].Kind != event.KindSessionUp {
		t.Fatalf("up edge: %v", kinds(evs))
	}
}

func TestFailBeforeFirstSuccessIsSilent(t *testing.T) {
	tr := newTracker("local", "ghost")
	evs, changed := tr.fail(time.Now())
	if len(evs) != 0 || changed {
		t.Fatalf("never-seen session should not emit: %v", kinds(evs))
	}
}

func TestViewSeverityAndCounts(t *testing.T) {
	var v View
	v.Sessions = []SessionView{
		{Host: "local", Name: "a", Up: true, Agents: []AgentView{{Status: "working"}, {Status: "idle"}}},
		{Host: "local", Name: "b", Up: false},
	}
	c := v.Counts()
	if c.Working != 1 || c.Waiting != 1 || c.Down != 1 || c.Blocked != 0 {
		t.Fatalf("counts: %+v", c)
	}
	if v.Severity() != "down" {
		t.Errorf("severity = %q, want down", v.Severity())
	}
	v.Sessions[1].Up = true
	if v.Severity() != "waiting" {
		t.Errorf("severity = %q, want waiting", v.Severity())
	}
	v.Sessions[0].Agents[1].Status = "working"
	if v.Severity() != "working" {
		t.Errorf("severity = %q, want working", v.Severity())
	}
	if s := v.Summary(); s != "2 working" {
		t.Errorf("summary = %q", s)
	}
}
