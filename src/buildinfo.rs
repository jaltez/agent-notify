//! Release identity. Port of `internal/buildinfo/buildinfo.go`.
//!
//! `VERSION` is `"dev"` for a plain `cargo build`; release builds inject the
//! tag at compile time (the Go side uses `-ldflags -X` via goreleaser/Makefile):
//!
//! ```sh
//! AGENT_NOTIFY_VERSION=v0.6.0 cargo build --release
//! ```

/// Owner half of the GitHub project releases are published to.
pub const OWNER: &str = "jaltez";

/// Repo half of the GitHub project releases are published to.
pub const REPO: &str = "agent-notify";

/// `"dev"` for plain builds; release builds override via
/// `AGENT_NOTIFY_VERSION` at compile time (Go: `-ldflags -X`).
pub const VERSION: &str = match option_env!("AGENT_NOTIFY_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// Reports whether this binary carries a released version.
pub fn is_release() -> bool {
    !VERSION.eq("dev")
}
