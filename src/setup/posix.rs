//! POSIX half of the setup port (`internal/setup/setup_posix.go`,
//! `//go:build !windows`): ~/.local/bin install dir, XDG autostart, systemd
//! user service, ~/.profile PATH export, pgrep/SIGTERM instance stop.

use super::*;
use std::io::Write as _;
use std::path::Path;

/// unitTemplate is rendered with the installed binary path by
/// [`super::ensure_service`].
pub(crate) const UNIT_TEMPLATE: &str = include_str!("agent-notify.service");

/// desktopTemplate is rendered into ~/.config/autostart by
/// [`super::ensure_tray_autostart`].
pub(crate) const DESKTOP_TEMPLATE: &str = include_str!("agent-notify.desktop");

pub(crate) fn platform_install_dir() -> anyhow::Result<String> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("$HOME is not defined"))?;
    Ok(home.join(".local").join("bin").to_string_lossy().into_owned())
}

pub(crate) fn same_path(a: &str, b: &str) -> bool {
    go_clean(a) == go_clean(b)
}

pub(crate) fn platform_autostart_desc() -> String {
    format!("XDG autostart ({})", desktop_path())
}

fn desktop_path() -> String {
    match dirs::config_dir() {
        Some(dir) => dir
            .join("autostart")
            .join(format!("{SERVICE_NAME}.desktop"))
            .to_string_lossy()
            .into_owned(),
        None => String::new(),
    }
}

pub(crate) fn platform_tray_autostart_installed() -> bool {
    let p = desktop_path();
    !p.is_empty() && file_exists(Path::new(&p))
}

pub(crate) fn platform_create_tray_autostart(exe: &str) -> anyhow::Result<()> {
    let p = desktop_path();
    if p.is_empty() {
        anyhow::bail!("cannot resolve the XDG config directory");
    }
    mkdir_all(Path::new(&go_dir(&p)), 0o755)?;
    // The Exec key needs quoting when the path contains spaces.
    let mut exec_key = exe.to_string();
    if exe.contains([' ', '\t']) {
        exec_key = format!("\"{}\"", exe.replace('"', ""));
    }
    let content = DESKTOP_TEMPLATE.replacen("__EXEC__", &exec_key, 1);
    write_mode(Path::new(&p), content.as_bytes(), 0o644)
        .map_err(|e| anyhow::anyhow!("write {p}: {e}"))
}

pub(crate) fn platform_remove_tray_autostart() -> anyhow::Result<()> {
    let p = desktop_path();
    if p.is_empty() || !file_exists(Path::new(&p)) {
        return Ok(());
    }
    std::fs::remove_file(&p)?;
    Ok(())
}

fn systemd_unit_path() -> String {
    match dirs::config_dir() {
        Some(dir) => dir
            .join("systemd")
            .join("user")
            .join(format!("{SERVICE_NAME}.service"))
            .to_string_lossy()
            .into_owned(),
        None => String::new(),
    }
}

/// Reports whether a systemd user manager is reachable. WSL without
/// `[boot] systemd=true` is the common "no" case; the hint carries the
/// exact fix (`platformSystemd`).
pub(crate) fn platform_systemd() -> (bool, String) {
    if crate::proc::look_path("systemctl").is_none() {
        return (false, String::new());
    }
    match std::fs::metadata("/run/systemd/system") {
        Ok(m) if m.is_dir() => (true, String::new()),
        _ => (
            false,
            "systemd is not running here (WSL: set [boot] systemd=true in /etc/wsl.conf, run `wsl --shutdown` from Windows, then retry)"
                .to_string(),
        ),
    }
}

