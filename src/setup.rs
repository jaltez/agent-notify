//! Deployment detection and installation: canonical install dir, PATH,
//! autostart, systemd user service, config presence. Port of
//! `internal/setup/setup.go` + `setup_windows.go` / `setup_posix.go` —
//! scaffold: signatures only, bodies `todo!()`.

use crate::config;

/// The systemd user unit name (`setup.ServiceName`).
pub const SERVICE_NAME: &str = "agent-notify";

/// A point-in-time snapshot of the deployment, as shown by
/// `agent-notify setup --status` and the wizard's banner (`setup.Status`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub version: String,
    /// running binary, symlinks resolved.
    pub exe: String,
    /// canonical per-user install directory.
    pub install_dir: String,
    /// canonical binary path ("" when unsupported).
    pub install_exe: String,
    /// running from the canonical install directory.
    pub installed: bool,
    /// install_dir is on PATH.
    pub path_ok: bool,
    /// tray/login autostart registered.
    pub autostart: bool,
    /// systemd user unit installed and enabled.
    pub service_enabled: bool,
    /// systemd user manager usable here.
    pub systemd: bool,
    /// why systemd is unusable, when it is not.
    pub service_hint: String,
    pub config_path: String,
    pub config_exists: bool,
}

impl Status {
    /// Reports whether the first-run nudge should fire: a config file means
    /// deliberate use however the binary is deployed (portable mode is a
    /// choice); otherwise a fresh download or an install missing both
    /// autostart and service looks unfinished (`Status.NeedsSetup`).
    pub fn needs_setup(&self) -> bool {
        let _ = self;
        todo!("port of setup.Status.NeedsSetup (setup.go:65)")
    }

    /// Renders the status as (label, value) rows for the wizard banner
    /// (`Status.Lines`, setup.go:77).
    pub fn lines(&self) -> Vec<(String, String)> {
        let _ = self;
        todo!("port of setup.Status.Lines (setup.go:77)")
    }
}

/// Gathers the current state. It is cheap (filesystem and environment only,
/// no network), so the tray can call it at startup (`setup.Detect`,
/// setup.go:43).
pub fn detect() -> Status {
    let _ = config::path;
    todo!("port of setup.Detect (setup.go:43)")
}

/// Stops the running instance (tray/daemon), installs the binary into the
/// canonical dir, registers autostart + service, and reports whether it
/// had to stop something (`setup.Install`, setup.go:130).
pub fn install() -> anyhow::Result<bool> {
    todo!("port of setup.Install (setup.go:130)")
}

/// Registers the tray autostart entry for the current platform
/// (`setup.EnsureTrayAutostart`, setup.go:166).
pub fn ensure_tray_autostart() -> anyhow::Result<()> {
    todo!("port of setup.EnsureTrayAutostart (setup.go:166)")
}

/// Adds the install dir to the user PATH (`setup.EnsurePATH`,
/// setup.go:171).
pub fn ensure_path() -> anyhow::Result<()> {
    todo!("port of setup.EnsurePATH (setup.go:171)")
}

/// Installs + enables the systemd user unit (Linux/macOS only)
/// (`setup.EnsureService`, setup.go:180).
pub fn ensure_service() -> anyhow::Result<()> {
    todo!("port of setup.EnsureService (setup.go:180)")
}

/// Enables lingering for the systemd user manager (`setup.EnableLinger`,
/// setup.go:187).
pub fn enable_linger() -> anyhow::Result<()> {
    todo!("port of setup.EnableLinger (setup.go:187)")
}

/// Removes the installation; returns the list of cleaned artifacts
/// (`setup.Uninstall`, setup.go:193).
pub fn uninstall(purge_config: bool, config_flag: &str) -> anyhow::Result<Vec<String>> {
    let _ = (purge_config, config_flag);
    todo!("port of setup.Uninstall (setup.go:193)")
}

/// Whether WSL is usable on this machine (Windows only)
/// (`setup.WSLAvailable`, setup.go:228).
pub fn wsl_available() -> bool {
    todo!("port of setup.WSLAvailable (setup.go:228)")
}

/// Installs agent-notify inside a WSL distro (Windows only)
/// (`setup.InstallWSL`, setup.go:233).
pub fn install_wsl(distro: &str, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    let _ = (distro, out);
    todo!("port of setup.InstallWSL (setup.go:233)")
}

/// Starts the installed tray (detached) (`setup.StartInstalled`,
/// setup.go:239).
pub fn start_installed() -> anyhow::Result<()> {
    todo!("port of setup.StartInstalled (setup.go:239)")
}

/// Whether the one-time setup nudge is due (state file + cadence)
/// (`setup.NudgeDue`, setup.go:342). The nudge itself travels as an
/// [`Event`] with kind `Kind::Setup` synthesized by the tray.
pub fn nudge_due() -> bool {
    todo!("port of setup.NudgeDue (setup.go:342)")
}

/// Records that the nudge fired (`setup.MarkNudged`, setup.go:351).
pub fn mark_nudged() -> anyhow::Result<()> {
    todo!("port of setup.MarkNudged (setup.go:351)")
}
