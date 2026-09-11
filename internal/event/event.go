// Package event defines the normalized event model produced by sources
// (herdr today, other agent runners later) and consumed by sinks (tray,
// popups, webhooks, ...).
package event

import "time"

// Kind enumerates the event types agent-notify can emit.
type Kind string

const (
	// Attention events: an agent stopped working and probably wants a human.
	KindAgentIdle    Kind = "agent_idle"    // working → idle
	KindAgentDone    Kind = "agent_done"    // working → done
	KindAgentBlocked Kind = "agent_blocked" // anything → blocked

	// Non-attention kinds, opt-in via the "events" config key.
	KindAgentWorking Kind = "agent_working" // anything → working
	KindAgentSpawned Kind = "agent_spawned" // new agent pane appeared
	KindAgentLeft    Kind = "agent_left"    // agent pane disappeared
	KindSessionDown  Kind = "session_down"  // session stopped responding
	KindSessionUp    Kind = "session_up"    // session responded again

	// KindTest is synthesized by `agent-notify test` and the tray menu.
	KindTest Kind = "test"
	// KindUpdateAvail is synthesized by the background update check.
	KindUpdateAvail Kind = "update_available"
)

// Attention is the default event set: the transitions where an agent stopped
// and wants a human (idle, done, blocked).
var Attention = []Kind{KindAgentIdle, KindAgentDone, KindAgentBlocked}

// All lists every emittable kind.
var All = []Kind{
	KindAgentIdle, KindAgentDone, KindAgentBlocked,
	KindAgentWorking, KindAgentSpawned, KindAgentLeft,
	KindSessionDown, KindSessionUp, KindTest, KindUpdateAvail,
}

var verbs = map[Kind]string{
	KindAgentIdle:    "went idle",
	KindAgentDone:    "finished",
	KindAgentBlocked: "is blocked",
	KindAgentWorking: "is working",
	KindAgentSpawned: "joined",
	KindAgentLeft:    "left",
	KindSessionDown:  "went offline",
	KindSessionUp:    "is back online",
	KindTest:         "test notification",
	KindUpdateAvail:  "update available",
}

// Verb returns a short human phrase for the kind, usable in templates.
func (k Kind) Verb() string {
	if v, ok := verbs[k]; ok {
		return v
	}
	return string(k)
}

// Event is one observed state transition on a monitored session.
type Event struct {
	Kind    Kind      `json:"kind"`
	Time    time.Time `json:"time"`
	Source  string    `json:"source"`            // emitting source, e.g. "herdr"
	Host    string    `json:"host"`              // backend: local | windows | wsl
	Session string    `json:"session"`           // herdr session name
	Agent   string    `json:"agent,omitempty"`   // agent runner name, e.g. "claude"
	From    string    `json:"from,omitempty"`    // previous status
	To      string    `json:"to,omitempty"`      // new status
	Title   string    `json:"title,omitempty"`   // agent terminal title
	Project string    `json:"project,omitempty"` // cwd basename
	PaneID  string    `json:"pane_id,omitempty"`
	Focused bool      `json:"focused,omitempty"`
}

// Verb returns the human phrase for the event kind.
func (e Event) Verb() string { return e.Kind.Verb() }

// ParseSet expands a configured event list into concrete kinds. It accepts
// the pseudo-sets "attention" (default), "all", and "none" plus any kind
// names. A nil or empty list means the default attention set.
func ParseSet(names []string) ([]Kind, error) {
	if len(names) == 0 {
		return append([]Kind(nil), Attention...), nil
	}
	var out []Kind
	seen := map[Kind]bool{}
	add := func(ks ...Kind) error {
		for _, k := range ks {
			if !seen[k] {
				seen[k] = true
				out = append(out, k)
			}
		}
		return nil
	}
	for _, n := range names {
		switch n {
		case "attention":
			add(Attention...)
		case "all":
			add(All...)
		case "none":
			// no kinds
		default:
			k := Kind(n)
			known := false
			for _, cand := range All {
				if cand == k {
					known = true
					break
				}
			}
			if !known {
				return nil, &UnknownKindError{Kind: n, All: Names(All)}
			}
			add(k)
		}
	}
	return out, nil
}

// Names maps kinds back to their string form.
func Names(ks []Kind) []string {
	out := make([]string, len(ks))
	for i, k := range ks {
		out[i] = string(k)
	}
	return out
}

// UnknownKindError reports an event name that is not a known kind.
type UnknownKindError struct {
	Kind string
	All  []string
}

func (e *UnknownKindError) Error() string {
	return "unknown event kind " + e.Kind + " (known: " + joinComma(e.All) + ", attention, all, none)"
}

func joinComma(ss []string) string {
	out := ""
	for i, s := range ss {
		if i > 0 {
			out += ", "
		}
		out += s
	}
	return out
}
