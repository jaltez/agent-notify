//! Deployment detection and installation: canonical install dir, PATH,
//! autostart, systemd user service, config presence. Port of
//! `internal/setup/setup.go`; the per-platform halves live in
//! [`posix`] (`setup_posix.go`) and [`windows`] (`setup_windows.go`),
//! cfg-gated like the Go build tags. Templates are `include_str!`-ed from
//! this directory, mirroring the Go `go:embed`.

#[cfg(not(target_os = "windows"))]
mod posix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(target_os = "windows"))]
pub(crate) use posix::*;
#[cfg(target_os = "windows")]
pub(crate) use windows::*;

use crate::config;
use std::path::{Path, PathBuf};

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
    /// autostart and service looks unfinished (`Status.NeedsSetup`,
    /// setup.go:65).
    pub fn needs_setup(&self) -> bool {
        if self.config_exists {
            return false;
        }
        if !self.installed {
            return true;
        }
        !self.autostart && !self.service_enabled
    }

    /// Renders the status as (label, value) rows for the wizard banner
    /// (`Status.Lines`, setup.go:77).
    pub fn lines(&self) -> Vec<(String, String)> {
        let yn = |b: bool, yes: &str| -> String {
            if b {
                if yes.is_empty() {
                    "yes".to_string()
                } else {
                    yes.to_string()
                }
            } else {
                "no".to_string()
            }
        };
        let mut rows: Vec<(String, String)> = vec![
            ("version".into(), self.version.clone()),
            ("binary".into(), self.exe.clone()),
        ];
        if self.install_dir.is_empty() {
            rows.push(("install".into(), "unsupported on this platform".into()));
            return rows;
        }
        let state = if self.installed { "installed" } else { "not installed" };
        rows.push((
            "install".into(),
            format!("{} ({})", self.install_dir, state),
        ));
        if self.path_ok {
            rows.push(("PATH".into(), "ok".into()));
        } else {
            rows.push(("PATH".into(), format!("{} missing", self.install_dir)));
        }
        if self.autostart {
            rows.push(("autostart".into(), platform_autostart_desc()));
        } else {
            rows.push(("autostart".into(), "none".into()));
        }
        if self.service_enabled {
            rows.push(("service".into(), "enabled (systemd user unit)".into()));
        } else if self.systemd {
            rows.push(("service".into(), yn(false, "")));
        } else {
            rows.push(("service".into(), "n/a (no systemd)".into()));
        }
        let cfg = if self.config_exists {
            self.config_path.clone()
        } else {
            format!("{} (missing)", self.config_path)
        };
        rows.push(("config".into(), cfg));
        rows
    }
}

/// Gathers the current state. It is cheap (filesystem and environment only,
/// no network), so the tray can call it at startup (`setup.Detect`,
/// setup.go:43).
pub fn detect() -> Status {
    let mut st = Status {
        version: crate::buildinfo::VERSION.to_string(),
        exe: self_exe(),
        ..Status::default()
    };
    if let Ok(dir) = platform_install_dir() {
        if !dir.is_empty() {
            st.install_dir = dir.clone();
            st.install_exe = install_dir_join(&dir, install_exe_name());
            st.installed = same_path(&go_dir(&st.exe), &dir);
            st.path_ok = platform_path_ok(&dir);
        }
    }
    st.autostart = platform_tray_autostart_installed();
    let (systemd, hint) = platform_systemd();
    st.systemd = systemd;
    st.service_hint = hint;
    st.service_enabled = st.systemd && platform_service_enabled();
    if let Some(p) = config::path("") {
        st.config_path = p.to_string_lossy().into_owned();
        st.config_exists = file_exists(&p);
    }
    st
}

/// Stops the running instance (tray/daemon), installs the binary into the
/// canonical dir (a tray or daemon already running from the target is
/// stopped first: Windows locks a running exe) and restarts an enabled
/// service. Reports whether anything was stopped (`setup.Install`,
/// setup.go:130).
pub fn install() -> anyhow::Result<bool> {
    let exe = self_exe();
    if exe.is_empty() {
        anyhow::bail!("cannot locate the running binary");
    }
    let dir = platform_install_dir()
        .ok()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no canonical install location on this platform"))?;
    let stopped = platform_stop_instances(&exe);
    mkdir_all(Path::new(&dir), 0o755)?;
    // Canonical name, even when the download was renamed: autostart, the
    // service unit and the upgrader all expect agent-notify[.exe].
    let mut dst_name = install_exe_name().to_string();
    if cfg!(windows) && go_base(&exe) == console_exe_name() {
        dst_name = console_exe_name().to_string();
    }
    copy_file(&exe, &install_dir_join(&dir, &dst_name))?;
    for sibling in sibling_exes(&exe) {
        copy_file(&sibling, &install_dir_join(&dir, &go_base(&sibling)))?;
    }
    // A running service would keep serving from the old binary image.
    if platform_service_enabled() {
        let _ = platform_restart_service();
    }
    Ok(stopped)
}

