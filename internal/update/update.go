// Package update implements self-update against the project's GitHub
// releases: version check, checksum-verified download, binary swap, and
// restart of the running process.
package update

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"time"

	"github.com/Masterminds/semver/v3"
	"github.com/creativeprojects/go-selfupdate"

	"github.com/jaltez/agent-notify/internal/buildinfo"
)

// repo is the GitHub project releases are published to.
var repo = selfupdate.NewRepositorySlug(buildinfo.Owner, buildinfo.Repo)

// Checker wraps the GitHub releases source.
type Checker struct {
	updater *selfupdate.Updater
}

// New builds a Checker verifying assets against goreleaser's
// checksums.txt.
func New() (*Checker, error) {
	u, err := selfupdate.NewUpdater(selfupdate.Config{
		Validator: &selfupdate.ChecksumValidator{UniqueFilename: "checksums.txt"},
	})
	if err != nil {
		return nil, err
	}
	return &Checker{updater: u}, nil
}

// Status is the outcome of a version check.
type Status struct {
	Current     string
	Latest      string
	UpdateAvail bool
	Release     *selfupdate.Release
}

// Check compares the running build against the latest release.
// Development builds ("dev") never report an update — they only show the
// latest published version.
func (c *Checker) Check(ctx context.Context) (Status, error) {
	var st Status
	st.Current = buildinfo.Version
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	rel, found, err := c.updater.DetectLatest(ctx, repo)
	if err != nil {
		return st, err
	}
	if !found || rel == nil {
		st.Latest = "none"
		return st, nil
	}
	st.Latest = rel.Version()
	st.Release = rel
	if !buildinfo.IsRelease() {
		return st, nil
	}
	if cur, err := semver.NewVersion(buildinfo.Version); err == nil {
		st.UpdateAvail = rel.GreaterThan(cur.String())
	}
	return st, nil
}

// Apply downloads the release asset, verifies its checksum and swaps the
// running binary in place (on Windows the running exe is renamed aside).
func (c *Checker) Apply(ctx context.Context, rel *selfupdate.Release) error {
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	return c.updater.UpdateTo(ctx, rel, exe)
}

// RestartSelf launches the (already swapped) binary as a detached process
// and returns; the caller should exit right after.
func RestartSelf() error {
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	return exec.Command(exe).Start()
}

// Describe renders a Status for humans.
func (s Status) Describe() string {
	if s.Latest == "none" {
		return fmt.Sprintf("current %s — no releases published", s.Current)
	}
	if !buildinfo.IsRelease() {
		return fmt.Sprintf("development build — latest release is %s", s.Latest)
	}
	if s.UpdateAvail {
		return fmt.Sprintf("update available: %s → %s", s.Current, s.Latest)
	}
	return fmt.Sprintf("up to date (%s)", s.Current)
}