pub(crate) fn platform_service_enabled() -> bool {
    let p = systemd_unit_path();
    if p.is_empty() || !file_exists(Path::new(&p)) {
        return false;
    }
    // PORT-NOTE: Go's exec.Command(...).Run() with nil Stdout/Stderr
    // discards the child's output; .status() would inherit it and leak
    // systemctl's "enabled"/"disabled" line into `setup --status` (spotted
    // while porting cli). .output() swallows it like Go.
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", SERVICE_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) fn platform_install_service(exe: &str) -> anyhow::Result<()> {
    let (ok, hint) = platform_systemd();
    if !ok {
        anyhow::bail!("cannot install the service: {hint}");
    }
    let p = systemd_unit_path();
    mkdir_all(Path::new(&go_dir(&p)), 0o755)?;
    let content = UNIT_TEMPLATE.replacen("__BIN__", exe, 1);
    write_mode(Path::new(&p), content.as_bytes(), 0o644)
        .map_err(|e| anyhow::anyhow!("write {p}: {e}"))?;
    for args in [
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", "--now", SERVICE_NAME],
    ] {
        let out = std::process::Command::new("systemctl")
            .args(&args)
            .output()
            .map_err(|e| anyhow::anyhow!("systemctl {}: {e}", args.join(" ")))?;
        if !out.status.success() {
            // CombinedOutput equivalent: stdout then stderr.
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            anyhow::bail!(
                "systemctl {}: exit status {}: {}",
                args.join(" "),
                exit_code(&out.status),
                String::from_utf8_lossy(&combined).trim()
            );
        }
    }
    Ok(())
}

/// Go-style `exit status N` for an exited child.
fn exit_code(st: &std::process::ExitStatus) -> i32 {
    st.code().unwrap_or(-1)
}

pub(crate) fn platform_remove_service() -> anyhow::Result<()> {
    // Best-effort: a dead systemd must not block uninstalling the files.
    // .output() discards systemctl's own output like Go's cmd.Run() (see
    // platform_service_enabled).
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", SERVICE_NAME])
        .output();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    let p = systemd_unit_path();
    if p.is_empty() || !file_exists(Path::new(&p)) {
        return Ok(());
    }
    std::fs::remove_file(&p)?;
    Ok(())
}

pub(crate) fn platform_restart_service() -> anyhow::Result<()> {
    let st = std::process::Command::new("systemctl")
        .args(["--user", "restart", SERVICE_NAME])
        .output()?; // discard systemctl output like Go's cmd.Run()
    if !st.status.success() {
        anyhow::bail!("exit status {}", st.status.code().unwrap_or(-1));
    }
    Ok(())
}

pub(crate) fn platform_enable_linger() -> anyhow::Result<()> {
    let st = std::process::Command::new("loginctl")
        .arg("enable-linger")
        .status()?;
    if !st.success() {
        anyhow::bail!("exit status {}", st.code().unwrap_or(-1));
    }
    Ok(())
}

pub(crate) fn platform_path_ok(dir: &str) -> bool {
    match std::env::var("PATH") {
        Ok(path) => std::env::split_paths(&path)
            .any(|p| same_path(&p.to_string_lossy(), dir)),
        Err(_) => false,
    }
}

/// Appends a marked export to ~/.profile, the least surprising user shell
/// file, only when the directory isn't already exported there
/// (`platformAddToPath`).
pub(crate) fn platform_add_to_path(dir: &str) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("$HOME is not defined"))?;
    let profile = home.join(".profile");
    if let Ok(data) = std::fs::read(&profile) {
        if String::from_utf8_lossy(&data).contains(dir) {
            return Ok(());
        }
    }
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .write(true)
            .mode(0o644)
            .open(&profile)?
    };
    write!(f, "\n# agent-notify path\nexport PATH={}:$PATH\n", go_quote(dir))?;
    Ok(())
}

/// Terminates other agent-notify processes so an old copy never keeps
/// running beside the installed one (Linux doesn't lock the file, but a
/// stale tray would keep toasting). Returns whether anything was stopped
/// (`platformStopInstances`).
pub(crate) fn platform_stop_instances(self_exe: &str) -> bool {
    let _ = self_exe; // unused on posix, kept for signature parity with Go
    let Ok(out) = std::process::Command::new("pgrep").args(["-x", SERVICE_NAME]).output()
    else {
        return false;
    };
    let mut stopped = false;
    for field in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let Ok(pid) = field.parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        // syscall.Kill(pid, SIGTERM)
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            stopped = true;
        }
    }
    stopped
}

