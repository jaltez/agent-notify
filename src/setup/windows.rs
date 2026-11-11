//! Windows half of the setup port (`internal/setup/setup_windows.go`):
//! %LOCALAPPDATA%\Programs install dir, Startup .lnk autostart via
//! PowerShell, user PATH through the registry + WM_SETTINGCHANGE
//! broadcast, WSL daemon install. Compiled only on Windows; everything
//! outside this module is OS-neutral.

use super::*;
// `::windows` (leading ::) — `use super::*` imports the sibling module
// named `windows`, which would shadow the extern crate of the same name.
use std::io::Read as _;
use std::path::Path;

use ::windows::core::HSTRING;
use ::windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WPARAM};
use ::windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
use ::windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

pub(crate) fn platform_install_dir() -> anyhow::Result<String> {
    let base = std::env::var("LOCALAPPDATA").map_err(|_| {
        anyhow::anyhow!("%LOCALAPPDATA% is not set")
    })?;
    if base.is_empty() {
        anyhow::bail!("%LOCALAPPDATA% is not set");
    }
    Ok(Path::new(&base)
        .join("Programs")
        .join(SERVICE_NAME)
        .to_string_lossy()
        .into_owned())
}

/// Same path is case-insensitive: Windows paths are.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    go_clean(a).eq_ignore_ascii_case(&go_clean(b))
}

pub(crate) fn platform_autostart_desc() -> String {
    format!("Startup shortcut ({})", startup_shortcut_path())
}

fn startup_shortcut_path() -> String {
    match std::env::var("APPDATA") {
        Ok(app_data) if !app_data.is_empty() => Path::new(&app_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join(format!("{SERVICE_NAME}.lnk"))
            .to_string_lossy()
            .into_owned(),
        _ => String::new(),
    }
}

pub(crate) fn platform_tray_autostart_installed() -> bool {
    let p = startup_shortcut_path();
    !p.is_empty() && file_exists(Path::new(&p))
}

/// Writes the Startup shortcut. A .lnk is a COM-serialized object, not
/// plain bytes, so it is built by a generated PowerShell script (same
/// WScript.Shell flow install.ps1 used) (`platformCreateTrayAutostart`).
pub(crate) fn platform_create_tray_autostart(exe: &str) -> anyhow::Result<()> {
    let lnk = startup_shortcut_path();
    if lnk.is_empty() {
        anyhow::bail!("%APPDATA% is not set");
    }
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut('{}')\n\
         $s.TargetPath = '{}'\n\
         $s.WorkingDirectory = '{}'\n\
         $s.Save()\n",
        ps_quote(&lnk),
        ps_quote(exe),
        ps_quote(&go_dir(exe)),
    );
    run_power_shell(&script)?;
    Ok(())
}

pub(crate) fn platform_remove_tray_autostart() -> anyhow::Result<()> {
    let p = startup_shortcut_path();
    if p.is_empty() || !file_exists(Path::new(&p)) {
        return Ok(());
    }
    std::fs::remove_file(&p)?;
    Ok(())
}

// The systemd integration is Linux/WSL-only.
pub(crate) fn platform_systemd() -> (bool, String) {
    (false, String::new())
}

pub(crate) fn platform_service_enabled() -> bool {
    false
}

pub(crate) fn platform_install_service(_exe: &str) -> anyhow::Result<()> {
    anyhow::bail!("the systemd service is Linux/WSL only; on Windows use autostart")
}

pub(crate) fn platform_remove_service() -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn platform_restart_service() -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn platform_enable_linger() -> anyhow::Result<()> {
    anyhow::bail!("linger is a systemd (Linux) concept")
}

pub(crate) fn platform_path_ok(dir: &str) -> bool {
    let Ok(cur) = user_path_value() else {
        return false;
    };
    cur.split(';').any(|p| same_path(go_expand_env(p).trim(), dir))
}

/// Reads the raw user `Path` value from HKCU\Environment
/// (`userPathValue`). Returns the registry text unexpanded (like Go's
/// registry.GetStringValue on a REG_EXPAND_SZ).
fn user_path_value() -> anyhow::Result<String> {
    match reg_get_string("Environment", "Path") {
        Ok(s) => Ok(s),
        Err(RegError::NotFound) => Err(anyhow::anyhow!("registry value not found")),
        Err(RegError::Other(e)) => Err(e),
    }
}

