//go:build windows

package cli

import (
	"os"

	"golang.org/x/sys/windows"
)

// isTerminal reports whether stdin is an interactive console.
func isTerminal() bool {
	var mode uint32
	return windows.GetConsoleMode(windows.Handle(os.Stdin.Fd()), &mode) == nil
}
