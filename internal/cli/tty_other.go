//go:build !linux && !windows

package cli

import "os"

// isTerminal falls back to a character-device check where no ioctl
// constant is portable; agent-notify ships no binaries for these
// platforms today.
func isTerminal() bool {
	fi, err := os.Stdin.Stat()
	return err == nil && fi.Mode()&os.ModeCharDevice != 0
}
