//! Centralizes subprocess mechanics shared by the herdr backends and the
//! popup sink: running commands with annotated errors, Windows-interop
//! quirks (UTF-16 output, UNC working-directory warnings), and binary
//! lookup. Port of `internal/proc/proc.go`, `proc_windows.go` and
//! `proc_other.go`.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Applies the child-process attributes every spawned child must use
/// (`proc.ChildSysProcAttr`): on Windows, suppress the child console window
/// (the popup sink and the command sink spawn children the same way the
/// herdr backends do).
pub fn child_attrs(cmd: &mut Command) {
    child_creation_flags(cmd);
}

/// Executes argv and returns stdout (`proc.Run`). `extra_env` entries are
/// "KEY=VALUE" strings appended to the inherited environment. `dir` empty
/// means inherit. `timeout` stands in for Go's `context.WithTimeout`: the
/// child is killed and the call fails once it elapses. Errors carry a
/// trimmed snippet of stderr so callers see the real cause (e.g. herdr's
/// "server_not_running").
pub fn run(
    argv: &[String],
    extra_env: &[String],
    dir: &str,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let (name, rest) = match argv.split_first() {
        Some(x) if !x.0.is_empty() => (x.0, x.1),
        _ => anyhow::bail!("proc: empty argv"),
    };
    let mut cmd = Command::new(name);
    cmd.args(rest);
    for kv in extra_env {
        match kv.split_once('=') {
            Some((k, v)) => {
                cmd.env(k, v);
            }
            None => anyhow::bail!("proc: malformed env entry {kv:?} (want KEY=VALUE)"),
        }
    }
    if !dir.is_empty() {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    child_creation_flags(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Err(annotate(name, &e.to_string(), "")),
    };

    // Drain stdout and stderr on separate threads so a chatty stderr cannot
    // deadlock the pipe; Go's cmd.Output does this internally.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(annotate(name, &e.to_string(), ""));
            }
        }
    };
    let out = stdout_t.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr_t.join().unwrap_or_default()).into_owned();

    match status {
        None => Err(annotate(name, "context deadline exceeded", &stderr)),
        Some(st) if !st.success() => {
            Err(annotate(name, &format!("exit status {st}"), &stderr))
        }
        Some(_) => Ok(out),
    }
}

/// `{name}: {err}` or `{name}: {err}: {stderr-snippet}` — the first line of
/// stderr, trimmed, capped at 200 chars (`proc.annotate`).
fn annotate(name: &str, err: &str, stderr: &str) -> anyhow::Error {
    let mut snip = stderr.trim();
    if let Some(i) = snip.find('\n') {
        if i > 0 && i < 200 {
            snip = &snip[..i];
        }
    }
    if snip.len() > 200 {
        snip = &snip[..200];
    }
    if snip.is_empty() {
        anyhow::anyhow!("{name}: {err}")
    } else {
        anyhow::anyhow!("{name}: {err}: {snip}")
    }
}

/// Returns a Windows-visible working directory ("/mnt/c/Windows" under WSL)
/// so Windows executables spawned from WSL do not warn about UNC paths.
/// Returns `None` when not applicable or unavailable (`proc.InteropDir`).
pub fn interop_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        return None;
    }
    for d in ["/mnt/c/Windows", "/mnt/c"] {
        if std::fs::metadata(d).map(|m| m.is_dir()).unwrap_or(false) {
            return Some(PathBuf::from(d));
        }
    }
    None
}

/// Returns b as UTF-8, decoding UTF-16LE when it smells like it
/// (`proc.DecodeUTF16`). wsl.exe transcodes some outputs to UTF-16; plain
/// UTF-8 (the common case, and everything herdr emits) passes through
/// untouched.
pub fn decode_utf16(b: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if b.len() < 2 {
        return std::borrow::Cow::Borrowed(b);
    }
    if b[0] == 0xFF && b[1] == 0xFE {
        return std::borrow::Cow::Owned(decode_utf16le(&b[2..]));
    }
    let sample_len = b.len().min(1024);
    let mut odd_zeros = 0;
    let mut i = 1;
    while i < sample_len {
        if b[i] == 0 {
            odd_zeros += 1;
        }
        i += 2;
    }
    if odd_zeros == 0 || odd_zeros * 3 < sample_len / 2 {
        return std::borrow::Cow::Borrowed(b); // ordinary UTF-8/ASCII
    }
    std::borrow::Cow::Owned(decode_utf16le(b))
}

fn decode_utf16le(b: &[u8]) -> Vec<u8> {
    let units: Vec<u16> =
        b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).into_bytes()
}

/// Looks a binary up in PATH (`exec.LookPath`), returning its resolved path.
/// On Windows, a name without extension also tries `.exe`. Returns `None`
/// when not found or PATH is unusable.
pub fn look_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        return if std::fs::metadata(name).map(|m| m.is_file()).unwrap_or(false) {
            Some(PathBuf::from(name))
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    let extra_ext: &[&str] =
        if cfg!(target_os = "windows") && !name.to_ascii_lowercase().ends_with(".exe") {
            &[".exe"]
        } else {
            &[]
        };
    for dir in std::env::split_paths(&path_var) {
        let mut candidates = vec![dir.join(name)];
        for ext in extra_ext {
            candidates.push(dir.join(format!("{name}{ext}")));
        }
        for cand in candidates {
            // Windows LookPath matches files only; metadata is a cheap proxy
            // for "exists and is a regular file".
            if std::fs::metadata(&cand).map(|m| m.is_file()).unwrap_or(false) {
                return Some(cand);
            }
        }
    }
    None
}

/// Windows: suppress child console windows. The tray runs as a windowsgui
/// process with no console, so without CREATE_NO_WINDOW every spawned CLI
/// (herdr.exe, wsl.exe, powershell.exe) would allocate and flash a new
/// console window (`proc_windows.go`: HideWindow + CREATE_NO_WINDOW;
/// CREATE_NO_WINDOW alone covers both, so std's CommandExt suffices).
#[cfg(target_os = "windows")]
fn child_creation_flags(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // == windows::Win32::System::Threading::CREATE_NO_WINDOW
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Off Windows, a no-op (`proc_other.go`).
#[cfg(not(target_os = "windows"))]
fn child_creation_flags(_cmd: &mut Command) {}
