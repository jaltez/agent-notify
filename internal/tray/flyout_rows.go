package tray

import (
	"fmt"
	"strings"

	"github.com/jaltez/agent-notify/internal/engine"
	"github.com/jaltez/agent-notify/internal/icon"
)

// rowKind classifies a flyout row; the Windows painter picks layout,
// height, and interaction from it. The model itself is platform-neutral
// so it can be unit-tested anywhere.
type rowKind uint8

// Panel geometry shared by the row model (the state bar spans the
// inner width) and the Windows painter.
const (
	flyWidth = 420
	flyPad   = 14
)

const (
	rowSummary      rowKind = iota // fleet title line with severity dot
	rowSummarySub                  // counts line with the fleet state bar
	rowSession                     // session header: chevron + counts, click toggles
	rowAgent                       // two-line attention block (dot + status wash)
	rowAgentCompact                // one collapsed-group agent on a single line
	rowGroup                       // "N finished"/"N resting" line, click toggles the group
	rowEmpty                       // placeholder when nothing is known
)

// barSeg is one segment of the fleet state bar (pixel width precomputed).
type barSeg struct {
	col uint32
	px  int32
}

// flyRow is one painted line (or block) of the flyout panel.
type flyRow struct {
	kind   rowKind
	dot    uint32   // COLORREF; 0 = no dot
	text   string   // line 1 (agent blocks: the title)
	sub    string   // line 2, agent blocks only (runner · project)
	dim    bool     // paint in the dim color
	toggle string   // group key this row toggles ("" = inert)
	open   bool     // effective open state, toggle rows only
	bar    []barSeg // state bar segments, rowSummarySub only
}

// colorRef renders an icon color in Windows COLORREF (BGR) layout.
func colorRef(c icon.RGB) uint32 {
	return uint32(c.R) | uint32(c.G)<<8 | uint32(c.B)<<16
}

func pluralS(n int) string {
	if n == 1 {
		return ""
	}
	return "s"
}

func orDash(s string) string {
	if s == "" {
		return "—"
	}
	return s
}

// gdiSafe strips glyphs the flyout's Segoe UI text drawing renders as
// tofu boxes: emoji, pictographs, nerd-font Private Use Area icons,
// variation selectors, and zero-width joiners that herdr titles carry.
// Geometric shapes (chevrons) and normal text pass through.
func gdiSafe(s string) string {
	clean := make([]rune, 0, len(s))
	for _, r := range s {
		switch {
		case r >= 0x1F000 && r <= 0x1FAFF, // emoji & pictographs
			r >= 0x2600 && r <= 0x27BF,    // misc symbols, dingbats
			r >= 0x2B00 && r <= 0x2BFF,    // arrows, stars
			r >= 0xFE00 && r <= 0xFE0F,    // variation selectors
			r >= 0x200B && r <= 0x200F,    // zero-width, bidi marks
			r >= 0xE000 && r <= 0xF8FF,    // BMP private use (nerd fonts)
			r >= 0xF0000 && r <= 0x10FFFD, // supplementary private use
			r == 0x200D, r == 0x20E3:      // ZWJ, keycap
			continue
		}
		clean = append(clean, r)
	}
	return strings.Join(strings.Fields(string(clean)), " ")
}

// buildRows lays out the flyout for a view. Session headers are always
// present; a session expands by default only when it has working or
// blocked agents. Finished (green) and resting agents each collapse
// into a single per-session line; opened groups list their agents as
// compact single-line rows. openState and restOpen hold the user's
// click overrides (session key, and group key "session/finished" or
// "session/resting").
func buildRows(v engine.View, openState, restOpen map[string]bool) []flyRow {
	rows := []flyRow{
		{kind: rowSummary, dot: colorRef(icon.Color(v.Severity())), text: "Agent Notify"},
		{kind: rowSummarySub, text: v.Summary(), dim: true, bar: stateBar(v)},
	}
	if len(v.Sessions) == 0 {
		rows = append(rows, flyRow{kind: rowEmpty, text: "No herdr sessions seen yet; start a herdr session", dim: true})
		return rows
	}
	for _, s := range v.Sessions {
		key := s.Host + "/" + s.Name
		var active, finished, resting []engine.AgentView
		for _, a := range s.Agents {
			switch engine.RowSeverity(a.Status, a.Wants) {
			case "working", "blocked":
				active = append(active, a)
			case "waiting":
				finished = append(finished, a)
			default:
				resting = append(resting, a)
			}
		}
		open := len(active) > 0
		if o, ok := openState[key]; ok {
			open = o
		}
		state := ""
		if !s.Up {
			state = " · offline"
		}
		rows = append(rows, flyRow{
			kind:   rowSession,
			text:   fmt.Sprintf("%s %s/%s — %d agent%s%s", chevron(open), s.Host, s.Name, len(s.Agents), pluralS(len(s.Agents)), tally(s.Agents)+state),
			dim:    len(active) == 0,
			toggle: key,
			open:   open,
		})
		if !open {
			continue
		}
		for _, a := range active {
			title, info := a.Title, a.Name
			if a.Project != "" {
				if title == "" {
					title = a.Project
				} else {
					info += " · " + a.Project
				}
			}
			rows = append(rows, flyRow{
				kind: rowAgent,
				dot:  colorRef(icon.Color(engine.RowSeverity(a.Status, a.Wants))),
				text: orDash(gdiSafe(title)), // line 1: what it is doing
				sub:  gdiSafe(info),          // line 2: runner · project, dim
			})
		}
		rows = appendGroup(rows, key+"/finished", "finished", finished, "waiting", restOpen)
		rows = appendGroup(rows, key+"/resting", "resting", resting, "idle", restOpen)
	}
	return rows
}

