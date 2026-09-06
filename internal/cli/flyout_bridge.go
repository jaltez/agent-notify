package cli

import (
	"log/slog"

	"agent-notify/internal/engine"
	"agent-notify/internal/tray"
)

func newTrayFlyout(eng *engine.Engine, log *slog.Logger) (*tray.Flyout, error) {
	return tray.NewFlyoutForTest(eng, log)
}
