// Package herdr implements the herdr source: session discovery and
// snapshot polling across three backends —
//
//   - local:   native Unix sockets at ~/.config/herdr (Linux, WSL, macOS)
//   - windows: herdr.exe sessions (native Windows, or the Windows side
//     polled from WSL via interop)
//   - wsl:     sessions inside a WSL distro, polled from a Windows-native
//     process via wsl.exe
package herdr

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/jaltez/agent-notify/internal/proc"
)

// Agent statuses reported by herdr's snapshot API.
const (
	StatusIdle    = "idle"
	StatusWorking = "working"
	StatusBlocked = "blocked"
	StatusDone    = "done"
	StatusUnknown = "unknown"
)

// Agent is one monitored agent pane, normalized across sources.
type Agent struct {
	PaneID  string
	Name    string
	Status  string
	Title   string
	Project string
	Focused bool
}

// Snapshot is a point-in-time view of one session.
type Snapshot struct {
	Agents []Agent
}

// Session identifies one herdr session on one backend. Ref is the
// backend-specific handle (socket path or session name); Home is the WSL
// $HOME used to build PATH entries inside the distro.
type Session struct {
	Host string
	Name string
	Ref  string
	Home string
}

// Backend discovers sessions and fetches their snapshots.
type Backend interface {
	Name() string
	Discover(ctx context.Context) ([]Session, error)
	Fetch(ctx context.Context, s Session) (*Snapshot, error)
	// Rescan is how often Discover should be re-run; zero means every poll.
	Rescan() time.Duration
}

// ParseSnapshot decodes herdr CLI JSON, peeling the response envelopes
// ({"id": ..., "result": {"snapshot": {...}}}) and normalizing agents.
func ParseSnapshot(data []byte) (*Snapshot, error) {
	data = proc.DecodeUTF16(data)
	var doc map[string]any
	if err := json.Unmarshal(data, &doc); err != nil {
		return nil, fmt.Errorf("parse snapshot JSON: %w", err)
	}
	doc = unwrap(doc)
	snap := &Snapshot{}
	raw, _ := doc["agents"].([]any)
	for _, ra := range raw {
		m, ok := ra.(map[string]any)
		if !ok {
			continue
		}
		snap.Agents = append(snap.Agents, Agent{
			Name:    str(m, "agent", "display_agent"),
			Status:  NormalizeStatus(str(m, "agent_status")),
			Title:   str(m, "terminal_title_stripped", "terminal_title"),
			Project: projectOf(str(m, "foreground_cwd"), str(m, "cwd")),
			PaneID:  str(m, "pane_id"),
			Focused: m["focused"] == true,
		})
	}
	return snap, nil
}

// unwrap walks the CLI envelope keys result/snapshot until the payload.
func unwrap(doc map[string]any) map[string]any {
	for {
		found := false
		for _, key := range []string{"snapshot", "result"} {
			if inner, ok := doc[key].(map[string]any); ok {
				doc, found = inner, true
				break
			}
		}
		if !found {
			return doc
		}
	}
}

// NormalizeStatus maps a raw status string onto the known set.
func NormalizeStatus(s string) string {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case StatusIdle:
		return StatusIdle
	case StatusWorking:
		return StatusWorking
	case StatusBlocked:
		return StatusBlocked
	case StatusDone:
		return StatusDone
	default:
		return StatusUnknown
	}
}

func str(m map[string]any, keys ...string) string {
	for _, k := range keys {
		if v, ok := m[k].(string); ok && v != "" {
			return v
		}
	}
	return ""
}

// projectOf returns the basename of the first non-empty path, tolerating
// Windows separators.
func projectOf(paths ...string) string {
	for _, p := range paths {
		p = strings.TrimRight(strings.ReplaceAll(p, "\\", "/"), "/")
		if p == "" {
			continue
		}
		if i := strings.LastIndex(p, "/"); i >= 0 {
			p = p[i+1:]
		}
		return p
	}
	return ""
}