// appendGroup adds one collapsible group line and, when the group is
// open, one compact row per agent (dot in the group's severity color).
func appendGroup(rows []flyRow, key, label string, agents []engine.AgentView, sev string, open map[string]bool) []flyRow {
	if len(agents) == 0 {
		return rows
	}
	isOpen := open[key]
	rows = append(rows, flyRow{
		kind:   rowGroup,
		dot:    colorRef(icon.Color(sev)),
		text:   fmt.Sprintf("%s %d %s", chevron(isOpen), len(agents), label),
		dim:    false, // normal weight: collapsibles must outweigh their rows
		toggle: key,
		open:   isOpen,
	})
	if !isOpen {
		return rows
	}
	for _, a := range agents {
		title := a.Title
		if title == "" {
			title = a.Project
		}
		rows = append(rows, flyRow{
			kind: rowAgentCompact,
			dot:  colorRef(icon.Color(sev)),
			text: orDash(gdiSafe(title)),
			dim:  true,
		})
	}
	return rows
}

// chevron is the expand/collapse marker for toggle rows.
func chevron(open bool) string {
	if open {
		return "▾"
	}
	return "▸"
}

// tally summarizes a session's agents for its header, e.g.
// " · 1 working · 1 blocked"; danger first.
func tally(agents []engine.AgentView) string {
	var blocked, working, waiting int
	for _, a := range agents {
		switch engine.RowSeverity(a.Status, a.Wants) {
		case "blocked":
			blocked++
		case "working":
			working++
		case "waiting":
			waiting++
		}
	}
	var out string
	for _, p := range []struct {
		n int
		w string
	}{
		{blocked, "blocked"},
		{working, "working"},
		{waiting, "waiting"},
	} {
		if p.n > 0 {
			out += fmt.Sprintf(" · %d %s", p.n, p.w)
		}
	}
	return out
}

// stateBar splits the panel's inner width across agent states:
// blocked, offline, working, waiting, then resting gray.
func stateBar(v engine.View) []barSeg {
	const inner = flyWidth - 2*flyPad
	var blocked, down, working, waiting, total int
	for _, s := range v.Sessions {
		if !s.Up {
			down++
			continue
		}
		for _, a := range s.Agents {
			total++
			switch engine.RowSeverity(a.Status, a.Wants) {
			case "blocked":
				blocked++
			case "working":
				working++
			case "waiting":
				waiting++
			}
		}
	}
	if total == 0 {
		return nil
	}
	idle := total - blocked - working - waiting
	parts := []struct {
		n int
		c uint32
	}{
		{blocked, colorRef(icon.Color("blocked"))},
		{down, colorRef(icon.Color("down"))},
		{working, colorRef(icon.Color("working"))},
		{waiting, colorRef(icon.Color("waiting"))},
		{idle, colorRef(icon.Color("idle"))},
	}
	segs := make([]barSeg, 0, len(parts))
	var used int32
	for _, p := range parts {
		if p.n == 0 {
			continue
		}
		px := int32(float64(inner) * float64(p.n) / float64(total))
		if px < 1 {
			px = 1
		}
		segs = append(segs, barSeg{col: p.c, px: px})
		used += px
	}
	segs[len(segs)-1].px += inner - used // last segment absorbs rounding
	return segs
}
