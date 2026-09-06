// Package tray renders the agent-notify system-tray UI: an icon whose
// color follows the most severe live state, a tooltip summary, and a
// per-session/per-agent menu rebuilt on change. Attention popups are
// delegated to the popup sink, which the Tray forwards events to.
package tray

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	"fyne.io/systray"

	"agent-notify/internal/engine"
	"agent-notify/internal/event"
	"agent-notify/internal/sink"
)

const (
	iconSize       = 32
	maxAgentsShown = 15
	debounce       = 300 * time.Millisecond
)

// Tray is both a sink (event → popup) and a live view renderer.
type Tray struct {
	eng *engine.Engine
	pop *sink.Popup // nil = silent icon-only mode
	log *slog.Logger
}

// New builds the tray sink.
func New(eng *engine.Engine, pop *sink.Popup, log *slog.Logger) *Tray {
	return &Tray{eng: eng, pop: pop, log: log}
}

// Name implements sink.Sink.
func (t *Tray) Name() string { return "tray" }

// Deliver forwards events to the popup sink (if enabled).
func (t *Tray) Deliver(ctx context.Context, ev event.Event) error {
	if t.pop == nil {
		return nil
	}
	return t.pop.Deliver(ctx, ev)
}

// Run blocks running the tray UI; call from the main goroutine. It returns
// after the user quits (or ctx is done), via systray.Quit().
func (t *Tray) Run(ctx context.Context, onTest func()) {
	systray.Run(
		func() { t.onReady(ctx, onTest) },
		func() {},
	)
}

func (t *Tray) onReady(ctx context.Context, onTest func()) {
	systray.SetIcon(IconBytes("idle", iconSize))
	systray.SetTooltip("agent-notify — starting…")
	t.buildMenu(engine.View{}, onTest)
	go t.refreshLoop(ctx, onTest)
}

// refreshLoop renders the view on state changes, debouncing bursts.
func (t *Tray) refreshLoop(ctx context.Context, onTest func()) {
	for {
		select {
		case <-ctx.Done():
			systray.Quit()
			return
		case <-t.eng.StateChange():
		}
		timer := time.NewTimer(debounce)
	wait:
		for {
			select {
			case <-t.eng.StateChange():
				timer.Reset(debounce)
			case <-timer.C:
				break wait
			case <-ctx.Done():
				systray.Quit()
				return
			}
		}
		t.render(onTest)
	}
}

func (t *Tray) render(onTest func()) {
	v := t.eng.View()
	systray.SetIcon(IconBytes(v.Severity(), iconSize))
	systray.SetTooltip("agent-notify — " + v.Summary())
	t.buildMenu(v, onTest)
}

// buildMenu (re)creates the whole menu. ResetMenu closes removed items'
// ClickedCh channels, so the per-item handler goroutines exit cleanly.
func (t *Tray) buildMenu(v engine.View, onTest func()) {
	systray.ResetMenu()

	summary := systray.AddMenuItem(v.Summary(), "Live agent status")
	summary.Disable()
	systray.AddSeparator()

	if len(v.Sessions) == 0 {
		m := systray.AddMenuItem("No sessions yet", "Waiting for herdr sessions to appear")
		m.Disable()
	}
	for _, s := range v.Sessions {
		label := fmt.Sprintf("%s/%s", s.Host, s.Name)
		if !s.Up {
			label += " · offline"
		}
		hdr := systray.AddMenuItem(label, "herdr session")
		hdr.Disable()
		for i, a := range s.Agents {
			if i >= maxAgentsShown {
				more := systray.AddMenuItem(fmt.Sprintf("… and %d more", len(s.Agents)-maxAgentsShown), "")
				more.Disable()
				break
			}
			item := hdr.AddSubMenuItem(agentLabel(a), a.Title)
			item.Disable()
		}
		if len(s.Agents) == 0 && s.Up {
			empty := hdr.AddSubMenuItem("(no agents)", "")
			empty.Disable()
		}
	}

	systray.AddSeparator()
	if onTest != nil {
		tm := systray.AddMenuItem("Test notification", "Send a test event through the sinks")
		go func() {
			for range tm.ClickedCh {
				onTest()
			}
		}()
	}
	quit := systray.AddMenuItem("Quit", "Stop agent-notify")
	go func() {
		for range quit.ClickedCh {
			systray.Quit()
		}
	}()
}

func agentLabel(a engine.AgentView) string {
	status := a.Status
	if status == "" {
		status = "unknown"
	}
	label := status + " · " + a.Name
	if a.Project != "" {
		label += " — " + a.Project
	}
	if a.Focused {
		label += " ✱"
	}
	return label
}
