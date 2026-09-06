package engine

import (
	"context"
	"sync"
	"testing"
	"time"

	"agent-notify/internal/event"
	"agent-notify/internal/herdr"
)

// fakeBackend feeds canned snapshots per session.
type fakeBackend struct {
	mu       sync.Mutex
	sessions []herdr.Session
	snaps    map[string]*herdr.Snapshot
	errs     map[string]error
}

func (f *fakeBackend) Name() string          { return "fake" }
func (f *fakeBackend) Rescan() time.Duration { return 0 }

func (f *fakeBackend) Discover(ctx context.Context) ([]herdr.Session, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]herdr.Session(nil), f.sessions...), nil
}

func (f *fakeBackend) Fetch(ctx context.Context, s herdr.Session) (*herdr.Snapshot, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if err, ok := f.errs[s.Name]; ok {
		return nil, err
	}
	return f.snaps[s.Name], nil
}

func (f *fakeBackend) set(name string, agents ...herdr.Agent) {
	f.mu.Lock()
	defer f.mu.Unlock()
	found := false
	for _, s := range f.sessions {
		if s.Name == name {
			found = true
		}
	}
	if !found {
		f.sessions = append(f.sessions, herdr.Session{Host: "local", Name: name, Ref: name})
	}
	if f.snaps == nil {
		f.snaps = map[string]*herdr.Snapshot{}
	}
	f.snaps[name] = &herdr.Snapshot{Agents: agents}
}

// recorder collects the engine's events safely.
type recorder struct {
	mu  sync.Mutex
	evs []event.Event
}

func (r *recorder) attach(e *Engine) {
	go func() {
		for ev := range e.Events() {
			r.mu.Lock()
			r.evs = append(r.evs, ev)
			r.mu.Unlock()
		}
	}()
}

func (r *recorder) snapshot() []event.Event {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]event.Event(nil), r.evs...)
}

func newTestEngine(t *testing.T, kinds []event.Kind, opts ...Option) (*Engine, *fakeBackend) {
	t.Helper()
	fb := &fakeBackend{}
	ks, err := event.ParseSet(event.Names(kinds))
	if err != nil {
		t.Fatal(err)
	}
	opts = append([]Option{WithBackends(fb)}, opts...)
	e, err := New(Config{Kinds: kindSet(ks)}, opts...)
	if err != nil {
		t.Fatalf("engine: %v", err)
	}
	return e, fb
}

func kindSet(ks []event.Kind) map[event.Kind]bool {
	m := make(map[event.Kind]bool, len(ks))
	for _, k := range ks {
		m[k] = true
	}
	return m
}

// waitFor polls cond until true or the deadline passes.
func waitFor(t *testing.T, d time.Duration, cond func() bool) bool {
	t.Helper()
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		if cond() {
			return true
		}
		time.Sleep(10 * time.Millisecond)
	}
	return false
}

func TestEngineEmitsAttentionTransition(t *testing.T) {
	e, fb := newTestEngine(t, event.Attention)
	working := herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "working", Title: "Refactor auth", Project: "api"}
	idle := herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "idle", Title: "Refactor auth", Project: "api"}
	fb.set("work", working)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	rec := &recorder{}
	rec.attach(e)
	go e.Run(ctx)

	if !waitFor(t, 2*time.Second, func() bool { return len(e.View().Sessions) == 1 }) {
		t.Fatal("baseline never appeared in view")
	}
	if evs := rec.snapshot(); len(evs) != 0 {
		t.Fatalf("baseline emitted events: %+v", evs)
	}

	fb.set("work", idle)
	if !waitFor(t, 2*time.Second, func() bool { return len(rec.snapshot()) == 1 }) {
		t.Fatalf("expected 1 event, got %+v", rec.snapshot())
	}
	ev := rec.snapshot()[0]
	if ev.Kind != event.KindAgentIdle || ev.Agent != "claude" || ev.From != "working" || ev.To != "idle" {
		t.Fatalf("event = %+v", ev)
	}
	v := e.View()
	if v.Severity() != "waiting" || len(v.Sessions) != 1 || v.Sessions[0].Agents[0].Status != "idle" {
		t.Fatalf("view: %+v", v)
	}
}

func TestEngineKindsFilter(t *testing.T) {
	e, fb := newTestEngine(t, event.Attention)
	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "idle"})

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	rec := &recorder{}
	rec.attach(e)
	go e.Run(ctx)

	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "working"})
	time.Sleep(300 * time.Millisecond)
	if evs := rec.snapshot(); len(evs) != 0 {
		t.Fatalf("non-attention event leaked: %+v", evs)
	}
	if v := e.View(); v.Severity() != "working" {
		t.Fatalf("view should still update: %q", v.Severity())
	}
}

func TestEngineCooldown(t *testing.T) {
	e, fb := newTestEngine(t, event.Attention)
	e.cfg.Cooldown = time.Hour
	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "working"})

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	rec := &recorder{}
	rec.attach(e)
	go e.Run(ctx)

	if !waitFor(t, 2*time.Second, func() bool { return len(e.View().Sessions) == 1 }) {
		t.Fatal("baseline never appeared")
	}
	// Two rapid working→idle cycles: the second must be suppressed.
	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "idle"})
	if !waitFor(t, 2*time.Second, func() bool { return len(rec.snapshot()) == 1 }) {
		t.Fatal("first event missing")
	}
	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "working"})
	fb.set("work", herdr.Agent{PaneID: "w1:p1", Name: "claude", Status: "idle"})
	time.Sleep(300 * time.Millisecond)
	if evs := rec.snapshot(); len(evs) != 1 {
		t.Fatalf("cooldown did not suppress duplicate: %d events", len(evs))
	}
}

func TestEngineSessionFilter(t *testing.T) {
	e, fb := newTestEngine(t, event.Attention)
	e.cfg.Include = []string{"work*"}
	e.cfg.Exclude = []string{"work-scratch"}
	fb.set("work")
	fb.set("work-scratch")
	fb.set("personal")

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go e.Run(ctx)

	if !waitFor(t, 2*time.Second, func() bool {
		v := e.View()
		return len(v.Sessions) == 1 && v.Sessions[0].Name == "work"
	}) {
		t.Fatalf("filter mismatch: %+v", e.View().Sessions)
	}
}
