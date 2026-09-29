//go:build linux

package cli

import (
	"os"
	"syscall"
	"unsafe"
)

// isTerminal reports whether stdin is an interactive TTY. A plain
// ModeCharDevice check is not enough: /dev/null is a character device
// too, and a redirected-from-/dev/null wizard must not start prompting.
func isTerminal() bool {
	var termios syscall.Termios
	_, _, errno := syscall.Syscall6(syscall.SYS_IOCTL, os.Stdin.Fd(),
		syscall.TCGETS, uintptr(unsafe.Pointer(&termios)), 0, 0, 0)
	return errno == 0
}
