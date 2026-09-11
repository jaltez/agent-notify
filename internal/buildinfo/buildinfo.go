// Package buildinfo carries release identity. Version is overridden at
// build time via -ldflags (goreleaser / Makefile).
package buildinfo

// Owner and Repo identify the GitHub project releases are published to.
const (
	Owner = "jaltez"
	Repo  = "agent-notify"
)

// Version is "dev" for plain `go build`; release builds inject the tag.
var Version = "dev"

// IsRelease reports whether this binary carries a released version.
func IsRelease() bool { return Version != "dev" }
