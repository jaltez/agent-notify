//go:build windows

package cli

import (
	"fmt"
	"syscall"

	"golang.org/x/sys/windows"
)

// acquireTrayLock prevents multiple tray instances: a second exe launch
// would otherwise double every toast. Returns a release func that keeps
// the mutex alive for the process duration.
func acquireTrayLock() (func(), error) {
	name, err := windows.UTF16PtrFromString(`Local\agent-notify-tray`)
	if err != nil {
		return nil, err
	}
	handle, err := windows.CreateMutex(nil, false, name)
	if err == syscall.ERROR_ALREADY_EXISTS {
		return nil, fmt.Errorf("another agent-notify tray is already running")
	}
	if err != nil && err != syscall.ERROR_ALREADY_EXISTS {
		return nil, fmt.Errorf("create mutex: %w", err)
	}
	return func() { _ = handle }, nil
}
