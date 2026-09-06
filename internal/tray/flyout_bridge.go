package tray

import (
	"log/slog"

	"agent-notify/internal/engine"
)

// NewFlyoutForTest exposes the platform flyout constructor to the CLI
// (diagnostics only).
func NewFlyoutForTest(eng *engine.Engine, log *slog.Logger) (*flyout, error) {
	return newFlyout(eng, log)
}
