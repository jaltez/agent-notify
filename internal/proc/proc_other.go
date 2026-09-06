//go:build !windows

package proc

import "syscall"

// childSysProcAttr is a no-op off Windows.
func childSysProcAttr() *syscall.SysProcAttr { return nil }
