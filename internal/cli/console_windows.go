//go:build windows

package cli

import (
	"os"

	"golang.org/x/sys/windows"
)

const attachParentProcess = ^uintptr(0) // ATTACH_PARENT_PROCESS

var kernel32 = windows.NewLazySystemDLL("kernel32.dll")
var procAttachConsole = kernel32.NewProc("AttachConsole")
var procGetStdHandle = kernel32.NewProc("GetStdHandle")

// attachParentConsole lets a windowsgui-subsystem binary print to the
// terminal it was launched from (agent-notify probe/test/... from cmd or
// PowerShell). A windowless launch (Explorer double-click) has no parent
// console and the call fails silently — the tray starts with no window.
// Under WSL interop the std pipes are inherited directly and the call is
// simply a no-op.
func attachParentConsole() {
	if r, _, _ := procAttachConsole.Call(attachParentProcess); r == 0 {
		return
	}
	rebind := []struct {
		std uintptr
		f   **os.File
	}{
		{uintptr(windows.STD_OUTPUT_HANDLE) & 0xffffffff, &os.Stdout},
		{uintptr(windows.STD_ERROR_HANDLE) & 0xffffffff, &os.Stderr},
		{uintptr(windows.STD_INPUT_HANDLE) & 0xffffffff, &os.Stdin},
	}
	for _, r := range rebind {
		h, _, _ := procGetStdHandle.Call(r.std)
		if h != 0 && h != ^uintptr(0) {
			*r.f = os.NewFile(h, "console")
		}
	}
}
