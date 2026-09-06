//go:build windows

package proc

import (
	"syscall"

	"golang.org/x/sys/windows"
)

// creationFlags hide child console windows: the tray runs as a
// windowsgui process with no console, so without CREATE_NO_WINDOW every
// spawned CLI (herdr.exe, wsl.exe, powershell.exe) would allocate and
// flash a new console window.
const childCreationFlags = windows.CREATE_NO_WINDOW

// childSysProcAttr returns the SysProcAttr all children must use on Windows.
func childSysProcAttr() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: childCreationFlags,
	}
}
