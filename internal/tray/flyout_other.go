//go:build !windows

package tray

import (
	"log/slog"

	"agent-notify/internal/engine"
)

// The flyout panel is a Windows-only UI; elsewhere the tray menu is the
// detail surface.
type flyout struct{}

// Flyout is the exported handle used by diagnostics (agent-notify flytest).
type Flyout = flyout

func newFlyout(*engine.Engine, *slog.Logger) (*flyout, error) { return nil, nil }

func (f *flyout) debugf(string, ...any) {}

func (f *flyout) toggle()   {}
func (f *flyout) notify()   {}
func (f *flyout) SelfTest() {}
