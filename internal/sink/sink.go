// Package sink delivers events to notifiers: the tray, desktop popups,
// the terminal bell, arbitrary commands, HTTP webhooks, and log output.
// A Manager fans events out to every registered sink without letting a
// slow one block the engine.
package sink

import (
	"context"
	"log/slog"
	"time"

	"agent-notify/internal/event"
)

// Sink consumes events. Deliver should honor ctx cancellation; the Manager
// bounds every call with a hard timeout anyway.
type Sink interface {
	Name() string
	Deliver(ctx context.Context, ev event.Event) error
}

// deliverTimeout bounds a single Deliver call.
const deliverTimeout = 30 * time.Second

// queue per sink; full queue drops (and logs) rather than blocking.
const queueSize = 16

type registered struct {
	s  Sink
	ch chan event.Event
}

// Manager fans events out to sinks, one worker goroutine each.
type Manager struct {
	log   *slog.Logger
	sinks []registered
}

// NewManager builds an empty Manager.
func NewManager(log *slog.Logger) *Manager {
	return &Manager{log: log}
}

// Add registers a sink and starts its worker. Call before Run.
func (m *Manager) Add(s Sink) {
	m.sinks = append(m.sinks, registered{s: s, ch: make(chan event.Event, queueSize)})
}

// Names lists registered sink names in order.
func (m *Manager) Names() []string {
	out := make([]string, len(m.sinks))
	for i, r := range m.sinks {
		out[i] = r.s.Name()
	}
	return out
}

// Deliver fans one event out to every sink (non-blocking).
func (m *Manager) Deliver(ev event.Event) {
	for _, r := range m.sinks {
		select {
		case r.ch <- ev:
		default:
			m.log.Warn("sink queue full; dropping event", "sink", r.s.Name(), "kind", string(ev.Kind))
		}
	}
}

// Run drives the workers until ctx is done.
func (m *Manager) Run(ctx context.Context) {
	done := make(chan struct{})
	for _, r := range m.sinks {
		go func(r registered) {
			defer func() { done <- struct{}{} }()
			for {
				select {
				case <-ctx.Done():
					return
				case ev := <-r.ch:
					cctx, cancel := context.WithTimeout(ctx, deliverTimeout)
					if err := r.s.Deliver(cctx, ev); err != nil {
						m.log.Warn("sink delivery failed", "sink", r.s.Name(), "kind", string(ev.Kind), "error", err)
					}
					cancel()
				}
			}
		}(r)
	}
	<-ctx.Done()
	for range m.sinks {
		<-done
	}
}