/// Registers the tray autostart entry for the current platform
/// (`setup.EnsureTrayAutostart`, setup.go:166).
pub fn ensure_tray_autostart() -> anyhow::Result<()> {
    let target = autostart_target();
    platform_create_tray_autostart(&target)
}

/// Adds the install dir to the user PATH (`setup.EnsurePATH`, setup.go:171).
pub fn ensure_path() -> anyhow::Result<()> {
    let st = detect();
    if st.install_dir.is_empty() {
        anyhow::bail!("no install directory on this platform");
    }
    platform_add_to_path(&st.install_dir)
}

/// Installs + enables the systemd user unit (`setup.EnsureService`,
/// setup.go:180).
pub fn ensure_service() -> anyhow::Result<()> {
    let target = autostart_target();
    platform_install_service(&target)
}

/// Enables lingering for the systemd user manager. Offered, never forced:
/// it needs the user's own polkit grant on some systems
/// (`setup.EnableLinger`, setup.go:187).
pub fn enable_linger() -> anyhow::Result<()> {
    platform_enable_linger()
}

/// Removes the installation; returns the list of cleaned artifacts
/// (`setup.Uninstall`, setup.go:193). The Go signature is
/// `([]string, error)` — notes are printed even when the flow fails — so
/// the port returns the tuple `(notes, result)` instead of folding the
/// notes into the error.
pub fn uninstall(purge_config: bool, config_flag: &str) -> (Vec<String>, anyhow::Result<()>) {
    let mut notes = Vec::new();
    let st = detect();
    if st.service_enabled {
        if let Err(e) = platform_remove_service() {
            return (notes, Err(e));
        }
        notes.push("disabled and removed the systemd user service".to_string());
    }
    if st.autostart {
        if let Err(e) = platform_remove_tray_autostart() {
            return (notes, Err(e));
        }
        notes.push("removed login autostart".to_string());
    }
    let exe = self_exe();
    platform_stop_instances(&exe);
    if !st.install_dir.is_empty() {
        notes.extend(platform_remove_installed(&st));
    }
    if purge_config {
        if let Some(p) = config::path(config_flag) {
            if file_exists(&p) {
                if let Err(e) = std::fs::remove_file(&p) {
                    return (notes, Err(e.into()));
                }
                notes.push(format!("removed {}", p.display()));
            }
        }
        if let Ok(p) = state_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    notes.push(format!(
        "PATH entries were left in place; remove {} from PATH manually if desired",
        st.install_dir
    ));
    (notes, Ok(()))
}

/// Whether WSL is usable on this machine (Windows only)
/// (`setup.WSLAvailable`, setup.go:228).
pub fn wsl_available() -> bool {
    platform_wsl_available()
}

/// Installs agent-notify inside a WSL distro (Windows only), streaming the
/// install script's output into `out`. Empty distro means wsl.exe's
/// default (`setup.InstallWSL`, setup.go:233).
pub fn install_wsl(distro: &str, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    platform_install_wsl(distro, out)
}

/// Starts the installed tray (detached), used to bring it back after an
/// in-place upgrade (`setup.StartInstalled`, setup.go:239).
pub fn start_installed() -> anyhow::Result<()> {
    let st = detect();
    if !file_exists(Path::new(&st.install_exe)) {
        anyhow::bail!("no installed binary to start");
    }
    exec_command_detached(&st.install_exe)
}

/// Picks the binary autostart should launch: the canonical install when
/// present, else the running binary (portable mode)
/// (`setup.autostartTarget`, setup.go:249).
fn autostart_target() -> String {
    let st = detect();
    if file_exists(Path::new(&st.install_exe)) {
        st.install_exe
    } else {
        st.exe
    }
}

/// The installed binary name (`setup.installExeName`).
pub(crate) fn install_exe_name() -> &'static str {
    if cfg!(windows) {
        "agent-notify.exe"
    } else {
        SERVICE_NAME
    }
}