/// Appends the install dir to the user PATH and broadcasts the change so
/// new terminals (children of Explorer) see it without re-logging
/// (`platformAddToPath`).
pub(crate) fn platform_add_to_path(dir: &str) -> anyhow::Result<()> {
    unsafe {
        let subkey = HSTRING::from("Environment");
        let mut hk = HKEY::default();
        let st = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &subkey,
            None,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            &mut hk,
        );
        if st != ERROR_SUCCESS {
            anyhow::bail!("registry open failed: {st:?}");
        }
        let result = (|| -> anyhow::Result<()> {
            let cur = match reg_get_string("Environment", "Path") {
                Ok(v) => v,
                // ErrNotExist: no user Path yet — start from empty.
                Err(RegError::NotFound) => String::new(),
                Err(RegError::Other(e)) => return Err(e),
            };
            if !cur.is_empty() && platform_path_ok(dir) {
                return Ok(());
            }
            let trimmed = cur.trim();
            let mut nv = trimmed.strip_suffix(';').unwrap_or(trimmed).to_string();
            if nv.is_empty() {
                nv = dir.to_string();
            } else {
                nv.push(';');
                nv.push_str(dir);
            }
            // REG_EXPAND_SZ keeps any %VAR% references in the existing
            // value expanding; plain paths are unaffected.
            let mut wide: Vec<u16> = nv.encode_utf16().collect();
            wide.push(0);
            let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_le_bytes()).collect();
            let name = HSTRING::from("Path");
            let st = RegSetValueExW(hk, &name, None, REG_EXPAND_SZ, Some(&bytes));
            if st != ERROR_SUCCESS {
                anyhow::bail!("registry set failed: {st:?}");
            }
            broadcast_env_change();
            Ok(())
        })();
        let _ = RegCloseKey(hk);
        result
    }
}

/// Notifies the shell about the PATH update, the SetEnvironmentVariable
/// equivalent of .NET, which does this too (`broadcastEnvChange`).
fn broadcast_env_change() {
    unsafe {
        const SMTO_TIMEOUT_MS: u32 = 5000;
        let env = HSTRING::from("Environment");
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(env.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            SMTO_TIMEOUT_MS,
            None,
        );
    }
}

/// Stops agent-notify processes that would lock the files being replaced
/// (a tray running from the install dir), sparing the wizard itself.
/// Returns whether anything was stopped (`platformStopInstances`).
pub(crate) fn platform_stop_instances(self_exe: &str) -> bool {
    let script = format!(
        "$p = Get-Process '{ServiceName}','{console}' -ErrorAction SilentlyContinue | \
         Where-Object {{ $_.Path -and $_.Path -ne '{self}' }}\n\
         if ($p) {{ $p | Stop-Process -Force; Write-Output 1 }} else {{ Write-Output 0 }}\n",
        ServiceName = SERVICE_NAME,
        console = console_exe_name(),
        self = ps_quote(self_exe),
    );
    match run_power_shell(&script) {
        Ok(out) => out.trim() == "1",
        Err(_) => false,
    }
}

pub(crate) fn platform_wsl_available() -> bool {
    crate::proc::look_path("wsl.exe").is_some()
}

