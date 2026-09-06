// agent-notify — tray notifier for AI coding agents (herdr today).
package main

import (
	"os"
	"runtime"

	"agent-notify/internal/cli"
)

func main() {
	// The tray and flyout windows live on the main OS thread's message
	// queue; pin the thread before any window is created.
	runtime.LockOSThread()
	os.Exit(cli.Run(os.Args, os.Stdout, os.Stderr))
}
