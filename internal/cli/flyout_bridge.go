package cli

import (
	"log/slog"

	"github.com/jaltez/agent-notify/internal/engine"
	"github.com/jaltez/agent-notify/internal/tray"
)

func newTrayFlyout(eng *engine.Engine, log *slog.Logger) (*tray.Flyout, error) {
	return tray.NewFlyoutForTest(eng, log)
}