/// The console-subsystem twin shipped in the Windows zip (it hosts the
/// setup wizard's window) (`setup.consoleExeName`).
pub(crate) fn console_exe_name() -> &'static str {
    if cfg!(windows) {
        "agent-notify-console.exe"
    } else {
        ""
    }
}

/// Shipped companions to copy alongside the binary: the console twin on
/// Windows (the tray menu's wizard window) (`setup.siblingExes`).
pub(crate) fn sibling_exes(exe: &str) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut twin = console_exe_name().to_string();
    if go_base(exe) == twin {
        twin = install_exe_name().to_string();
    }
    let p = install_dir_join(&go_dir(exe), &twin);
    if file_exists(Path::new(&p)) {
        vec![p]
    } else {
        Vec::new()
    }
}

/// Places src at dst atomically: write beside the target, then rename over
/// it. Rename-aside keeps a locked (running) destination on Windows
/// replaceable (`setup.copyFile`, setup.go:293).
pub(crate) fn copy_file(src: &str, dst: &str) -> anyhow::Result<()> {
    let data = std::fs::read(src)?;
    let tmp = format!("{dst}.tmp");
    write_mode(Path::new(&tmp), &data, 0o755)
        .map_err(|e| anyhow::anyhow!("write {tmp}: {e}"))?;
    match std::fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(err) => {
            if !cfg!(windows) {
                let _ = std::fs::remove_file(&tmp);
                return Err(anyhow::anyhow!("rename {tmp} to {dst}: {err}"));
            }
            // A running exe cannot be replaced in place, but it can be
            // moved out of the way first.
            let old = format!("{dst}.old");
            let _ = std::fs::rename(dst, &old);
            std::fs::rename(&tmp, dst)
                .map_err(|err2| anyhow::anyhow!("rename {tmp} to {dst}: {err2}"))
        }
    }
}

/// The running binary with symlinks resolved (`setup.selfExe`). Empty when
/// the executable path cannot be determined. The Windows verbatim `\\?\`
/// prefix is stripped to match what Go's EvalSymlinks would return.
fn self_exe() -> String {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    strip_verbatim(resolved).to_string_lossy().into_owned()
}

/// Windows canonicalize yields `\\?\C:\...`; Go never does.
#[cfg(not(target_os = "windows"))]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    p
}

#[cfg(target_os = "windows")]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

/// `setup.statePath`: `<user config dir>/agent-notify/setup-state.json`.
fn state_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve the user config dir"))?;
    Ok(dir.join(SERVICE_NAME).join("setup-state.json"))
}

/// Whether the one-time setup nudge is due: it fires exactly once per
/// machine, and only when the deployment looks unfinished
/// (`setup.NudgeDue`, setup.go:342). The nudge itself travels as an
/// event synthesized by the tray.
pub fn nudge_due() -> bool {
    let Ok(p) = state_path() else {
        return false;
    };
    if file_exists(&p) {
        return false;
    }
    detect().needs_setup()
}

/// Records that the nudge fired (`setup.MarkNudged`, setup.go:351).
pub fn mark_nudged() -> anyhow::Result<()> {
    let p = state_path()?;
    if let Some(dir) = p.parent() {
        mkdir_all(dir, 0o755)?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let data = serde_json::to_string(&NudgeState {
        nudged_at: rfc3339_utc(now),
    })?;
    write_mode(&p, data.as_bytes(), 0o644).map_err(|e| anyhow::anyhow!("write {}: {e}", p.display()))
}

/// `nudgeState`: one JSON line recording that the first-run toast fired.
/// Go marshals a `time.Time` (RFC 3339, local offset); the port writes UTC.
#[derive(serde::Serialize)]
struct NudgeState {
    nudged_at: String,
}

/// Formats a duration-since-epoch as RFC 3339 UTC with nanoseconds trimmed
/// of trailing zeros (Go's RFC3339Nano layout).
fn rfc3339_utc(d: std::time::Duration) -> String {
    let secs = d.as_secs() as i64;
    let nanos = d.subsec_nanos();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, day) = civil_from_days(days);
    let frac = format!("{nanos:09}");
    let frac = frac.trim_end_matches('0');
    let frac = if frac.is_empty() {
        String::new()
    } else {
        format!(".{frac}")
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{frac}Z",
        y,
        m,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `fileExists`: exists and is not a directory (setup.go:366).
pub(crate) fn file_exists(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| !m.is_dir()).unwrap_or(false)
}

/// Process-unique counter for test temp paths (stand-in for t.TempDir).
#[cfg(test)]
pub(crate) fn test_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// `os.MkdirAll(path, mode)`.
pub(crate) fn mkdir_all(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(mode)
            .recursive(true)
            .create(path)?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Write with a unix mode on creation (Go `os.WriteFile` semantics).
pub(crate) fn write_mode(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, data)
    }
}