/// Deletes the installed binary. Only the binary: ~/.local/bin is the
/// user's directory and may hold anything (`platformRemoveInstalled`).
pub(crate) fn platform_remove_installed(st: &Status) -> Vec<String> {
    if !file_exists(Path::new(&st.install_exe)) {
        return vec!["no installed binary found (portable mode?)".to_string()];
    }
    if let Err(err) = std::fs::remove_file(&st.install_exe) {
        return vec![format!("could not remove {}: {err}", st.install_exe)];
    }
    vec![format!("removed {}", st.install_exe)]
}

pub(crate) fn platform_wsl_available() -> bool {
    false
}

pub(crate) fn platform_install_wsl(
    _distro: &str,
    _w: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    anyhow::bail!("the WSL daemon is installed from Windows, not from here")
}

pub(crate) fn exec_command_detached(exe: &str) -> anyhow::Result<()> {
    std::process::Command::new(exe).spawn()?;
    Ok(())
}

/// `strconv.Quote`: the Go-quoted form `fmt`'s `%q` produces, used for the
/// PATH export in ~/.profile. Printable characters pass through; control
/// characters get Go escapes.
pub(crate) fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            // PORT-NOTE: Go escapes non-printable non-ASCII runes (\uXXXX);
            // printable non-ASCII (the normal case for paths) passes
            // through, which we replicate without a Unicode print table.
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ------------------------------------------------------------------ tests --
// Port of `internal/setup/setup_test.go` (also `//go:build !windows`).
// Tests must not call install()/uninstall(): those stop live agent-notify
// processes, and a test run on a machine with the tray up would kill it.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every env-mutating test (Go's t.Setenv does the same) and
    /// points HOME/XDG at a temp tree so detect() and the file-writing
    /// actions never touch the real user environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct Sandbox {
        // held for the whole test
        _g: MutexGuard<'static, ()>,
        home: PathBuf,
    }

    impl Sandbox {
        fn new() -> Sandbox {
            let g = ENV_LOCK.lock().unwrap();
            let home = std::env::temp_dir().join(format!(
                "an-setup-test-{}-{}",
                std::process::id(),
                crate::setup::test_counter(),
            ));
            std::fs::create_dir_all(&home).unwrap();
            // edition 2024: env writes are unsafe and racy across threads
            unsafe {
                std::env::set_var("HOME", &home);
                std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            }
            Sandbox { _g: g, home }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn detect_sandbox() {
        let sb = Sandbox::new();
        let st = detect();
        let want = sb.home.join(".local").join("bin");
        assert_eq!(
            st.install_dir,
            want.to_string_lossy(),
            "InstallDir mismatch"
        );
        assert!(!st.installed, "nothing installed in the sandbox");
        assert!(!st.path_ok, "sandbox dir is not on PATH");
        assert!(!st.autostart, "no autostart file exists in the sandbox");
        assert!(!st.service_enabled, "no unit file exists in the sandbox");
        assert!(!st.config_exists, "no config exists in the sandbox");
        assert!(st.needs_setup(), "a bare sandbox should need setup");
        assert!(!st.config_path.is_empty(), "a config path should still resolve");
    }

    #[test]
    fn needs_setup_matrix() {
        let cases: Vec<(&str, Status, bool)> = vec![
            ("bare download", Status::default(), true),
            ("installed only", Status { installed: true, ..Default::default() }, true),
            (
                "installed with autostart",
                Status { installed: true, autostart: true, ..Default::default() },
                false,
            ),
            (
                "installed with service",
                Status { installed: true, service_enabled: true, ..Default::default() },
                false,
            ),
            (
                "installed with config",
                Status { installed: true, config_exists: true, ..Default::default() },
                false,
            ),
            ("portable with config", Status { config_exists: true, ..Default::default() }, false),
        ];
        for (name, st, want) in cases {
            assert_eq!(st.needs_setup(), want, "{name}: NeedsSetup()");
        }
    }

    #[test]
    fn xdg_autostart_round_trip() {
        let _sb = Sandbox::new();
        let exe = "/home/u/.local/bin/agent-notify";
        platform_create_tray_autostart(exe).expect("create");
        let p = desktop_path();
        let data = std::fs::read_to_string(&p).expect("autostart file missing");
        let want = format!("Exec={exe}");
        assert!(data.contains(&want), "desktop entry lacks {want:?}:\n{data}");
        assert!(detect().autostart, "Detect should see the autostart entry");
        // Idempotent: a second run must succeed and keep one file.
        platform_create_tray_autostart(exe).expect("recreate");
        platform_remove_tray_autostart().expect("remove");
        assert!(!detect().autostart, "autostart should be gone after removal");
    }

    #[test]
    fn xdg_autostart_quotes_spaces() {
        let _sb = Sandbox::new();
        let exe = "/home/u/my binaries/agent-notify";
        platform_create_tray_autostart(exe).expect("create");
        let data = std::fs::read_to_string(desktop_path()).unwrap();
        let want = format!(r#"Exec="{exe}""#);
        assert!(
            data.contains(&want),
            "spacey path should be quoted as {want:?}:\n{data}"
        );
    }

    #[test]
    fn unit_template() {
        assert!(UNIT_TEMPLATE.contains("__BIN__"), "template lost its __BIN__ placeholder");
        let rendered = UNIT_TEMPLATE.replacen("__BIN__", "/home/u/.local/bin/agent-notify", 1);
        for want in [
            // the daemon exits cleanly on self-update, so Restart=always
            "ExecStart=/home/u/.local/bin/agent-notify run",
            "Restart=always",
        ] {
            assert!(rendered.contains(want), "rendered unit lacks {want:?}:\n{rendered}");
        }
    }

    #[test]
    fn profile_path_append() {
        let sb = Sandbox::new();
        let dir = sb.home.join(".local").join("bin");
        let dir = dir.to_string_lossy().into_owned();
        assert!(!platform_path_ok(&dir), "dir cannot already be on PATH in the sandbox");
        platform_add_to_path(&dir).expect("add");
        let data = std::fs::read_to_string(sb.home.join(".profile")).expect("profile missing");
        assert_eq!(
            data.matches("# agent-notify path").count(),
            1,
            "marker count should be 1 (idempotent):\n{data}"
        );
        assert!(data.contains(&dir), "profile does not export {dir:?}");
        // PATH detection against the real environment variable.
        let new_path = format!("{dir}:{}", std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", &new_path) };
        assert!(platform_path_ok(&dir), "platformPathOK should find the dir once PATH contains it");
    }

    #[test]
    fn nudge_state_round_trip() {
        let _sb = Sandbox::new();
        assert!(nudge_due(), "fresh sandbox: nudge should be due");
        mark_nudged().expect("mark");
        assert!(!nudge_due(), "nudge already shown, should not be due again");
    }

    #[test]
    fn copy_file_round_trip() {
        // the test binary itself: a real executable file
        let src = std::env::current_exe().unwrap();
        let src = src.to_string_lossy().into_owned();
        let dst = std::env::temp_dir().join(format!(
            "an-setup-copy-{}-{}",
            std::process::id(),
            crate::setup::test_counter(),
        ));
        let dst = dst.to_string_lossy().into_owned();
        copy_file(&src, &dst).expect("copy");
        let want = std::fs::read(&src).unwrap();
        let got = std::fs::read(&dst).expect("destination missing");
        assert_eq!(got, want, "copied bytes differ");
        // Overwrite in place (the self-update path).
        copy_file(&src, &dst).expect("re-copy");
        let leftovers: Vec<_> = glob_tmp(&format!("{dst}.tmp"));
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
        let _ = std::fs::remove_file(&dst);
    }

    fn glob_tmp(pattern: &str) -> Vec<String> {
        if std::fs::metadata(pattern).is_ok() {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        }
    }
}
