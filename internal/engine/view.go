package engine

import (
	"sort"
	"strconv"
	"strings"

	"agent-notify/internal/herdr"
)

// AgentView is one agent row in a View.
type AgentView struct {
	Name    string
	Status  string
	Title   string
	Project string
	PaneID  string
	Focused bool
}

// SessionView is one session (last known state) in a View.
type SessionView struct {
	Host   string
	Name   string
	Up     bool
	Agents []AgentView
}

// View is a point-in-time copy of everything the engine knows.
type View struct {
	Sessions []SessionView
}

// Counts summarizes a View: working agents, agents waiting for a human
// (idle or done), blocked agents, and offline sessions.
type Counts struct {
	Working int
	Waiting int
	Blocked int
	Down    int
}

// Counts aggregates agent statuses across reachable sessions.
func (v View) Counts() Counts {
	var c Counts
	for _, s := range v.Sessions {
		if !s.Up {
			c.Down++
			continue
		}
		for _, a := range s.Agents {
			switch a.Status {
			case herdr.StatusWorking:
				c.Working++
			case herdr.StatusIdle, herdr.StatusDone:
				c.Waiting++
			case herdr.StatusBlocked:
				c.Blocked++
			}
		}
	}
	return c
}

// Severity is the worst state in the view, driving the tray icon color:
// blocked > down > waiting > working > idle.
func (v View) Severity() string {
	c := v.Counts()
	switch {
	case c.Blocked > 0:
		return "blocked"
	case c.Down > 0:
		return "down"
	case c.Waiting > 0:
		return "waiting"
	case c.Working > 0:
		return "working"
	default:
		return "idle"
	}
}

// Summary renders counts for tooltips and menus, e.g.
// "2 working · 1 waiting · 1 blocked · 1 offline".
func (v View) Summary() string {
	c := v.Counts()
	var parts []string
	if c.Working > 0 {
		parts = append(parts, plural(c.Working, "working"))
	}
	if c.Waiting > 0 {
		parts = append(parts, plural(c.Waiting, "waiting"))
	}
	if c.Blocked > 0 {
		parts = append(parts, plural(c.Blocked, "blocked"))
	}
	if c.Down > 0 {
		parts = append(parts, plural(c.Down, "offline"))
	}
	if len(parts) == 0 {
		return "all quiet"
	}
	return strings.Join(parts, " · ")
}

func plural(n int, noun string) string {
	if n == 1 {
		return "1 " + noun
	}
	return strconv.Itoa(n) + " " + noun
}

// View returns a sorted, deep copy of the current state. Sessions appear
// once they have had at least one successful snapshot; agents keep their
// last known state while a session is offline.
func (e *Engine) View() View {
	e.mu.Lock()
	defer e.mu.Unlock()
	var v View
	for _, t := range e.sessions {
		if !t.seen {
			continue // never observed: don't show phantom sessions
		}
		sv := SessionView{Host: t.host, Name: t.name, Up: t.up}
		for _, a := range t.agents {
			sv.Agents = append(sv.Agents, AgentView{
				Name:    a.Name,
				Status:  a.Status,
				Title:   a.Title,
				Project: a.Project,
				PaneID:  a.PaneID,
				Focused: a.Focused,
			})
		}
		sort.Slice(sv.Agents, func(i, j int) bool {
			ai, aj := sv.Agents[i], sv.Agents[j]
			if ri, rj := statusRank(ai.Status), statusRank(aj.Status); ri != rj {
				return ri < rj
			}
			return ai.Name < aj.Name
		})
		v.Sessions = append(v.Sessions, sv)
	}
	sort.Slice(v.Sessions, func(i, j int) bool {
		if v.Sessions[i].Host != v.Sessions[j].Host {
			return v.Sessions[i].Host < v.Sessions[j].Host
		}
		return v.Sessions[i].Name < v.Sessions[j].Name
	})
	return v
}

func statusRank(status string) int {
	switch status {
	case herdr.StatusBlocked:
		return 0
	case herdr.StatusWorking:
		return 1
	case herdr.StatusIdle:
		return 2
	case herdr.StatusDone:
		return 3
	default:
		return 4
	}
}