/// `filepath.Join(dir, name)` for the (already clean) directory strings
/// this package deals with.
pub(crate) fn install_dir_join(dir: &str, name: &str) -> String {
    Path::new(dir).join(name).to_string_lossy().into_owned()
}

// ----------------------------------------------------- Go path semantics --
// filepath.Clean / Dir / Base, ported so samePath compares exactly what Go
// compares (Rust's Path components do not lexically resolve `..`).

#[cfg(not(target_os = "windows"))]
fn is_sep(b: u8) -> bool {
    b == b'/'
}

#[cfg(target_os = "windows")]
fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// Length of the windows volume prefix ("C:", `\\?\C:`, `\\?\UNC\share`);
/// 0 elsewhere. UNC roots beyond the share component are not modeled.
#[cfg(target_os = "windows")]
fn volume_prefix_len(b: &[u8]) -> usize {
    if b.starts_with(br"\\?\UNC\") {
        let mut i = 8;
        while i < b.len() && !is_sep(b[i]) {
            i += 1;
        }
        return i;
    }
    if b.starts_with(br"\\?\") && b.len() >= 6 && b[5] == b':' {
        return 6;
    }
    if b.starts_with(br"\\?\") {
        return 4;
    }
    if b.len() >= 2 && b[1] == b':' {
        return 2;
    }
    0
}

#[cfg(not(target_os = "windows"))]
fn volume_prefix_len(_b: &[u8]) -> usize {
    0
}

/// `filepath.Clean`.
pub(crate) fn go_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let b = path.as_bytes();
    let vol = volume_prefix_len(b);
    let rooted = vol < b.len() && is_sep(b[vol]);
    let mut out: Vec<u8> = b[..vol].to_vec();
    let mut r = vol;
    let mut dotdot = vol;
    if rooted {
        out.push(b'/');
        r += 1;
        dotdot = out.len();
    }
    while r < b.len() {
        if is_sep(b[r]) {
            r += 1;
        } else if b[r] == b'.' && (r + 1 == b.len() || is_sep(b[r + 1])) {
            r += 1;
        } else if b[r] == b'.'
            && r + 1 < b.len()
            && b[r + 1] == b'.'
            && (r + 2 == b.len() || is_sep(b[r + 2]))
        {
            r += 2;
            if out.len() > dotdot {
                out.pop();
                while out.len() > dotdot && !is_sep(*out.last().expect("non-empty")) {
                    out.pop();
                }
            } else if !rooted {
                if out.len() > vol {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // out carries the volume prefix, so the "no separator yet"
            // marks are vol+1 (rooted) / vol (relative), matching Go's
            // relative lazybuf positions.
            if (rooted && out.len() != vol + 1) || (!rooted && out.len() != vol) {
                out.push(b'/');
            }
            while r < b.len() && !is_sep(b[r]) {
                out.push(b[r]);
                r += 1;
            }
        }
    }
    if out.len() <= vol {
        return ".".to_string();
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `filepath.Dir`.
pub(crate) fn go_dir(path: &str) -> String {
    let b = path.as_bytes();
    let vol = volume_prefix_len(b);
    let mut i = b.len() as isize - 1;
    while i >= vol as isize && !is_sep(b[i as usize]) {
        i -= 1;
    }
    let dir = go_clean(&path[vol..(i + 1) as usize]);
    if dir == "." && vol > 2 {
        return path[..vol].to_string();
    }
    format!("{}{}", &path[..vol], dir)
}

/// `filepath.Base`.
pub(crate) fn go_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let b = path.as_bytes();
    let vol = volume_prefix_len(b);
    let mut end = b.len();
    while end > vol && is_sep(b[end - 1]) {
        end -= 1;
    }
    let mut i = end as isize - 1;
    while i >= 0 && !is_sep(b[i as usize]) {
        i -= 1;
    }
    let last = &path[(i + 1) as usize..end];
    if last.is_empty() {
        if cfg!(windows) {
            return "\\".to_string();
        }
        return "/".to_string();
    }
    // Go's Base drops the volume name: Base("C:\\foo") == "foo".
    last.to_string()
}
