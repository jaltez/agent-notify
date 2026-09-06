// Package proc centralizes subprocess mechanics shared by the herdr
// backends and the popup sink: running commands with annotated errors,
// Windows-interop quirks (UTF-16 output, UNC working-directory warnings),
// and binary lookup.
package proc

import (
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"unicode/utf16"
)

// Run executes argv and returns stdout. Errors carry a trimmed snippet of
// stderr so callers see the real cause (e.g. herdr's "server_not_running").
func Run(ctx context.Context, argv []string, extraEnv []string, dir string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, argv[0], argv[1:]...)
	if len(extraEnv) > 0 {
		cmd.Env = append(os.Environ(), extraEnv...)
	}
	if dir != "" {
		cmd.Dir = dir
	}
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		return nil, annotate(argv[0], err, stderr.String())
	}
	return out, nil
}

func annotate(name string, err error, stderr string) error {
	snip := strings.TrimSpace(stderr)
	if i := strings.Index(snip, "\n"); i > 0 && i < 200 {
		snip = snip[:i]
	}
	if len(snip) > 200 {
		snip = snip[:200]
	}
	if snip == "" {
		return fmt.Errorf("%s: %w", name, err)
	}
	return fmt.Errorf("%s: %w: %s", name, err, snip)
}

// InteropDir returns a Windows-visible working directory ("/mnt/c/Windows"
// under WSL) so Windows executables spawned from WSL do not warn about UNC
// paths. It returns "" when not applicable or unavailable.
func InteropDir() string {
	if runtime.GOOS == "windows" {
		return ""
	}
	for _, d := range []string{"/mnt/c/Windows", "/mnt/c"} {
		if fi, err := os.Stat(d); err == nil && fi.IsDir() {
			return d
		}
	}
	return ""
}

// DecodeUTF16 returns b as UTF-8, decoding UTF-16LE when it smells like it.
// wsl.exe transcodes some outputs to UTF-16; plain UTF-8 (the common case,
// and everything herdr emits) passes through untouched.
func DecodeUTF16(b []byte) []byte {
	if len(b) < 2 {
		return b
	}
	if b[0] == 0xFF && b[1] == 0xFE {
		return decodeUTF16LE(b[2:])
	}
	sample := b
	if len(sample) > 1024 {
		sample = sample[:1024]
	}
	oddZeros := 0
	for i := 1; i < len(sample); i += 2 {
		if b[i] == 0 {
			oddZeros++
		}
	}
	if oddZeros == 0 || oddZeros*3 < len(sample)/2 {
		return b // ordinary UTF-8/ASCII
	}
	return decodeUTF16LE(b)
}

func decodeUTF16LE(b []byte) []byte {
	u16 := make([]uint16, len(b)/2)
	for i := range u16 {
		u16[i] = binary.LittleEndian.Uint16(b[2*i:])
	}
	return []byte(string(utf16.Decode(u16)))
}
