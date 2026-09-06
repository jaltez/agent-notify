// agent-notify — tray notifier for AI coding agents (herdr today).
package main

import (
	"os"

	"agent-notify/internal/cli"
)

func main() {
	os.Exit(cli.Run(os.Args, os.Stdout, os.Stderr))
}
