//go:build !windows

package cli

// attachParentConsole is only meaningful for windowsgui builds.
func attachParentConsole() {}
