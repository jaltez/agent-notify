package engine

import (
	"fmt"
	"time"

	"github.com/jaltez/agent-notify/internal/event"
	"github.com/jaltez/agent-notify/internal/herdr"
)

// tracker holds the last known state of one session and turns snapshot
// diffs into events.
type tracker struct {
	host, name string
	seen       bool // had at least one successful snapshot
	up         bool // last fetch succeeded

	agents map[string]herdr.Agent // keyed by PaneID
	order  []string               // pane keys in last snapshot order (stable UI)
}

func newTracker(host, name string) *tracker {
	return &tracker{host: host, name: name, agents: map[string]herdr.Agent{}}
}

// apply folds a successful snapshot in and returns the transition events.
// The first snapshot is a baseline: it populates state without events.
// changed is true only when the view-relevant state actually moved — the
// tray rebuilds its menu on it and must not do that every poll.
func (t *tracker) apply(snap *herdr.Snapshot, now time.Time) (evs []event.Event, changed bool) {
	first := !t.seen
	wasDown := t.seen && !t.up
	changed = first || wasDown
	if wasDown {
		evs = append(evs, t.sessionEvent(event.KindSessionUp, now))
	}
	t.seen, t.up = true, true

	next := make(map[string]herdr.Agent, len(snap.Agents))
	nextOrder := make([]string, 0, len(snap.Agents))
	for i, a := range snap.Agents {
		k := agentKey(a, i)
		next[k] = a
		nextOrder = append(nextOrder, k)
		prev, ok := t.agents[k]
		switch {
		case !ok:
			if !first {
				evs = append(evs, t.agentEvent(event.KindAgentSpawned, herdr.Agent{}, a, now))
				changed = true
			}
		case prev.Status != a.Status:
			if ev := t.transition(prev, a, now); ev != nil {
				evs = append(evs, *ev)
			}
			changed = true
		case prev != a: // title/project/focus refresh: view-only change
			changed = true
		}
	}
	for k, prev := range t.agents {
		if _, ok := next[k]; !ok {
			if !first {
				evs = append(evs, t.agentEvent(event.KindAgentLeft, prev, herdr.Agent{}, now))
			}
			changed = true
		}
	}
	t.agents = next
	t.order = nextOrder
	return evs, changed
}

// transition maps a status change to an event, or nil for noise.
func (t *tracker) transition(prev, cur herdr.Agent, now time.Time) *event.Event {
	var kind event.Kind
	switch {
	case cur.Status == herdr.StatusBlocked && prev.Status != herdr.StatusBlocked:
		kind = event.KindAgentBlocked
	case prev.Status == herdr.StatusWorking && cur.Status == herdr.StatusIdle:
		kind = event.KindAgentIdle
	case prev.Status == herdr.StatusWorking && cur.Status == herdr.StatusDone:
		kind = event.KindAgentDone
	case cur.Status == herdr.StatusWorking && prev.Status != herdr.StatusWorking:
		kind = event.KindAgentWorking
	default:
		return nil // e.g. unknown flapping or idle ↔ done reshuffles
	}
	ev := t.agentEvent(kind, prev, cur, now)
	return &ev
}

// fail records a failed fetch, emitting session_down on the up→down edge.
func (t *tracker) fail(now time.Time) (evs []event.Event, changed bool) {
	if t.up {
		evs = append(evs, t.sessionEvent(event.KindSessionDown, now))
		changed = true
	}
	t.up = false
	return evs, changed
}

func (t *tracker) sessionEvent(kind event.Kind, now time.Time) event.Event {
	return event.Event{
		Kind:    kind,
		Time:    now,
		Source:  "herdr",
		Host:    t.host,
		Session: t.name,
	}
}

func (t *tracker) agentEvent(kind event.Kind, prev, cur herdr.Agent, now time.Time) event.Event {
	ev := event.Event{
		Kind:    kind,
		Time:    now,
		Source:  "herdr",
		Host:    t.host,
		Session: t.name,
		Agent:   cur.Name,
		Title:   cur.Title,
		Project: cur.Project,
		PaneID:  cur.PaneID,
		Focused: cur.Focused,
		From:    prev.Status,
		To:      cur.Status,
	}
	if ev.Agent == "" {
		ev.Agent = prev.Name // agent_left carries the departed agent
	}
	if ev.Title == "" {
		ev.Title = prev.Title
	}
	if ev.Project == "" {
		ev.Project = prev.Project
	}
	if ev.PaneID == "" {
		ev.PaneID = prev.PaneID
	}
	return ev
}

func agentKey(a herdr.Agent, i int) string {
	if a.PaneID != "" {
		return a.PaneID
	}
	return fmt.Sprintf("%s#%d", a.Name, i)
}
