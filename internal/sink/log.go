package sink

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"sync"

	"agent-notify/internal/event"
)

// Log prints one line per event to a writer — stdout for `monitor`,
// stderr as a config sink.
type Log struct {
	W    io.Writer
	JSON bool

	mu sync.Mutex
}

// NewLog builds the log sink.
func NewLog(w io.Writer, jsonMode bool) *Log {
	return &Log{W: w, JSON: jsonMode}
}

func (l *Log) Name() string { return "log" }

func (l *Log) Deliver(ctx context.Context, ev event.Event) error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.JSON {
		data, err := json.Marshal(ev)
		if err != nil {
			return err
		}
		_, err = fmt.Fprintln(l.W, string(data))
		return err
	}
	arrow := ""
	if ev.From != "" || ev.To != "" {
		arrow = ev.From + "→" + ev.To
	}
	_, err := fmt.Fprintf(l.W, "%s  %-16s %-14s %-8s %-18s %s\n",
		ev.Time.Format("15:04:05"),
		ev.Host+"/"+ev.Session,
		string(ev.Kind),
		ev.Agent,
		arrow,
		ev.Title,
	)
	return err
}

// Bell writes an ASCII BEL to a writer on every event.
type Bell struct {
	W io.Writer
}

// NewBell builds the bell sink.
func NewBell(w io.Writer) *Bell { return &Bell{W: w} }

func (b *Bell) Name() string { return "bell" }

func (b *Bell) Deliver(ctx context.Context, ev event.Event) error {
	_, err := io.WriteString(b.W, "\a")
	return err
}
