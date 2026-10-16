package engine

import (
	"testing"
	"time"

	"github.com/jaltez/agent-notify/internal/event"
	"github.com/jaltez/agent-notify/internal/herdr"
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

	// the down edge arrives on the downAfter-th consecutive failure
	var evs []event.Event
	var changed bool
	for i := 0; i < downAfter; i++ {
		evs, changed = tr.fail(time.Now())
	}
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
		{Host: "local", Name: "a", Up: true, Agents: []AgentView{{Status: "working"}, {Status: "idle", Wants: true}}},
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
	// one working + one waiting: working wins (busy fleet reads as busy)
	if v.Severity() != "working" {
		t.Errorf("severity = %q, want working", v.Severity())
	}
	if s := v.Summary(); s != "1 working · 1 waiting" {
		t.Errorf("summary = %q", s)
	}
	// green only when every agent stopped and someone latched a finish
	v.Sessions[0].Agents[0].Status = "done"
	v.Sessions[0].Agents[0].Wants = true
	if v.Severity() != "waiting" {
		t.Errorf("severity = %q, want waiting", v.Severity())
	}
	// an idle agent that never latched is quiet, not waiting
	v.Sessions[0].Agents[1].Wants = false
	v.Sessions[0].Agents[0] = AgentView{Status: "idle"}
	if v.Severity() != "idle" {
		t.Errorf("severity = %q, want idle for unlatched fleet", v.Severity())
	}
	if s := v.Summary(); s != "all quiet" {
		t.Errorf("summary = %q, want all quiet", s)
	}
}

func TestTrackerAttentionLatch(t *testing.T) {
	tr := newTracker("local", "work")
	pane := "w1:p1"
	view := func() View {
		e := &Engine{sessions: map[string]*tracker{"local/work": tr}}
		return e.View()
	}
	// baseline idle: resting, not waiting; a fresh tray must read gray
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", pane, "t")}}, time.Now())
	if view().Severity() != "idle" {
		t.Fatal("startup with idle agents must not be waiting")
	}
	// observed finish latches green
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("working", pane, "t")}}, time.Now())
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", pane, "t")}}, time.Now())
	v := view()
	if v.Severity() != "waiting" || !v.Sessions[0].Agents[0].Wants {
		t.Fatalf("finish should latch waiting: %+v", v)
	}
	// idle → done reshuffle keeps the latch
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("done", pane, "t")}}, time.Now())
	if view().Severity() != "waiting" {
		t.Fatal("idle→done must keep the latch")
	}
	// resuming work spends the attention
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("working", pane, "t")}}, time.Now())
	if v := view(); v.Severity() != "working" || v.Sessions[0].Agents[0].Wants {
		t.Fatalf("work must clear the latch: %+v", v)
	}
	// a second finish latches again
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("done", pane, "t")}}, time.Now())
	if view().Severity() != "waiting" {
		t.Fatal("second finish should latch again")
	}
	// the pane disappearing drops the latch with it
	tr.apply(&herdr.Snapshot{}, time.Now())
	if v := view(); v.Severity() != "idle" || len(v.Sessions[0].Agents) != 0 {
		t.Fatalf("agent_left must drop the latch: %+v", v)
	}
}

func TestTrackerStickyUnknown(t *testing.T) {
	tr := newTracker("local", "work")
	pane := "w1:p1"
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("working", pane, "t")}}, time.Now())
	// a detection flap must not drop the agent from counts nor gray its row
	evs, _ := tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("unknown", pane, "t")}}, time.Now())
	if len(evs) != 0 {
		t.Fatalf("unknown flap emitted events: %v", kinds(evs))
	}
	if a := tr.agents[pane]; a.Status != "working" {
		t.Fatalf("flap should keep last status, got %q", a.Status)
	}
	// an unknown reading at baseline stays unknown (a real reading wins later)
	tr2 := newTracker("local", "other")
	tr2.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("unknown", "w9:p1", "t")}}, time.Now())
	tr2.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("working", "w9:p1", "t")}}, time.Now())
	if a := tr2.agents["w9:p1"]; a.Status != "working" {
		t.Fatalf("baseline unknown must not be sticky, got %q", a.Status)
	}
}

