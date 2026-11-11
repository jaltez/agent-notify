//! GitHub-releases self-update. Port of `internal/update/update.go` —
//! scaffold: signatures only, bodies `todo!()`.
//!
//! Go uses `creativeprojects/go-selfupdate` with a checksums.txt validator;
//! the Rust port implements the same flow over `ureq` + `sha2` + `hex` +
//! `flate2`/`tar`/`zip` (deps already in Cargo.toml): list releases from
//! `https://api.github.com/repos/{OWNER}/{REPO}/releases`, verify the
//! asset checksum, swap the running binary.

use crate::buildinfo::{self, OWNER, REPO};

/// A published release, as far as the updater needs it
/// (`selfupdate.Release` reduced to what update.go touches).
#[derive(Clone, Debug, Default)]
pub struct Release {
    /// release tag, e.g. "v0.5.0".
    pub version: String,
    /// the asset for the running platform.
    pub asset_url: String,
}

/// The outcome of a version check (`update.Status`).
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub current: String,
    pub latest: String,
    pub update_avail: bool,
    /// set when a release was found, for [`Checker::apply`].
    pub release: Option<Release>,
}

impl Status {
    /// Renders a Status for humans (`Status.Describe`, update.go:95).
    pub fn describe(&self) -> String {
        let _ = self;
        todo!("port of update.Status.Describe (update.go:95)")
    }
}

/// Talks to the GitHub releases source (`update.Checker`).
pub struct Checker;

impl Checker {
    /// Builds a Checker verifying assets against goreleaser's checksums.txt
    /// (`update.New`, update.go:29).
    pub fn new() -> anyhow::Result<Checker> {
        let _ = (OWNER, REPO);
        todo!("port of update.New (update.go:29)")
    }

    /// Compares the running build against the latest release (`Checker.Check`,
    /// update.go:50). Development builds ("dev") never report an update; they
    /// only show the latest published version.
    pub fn check(&self, timeout: Duration) -> anyhow::Result<Status> {
        let _ = timeout;
        todo!("port of update.Checker.Check (update.go:50)")
    }

    /// Downloads the release asset, verifies its checksum and swaps the
    /// running binary in place (on Windows the running exe is renamed
    /// aside) (`Checker.Apply`, update.go:76).
    pub fn apply(&self, release: &Release) -> anyhow::Result<()> {
        let _ = release;
        todo!("port of update.Checker.Apply (update.go:76)")
    }
}

/// Launches the (already swapped) binary as a detached process and returns;
/// the caller should exit right after (`update.RestartSelf`, update.go:86).
pub fn restart_self() -> anyhow::Result<()> {
    let _ = buildinfo::VERSION;
    todo!("port of update.RestartSelf (update.go:86)")
}

use std::time::Duration;