/// Installs the headless daemon inside a WSL distro by piping the
/// repository's install.sh through it with --service, which in turn runs
/// `agent-notify setup --yes --service` inside the distro
/// (`platformInstallWSL`). Output from wsl.exe is streamed into `w`.
pub(crate) fn platform_install_wsl(
    distro: &str,
    w: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    // Point at the tree the running binary came from; dev builds use main.
    let reference = if crate::buildinfo::is_release() {
        crate::buildinfo::VERSION
    } else {
        "main"
    };
    let base = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        crate::buildinfo::OWNER,
        crate::buildinfo::REPO,
        reference
    );
    let inner = format!(
        "curl -fsSL {base}/scripts/install.sh | sh -s -- --service || wget -qO- {base}/scripts/install.sh | sh -s -- --service"
    );
    let mut args: Vec<String> = Vec::new();
    if !distro.is_empty() {
        args.push("-d".to_string());
        args.push(distro.to_string());
    }
    args.extend(["-e".to_string(), "sh".to_string(), "-c".to_string(), inner]);

    use std::process::{Command, Stdio};
    let mut child = Command::new("wsl.exe")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    // Both pipes stream into w like Go's cmd.Stdout = cmd.Stderr = w (two
    // copying goroutines): stderr is relayed through a channel so the main
    // loop can interleave.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let stderr_t = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        if let Some(p) = stderr.as_mut() {
            while let Ok(n) = p.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    });
    let mut result: anyhow::Result<()> = Ok(());
    if let Some(p) = stdout.as_mut() {
        let mut buf = [0u8; 4096];
        loop {
            match p.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = w.write_all(&buf[..n]) {
                        result = Err(e.into());
                        break;
                    }
                }
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
        }
    }
    while let Ok(chunk) = rx.try_recv() {
        let _ = w.write_all(&chunk);
    }
    let status = child.wait()?;
    if let Err(e) = stderr_t.join() {
        // a paned stderr reader must not mask the child's outcome
        let _ = e;
    }
    result?;
    if !status.success() {
        anyhow::bail!("wsl.exe: exit status {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Deletes the installed exes. A running exe cannot be deleted but can be
/// renamed aside; if even that fails, its deletion is scheduled for after
/// the wizard exits (`platformRemoveInstalled`).
pub(crate) fn platform_remove_installed(st: &Status) -> Vec<String> {
    let mut notes = Vec::new();
    let mut removed = false;
    for name in [install_exe_name(), console_exe_name()] {
        let p = Path::new(&st.install_dir).join(name);
        if !file_exists(&p) {
            continue;
        }
        if let Err(_) = std::fs::remove_file(&p) {
            let moved = std::env::temp_dir().join(format!("agent-notify-removed-{name}"));
            if std::fs::rename(&p, &moved).is_err() {
                delayed_delete(&p.to_string_lossy());
                notes.push(format!(
                    "{} is still running; it will be deleted when this window closes",
                    p.display()
                ));
                removed = true;
                continue;
            }
            let _ = std::fs::remove_file(&moved);
        }
        removed = true;
    }
    if removed {
        notes.push(format!("removed the installed binaries from {}", st.install_dir));
    } else {
        notes.push("no installed binaries found (portable mode?)".to_string());
    }
    // Succeeds only when we were the dir's sole tenant, by design.
    let _ = std::fs::remove_dir(&st.install_dir);
    notes
}

/// Removes path once this process has exited; ping is the always-available
/// sub-second sleep (`delayedDelete`).
fn delayed_delete(path: &str) {
    let script = format!("ping -n 4 127.0.0.1 >nul & del /q \"{path}\"");
    let _ = std::process::Command::new("cmd").args(["/C", &script]).spawn();
}

/// Executes script through powershell.exe and returns its combined output.
/// The script travels via a temp file: inline -Command quoting of arbitrary
/// paths is a losing game (`runPowerShell`).
fn run_power_shell(script: &str) -> anyhow::Result<String> {
    let tmp = std::env::temp_dir().join(format!(
        "agent-notify-{}-{}.ps1",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let cleanup = |p: &Path| {
        let _ = std::fs::remove_file(p);
    };
    if let Err(e) = std::fs::write(&tmp, script) {
        cleanup(&tmp);
        anyhow::bail!("write {}: {e}", tmp.display());
    }
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&tmp)
        .output();
    cleanup(&tmp);
    let out = out.map_err(|e| anyhow::anyhow!("powershell: {e}"))?;
    // CombinedOutput equivalent: stdout then stderr (Go merges the pipes).
    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);
    let combined = String::from_utf8_lossy(&combined).into_owned();
    if !out.status.success() {
        anyhow::bail!(
            "powershell: exit status {}: {}",
            out.status.code().unwrap_or(-1),
            combined.trim()
        );
    }
    Ok(combined)
}

/// Makes a PowerShell single-quoted literal (`psQuote`).
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

pub(crate) fn exec_command_detached(exe: &str) -> anyhow::Result<()> {
    std::process::Command::new(exe).spawn()?;
    Ok(())
}

/// Marks a file hidden when Windows refuses to delete its own running
/// binary's .old leftover (`update.hideFile` equivalent).
#[allow(dead_code)]
fn hide_file(path: &Path) -> anyhow::Result<()> {
    let s = path.to_string_lossy();
    unsafe {
        SetFileAttributesW(&HSTRING::from(s.as_ref()), FILE_ATTRIBUTE_HIDDEN)
            .map_err(|e| anyhow::anyhow!("SetFileAttributesW {}: {e}", path.display()))
    }
}

// ------------------------------------------------------------- registry --

enum RegError {
    /// ERROR_FILE_NOT_FOUND (Go registry.ErrNotExist).
    NotFound,
    Other(anyhow::Error),
}

/// Reads a REG_SZ/REG_EXPAND_SZ string value (Go:
/// registry.Key.GetStringValue).
fn reg_get_string(subkey: &str, value: &str) -> Result<String, RegError> {
    unsafe {
        let sub = HSTRING::from(subkey);
        let mut hk = HKEY::default();
        let st = RegOpenKeyExW(HKEY_CURRENT_USER, &sub, None, KEY_QUERY_VALUE, &mut hk);
        if st != ERROR_SUCCESS {
            return Err(RegError::Other(anyhow::anyhow!("registry open failed: {st:?}")));
        }
        let result = reg_query_string(hk, value);
        let _ = RegCloseKey(hk);
        result
    }
}

unsafe fn reg_query_string(hk: HKEY, value: &str) -> Result<String, RegError> {
    let name = HSTRING::from(value);
    let mut size: u32 = 0;
    // edition 2024: unsafe operations inside unsafe fn still need blocks
    let st = unsafe { RegQueryValueExW(hk, &name, None, None, None, Some(&mut size)) };
    if st == ERROR_FILE_NOT_FOUND {
        return Err(RegError::NotFound);
    }
    if st != ERROR_SUCCESS {
        return Err(RegError::Other(anyhow::anyhow!("registry query failed: {st:?}")));
    }
    let mut buf = vec![0u8; size as usize];
    let st = unsafe {
        RegQueryValueExW(hk, &name, None, None, Some(buf.as_mut_ptr()), Some(&mut size))
    };
    if st != ERROR_SUCCESS {
        return Err(RegError::Other(anyhow::anyhow!("registry query failed: {st:?}")));
    }
    buf.truncate(size as usize);
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// `os.ExpandEnv` ($var and ${var}), ported including its quirks — notably
/// that `%VAR%` is NOT expanded, exactly as in the Go original.
pub(crate) fn go_expand_env(s: &str) -> String {
    let b = s.as_bytes();
    let mut buf = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
            buf.push_str(&s[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        if i + 1 >= b.len() {
            // trailing '$': valid syntax with no name — leave it untouched
            buf.push('$');
            break;
        }
        let (name, w) = get_shell_name(&s[i + 1..]);
        if name.is_empty() && w > 0 {
            // invalid syntax: eat the characters
        } else if name.is_empty() {
            buf.push('$');
        } else {
            buf.push_str(&std::env::var(&name).unwrap_or_default());
        }
        i += 1 + w;
    }
    buf
}

/// Go os.getShellName: `${name}`, `$special`, or `$alphanumerics`.
fn get_shell_name(s: &str) -> (&str, usize) {
    let b = s.as_bytes();
    if b.is_empty() {
        return ("", 0);
    }
    if b[0] == b'{' {
        if b.len() > 2 && is_shell_special(b[1]) && b[2] == b'}' {
            return (&s[1..2], 3);
        }
        for i in 1..b.len() {
            if b[i] == b'}' {
                if i == 1 {
                    return ("", 2); // bad syntax; eat "${}"
                }
                return (&s[1..i], i + 1);
            }
        }
        return ("", 1);
    }
    if is_shell_special(b[0]) {
        return (&s[..1], 1);
    }
    let mut i = 0;
    while i < b.len() && is_alpha_num(b[i]) {
        i += 1;
    }
    (&s[..i], i)
}

fn is_shell_special(c: u8) -> bool {
    matches!(c, b'*' | b'#' | b'$' | b'@' | b'!' | b'?' | b'-' | b'0'..=b'9')
}

fn is_alpha_num(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

// ------------------------------------------------------------------ tests --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_env_matches_go_semantics() {
        // PORT-NOTE: %VAR% is deliberately NOT expanded — Go's os.ExpandEnv
        // has the same blind spot, and platform_path_ok relies on it.
        unsafe { std::env::set_var("AN_TEST_VAR", "x") };
        assert_eq!(go_expand_env("$AN_TEST_VAR"), "x");
        assert_eq!(go_expand_env("${AN_TEST_VAR}"), "x");
        assert_eq!(go_expand_env("a$AN_TEST_VAR-b"), "ax-b");
        assert_eq!(go_expand_env("%LOCALAPPDATA%"), "%LOCALAPPDATA%");
        assert_eq!(go_expand_env("$UNSET_VAR_XYZ"), "");
        assert_eq!(go_expand_env("no dollar"), "no dollar");
        assert_eq!(go_expand_env("$"), "$");
    }

    #[test]
    fn ps_quote_doubles_quotes() {
        assert_eq!(ps_quote(r"C:\Program Files\a.lnk"), r"C:\Program Files\a.lnk");
        assert_eq!(ps_quote("it's"), "it''s");
    }

    #[test]
    fn same_path_is_case_insensitive() {
        assert!(same_path(r"C:\Users\a\Programs\agent-notify", r"c:\users\a\programs\agent-notify"));
        assert!(!same_path(r"C:\a", r"C:\b"));
    }
}