func TestRowSeverity(t *testing.T) {
	cases := []struct {
		status string
		wants  bool
		want   string
	}{
		{"blocked", false, "blocked"},
		{"blocked", true, "blocked"},
		{"working", false, "working"},
		{"idle", true, "waiting"},
		{"done", true, "waiting"},
		{"idle", false, "idle"},
		{"done", false, "idle"},
		{"unknown", false, "idle"},
		{"unknown", true, "idle"},
	}
	for _, tc := range cases {
		if got := RowSeverity(tc.status, tc.wants); got != tc.want {
			t.Errorf("RowSeverity(%q, %v) = %q, want %q", tc.status, tc.wants, got, tc.want)
		}
	}
}

func TestApplyChangeDetection(t *testing.T) {
	tr := newTracker("local", "work")
	base := &herdr.Snapshot{Agents: []herdr.Agent{agent("working", "w1:p1", "Refactor auth")}}

	// baseline appears
	if _, changed := tr.apply(base, time.Now()); !changed {
		t.Error("baseline should mark changed")
	}
	// identical snapshot: no change
	if _, changed := tr.apply(base, time.Now()); changed {
		t.Error("identical snapshot flagged as changed")
	}
	// status move
	flipped := &herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "Refactor auth")}}
	if _, changed := tr.apply(flipped, time.Now()); !changed {
		t.Error("status change not detected")
	}
	// title-only refresh (view shows titles)
	retitled := &herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "Fix login flow")}}
	if _, changed := tr.apply(retitled, time.Now()); !changed {
		t.Error("title change not detected")
	}
	// spawn
	grown := &herdr.Snapshot{Agents: []herdr.Agent{
		agent("idle", "w1:p1", "Fix login flow"),
		agent("working", "w2:p1", "Write docs"),
	}}
	if _, changed := tr.apply(grown, time.Now()); !changed {
		t.Error("spawn not detected")
	}
	// leave
	if _, changed := tr.apply(retitled, time.Now()); !changed {
		t.Error("leave not detected")
	}
	// and quiet again
	if _, changed := tr.apply(retitled, time.Now()); changed {
		t.Error("stable snapshot flagged as changed")
	}
}

func TestDownRequiresConsecutiveFailures(t *testing.T) {
	tr := newTracker("local", "work")
	now := time.Now()
	tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "t")}}, now)

	// One or two failed polls are noise (slow wsl.exe, transient timeout).
	for i := 0; i < downAfter-1; i++ {
		evs, changed := tr.fail(now.Add(time.Duration(i) * time.Second))
		if len(evs) != 0 || changed {
			t.Fatalf("failure %d flapped the session: %v", i+1, kinds(evs))
		}
		if !tr.up {
			t.Fatalf("failure %d took the session down", i+1)
		}
	}
	// The third consecutive failure marks it down.
	evs, changed := tr.fail(now)
	if len(evs) != 1 || evs[0].Kind != event.KindSessionDown || !changed {
		t.Fatalf("expected session_down on failure %d, got %v", downAfter, kinds(evs))
	}
	// Further failures stay silent.
	if evs, _ := tr.fail(now); len(evs) != 0 {
		t.Fatalf("duplicate session_down: %v", kinds(evs))
	}
	// Recovery resets the counter and announces the comeback.
	evs, changed = tr.apply(&herdr.Snapshot{Agents: []herdr.Agent{agent("idle", "w1:p1", "t")}}, now)
	if len(evs) != 1 || evs[0].Kind != event.KindSessionUp || !changed {
		t.Fatalf("expected session_up on recovery, got %v", kinds(evs))
	}
	if !tr.up || tr.fails != 0 {
		t.Fatalf("recovery state wrong: up=%v fails=%d", tr.up, tr.fails)
	}
}
