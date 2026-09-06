//go:build !windows

package tray

import "agent-notify/internal/engine"

// The flyout panel is a Windows-only UI; elsewhere the tray menu is the
// detail surface.
type flyout struct{}

func newFlyout(*engine.Engine) *flyout { return nil }

func (f *flyout) toggle() {}
func (f *flyout) notify() {}
