//! Delivers events to notifiers: the tray, desktop popups, the terminal
//! bell, arbitrary commands, HTTP webhooks, and log output. Port of
//! `internal/sink/` (`sink.go`, `popup.go`, `command.go`, `webhook.go`,
//! `log.go`).
//!
//! - [`Sink`] + [`Manager`] — fan-out with a per-sink queue (std threads +
//!   `std::sync::mpsc`, no tokio); a slow sink never blocks the engine
//! - [`Popup`] — toast/notify-send popups (severity logo from [`crate::icon`])
//! - [`Webhook`] — POST the event (JSON or a rendered body template)
//! - [`Command`] — run an argv/shell command per event
//! - [`Log`] / [`Bell`] — log line / terminal bell
//!
//! Go's `context.Context` timeouts become `Duration` bounds passed to each
//! sink (`deadline`); the worker threads poll an `Arc<AtomicBool>` stop flag
//! where Go selects on `ctx.Done()`.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// AtomicU64 (toast id / test dirs) only exists on the native-toast path and
// in tests; PathBuf only for the identity icon and test dirs.
#[cfg(any(target_os = "windows", test))]
use std::sync::atomic::AtomicU64;
#[cfg(any(target_os = "windows", test))]
use std::path::PathBuf;

use crate::config;
use crate::event::{self, Event};
use crate::proc;
use crate::render;

/// Bounds a single Deliver call (`sink.deliverTimeout`).
pub const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-sink queue size; a full queue drops (and logs) rather than blocking
/// (`sink.queueSize`).
pub const QUEUE_SIZE: usize = 16;

/// How often the worker threads re-check the stop flag while idle. Only
/// bounds shutdown latency: `recv_timeout` still wakes on every event the
/// moment it arrives (PORT-NOTE: Go's `select` on `ctx.Done()` is instant).
const RUN_POLL: Duration = Duration::from_millis(100);

/// Consumes events (`sink.Sink`). `deliver` should honor `deadline`; the
/// [`Manager`] bounds every call with [`DELIVER_TIMEOUT`] anyway. Go's
/// `context.Context` timeout becomes the `deadline` argument.
pub trait Sink: Send + Sync {
    fn name(&self) -> &'static str;
    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Manager (sink.go)

struct Registered {
    sink: Arc<dyn Sink>,
    tx: mpsc::SyncSender<Event>,
    /// Taken by [`Manager::run`] when it spawns the worker; `None` after.
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
}

/// Fans events out to sinks, one worker thread each (`sink.Manager`).
pub struct Manager {
    sinks: Vec<Registered>,
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

impl Manager {
    /// Builds an empty Manager (`sink.NewManager`).
    pub fn new() -> Manager {
        Manager { sinks: Vec::new() }
    }

    /// Registers a sink and starts its worker. Call before [`Manager::run`]
    /// (`Manager.Add`).
    pub fn add(&mut self, sink: Arc<dyn Sink>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_SIZE);
        self.sinks.push(Registered { sink, tx, rx: Mutex::new(Some(rx)) });
    }

    /// Lists registered sink names in order (`Manager.Names`).
    pub fn names(&self) -> Vec<String> {
        self.sinks.iter().map(|r| r.sink.name().to_string()).collect()
    }

    /// Fans one event out to every sink (non-blocking; full queues drop and
    /// log) (`Manager.Deliver`, sink.go:59). A `Disconnected` send is
    /// ignored: Go's channels never close, so post-stop delivers simply
    /// buffer there.
    pub fn deliver(&self, ev: &Event) {
        for r in &self.sinks {
            match r.tx.try_send(ev.clone()) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    eprintln!(
                        "agent-notify: sink queue full; dropping event sink={} kind={}",
                        r.sink.name(),
                        ev.kind.as_str()
                    );
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
    }

    /// Drives the workers until `stop` is set, then joins them
    /// (`Manager.Run`, sink.go:70). Each worker bounds every deliver with
    /// [`DELIVER_TIMEOUT`] (the sinks enforce it themselves — commands are
    /// killed, HTTP requests timed out).
    pub fn run(&self, stop: Arc<AtomicBool>) {
        let mut handles = Vec::new();
        for r in &self.sinks {
            let Some(rx) = r.rx.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                continue; // already started
            };
            let sink = Arc::clone(&r.sink);
            let stop_flag = Arc::clone(&stop);
            let name = sink.name().to_string();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("sink-{name}"))
                    .spawn(move || {
                        while !stop_flag.load(Ordering::Relaxed) {
                            match rx.recv_timeout(RUN_POLL) {
                                Ok(ev) => {
                                    if let Err(err) = sink.deliver(&ev, DELIVER_TIMEOUT) {
                                        eprintln!(
                                            "agent-notify: sink delivery failed sink={} kind={} error={err}",
                                            sink.name(),
                                            ev.kind.as_str()
                                        );
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    })
                    .expect("spawn sink worker"),
            );
        }
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(RUN_POLL);
        }
        for h in handles {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Popup (popup.go)

/// Makes Windows attribute toasts to PowerShell, which ships an
/// AppUserModelID that needs no Start Menu shortcut
/// (`defaultPowerShellAUMID`).
const DEFAULT_POWERSHELL_AUMID: &str =
    "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

/// PORT-NOTE (improvement over Go): the native Windows path posts through
/// rune-notify under this AppUserModelID instead of PowerShell's. Go's
/// default AUMID is PowerShell's — a path separator-bearing string
/// rune-notify rejects for registry writes, and one this port must not
/// overwrite — so the native identity gets its own id unless the config
/// names one explicitly.
#[cfg(target_os = "windows")]
const NATIVE_APP_ID: &str = "com.jaltez.agent-notify";

/// Raises a Windows toast using the WinRT toast API. The payload travels in
/// environment variables so no quoting or escaping of user content is ever
/// needed. AN_IMG selects the image+3-line template (Windows-native only,
/// where the file path is Windows-visible); AN_EXTRA adds a third text line
/// (space + fleet summary). Byte-for-byte the Go `toastScript` (leading and
/// trailing newline included).
const TOAST_SCRIPT: &str = r#"
try {
  [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
  $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastText02
  if ($env:AN_IMG) { $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastImageAndText04 }
  elseif ($env:AN_EXTRA) { $tpl = [Windows.UI.Notifications.ToastTemplateType]::ToastText04 }
  $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent($tpl)
  if ($env:AN_IMG) {
    $img = $x.GetElementsByTagName('image').Item(0)
    [void]$img.SetAttribute('src', $env:AN_IMG)
    [void]$img.SetAttribute('placement', 'appLogoOverride')
    [void]$img.SetAttribute('crop', 'circle')
  }
  $t = $x.GetElementsByTagName('text')
  [void]$t.Item(0).AppendChild($x.CreateTextNode($env:AN_TITLE))
  [void]$t.Item(1).AppendChild($x.CreateTextNode($env:AN_BODY))
  if ($env:AN_EXTRA -and $t.Length -gt 2) { [void]$t.Item(2).AppendChild($x.CreateTextNode($env:AN_EXTRA)) }
  $n = [Windows.UI.Notifications.ToastNotification]::new($x)
  [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($env:AN_APPID).Show($n)
} catch { Write-Error $_; exit 1 }
"#;

/// Bounds one popup command (`popupTimeout`).
const POPUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The logo disc size for toasts (`toastImageSize`).
const TOAST_IMAGE_SIZE: usize = 96;

/// The resolved popup mechanism. `Native` (rune-notify) is this port's
/// replacement for Go's native-Windows PowerShell toast; `PowerShell` keeps
/// the Go toast script for explicit-binary configs and WSL interop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PopupMode {
    Native,
    NotifySend,
    PowerShell,
}

/// Shows desktop notifications: a native Windows toast via rune-notify
/// (new in this port), a Windows toast via PowerShell (native Windows, or
/// from WSL through interop), or notify-send elsewhere (`sink.Popup`).
pub struct Popup {
    mode: PopupMode,
    binary: String,
    app_id: String,
    expire: Duration,
    dir: String,
    image: bool, // severity logo in toasts (explicit-binary PowerShell only)
    r: Arc<render::Renderer>,

    /// Optionally supplies a third toast line (space + fleet summary);
    /// wired by the CLI to the engine's live view (`Popup.extra`).
    extra: Option<Box<dyn Fn(&Event) -> String + Send + Sync>>,

    /// severity → rendered PNG path (`Popup.imgPath`, guarded by imgMu).
    img_path: Mutex<HashMap<String, String>>,
}

impl Popup {
    /// Resolves the popup mechanism (`sink.NewPopup`, popup.go:81). With no
    /// explicit Binary the order is: native rune-notify (Windows) →
    /// notify-send → powershell.exe (via WSL interop where applicable; on
    /// native Windows powershell.exe/pwsh.exe last, as in Go). It errors
    /// when nothing is available.
    pub fn new(cfg: &config::Sink, r: Arc<render::Renderer>) -> anyhow::Result<Popup> {
        let mut p = Popup {
            mode: PopupMode::PowerShell,
            binary: String::new(),
            app_id: cfg.app_id.clone(),
            expire: cfg.expire.d(),
            dir: proc::interop_dir()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default(),
            image: cfg.image.is_none() || cfg.image == Some(true),
            r,
            extra: None,
            img_path: Mutex::new(HashMap::new()),
        };
        // toast images need a Windows-visible path (Go: runtime.GOOS == "windows")
        p.image = p.image && cfg!(target_os = "windows");
        if p.app_id.is_empty() {
            p.app_id = DEFAULT_POWERSHELL_AUMID.to_string();
        }
        if !cfg.binary.is_empty() {
            let path = proc::look_path(&cfg.binary).ok_or_else(|| {
                anyhow::anyhow!(
                    "popup binary {:?}: executable file not found in $PATH",
                    cfg.binary
                )
            })?;
            p.binary = path.to_string_lossy().into_owned();
            let base = std::path::Path::new(&p.binary)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            p.mode = if base.contains("notify-send") {
                PopupMode::NotifySend
            } else {
                PopupMode::PowerShell
            };
            return Ok(p);
        }
        // Native → notify-send → powershell (Go's order, with the native
        // rune-notify path prepended on Windows by this port).
        if native_available(&cfg) {
            p.mode = PopupMode::Native;
            return Ok(p);
        }
        if let Some(path) = proc::look_path("notify-send") {
            p.binary = path.to_string_lossy().into_owned();
            p.mode = PopupMode::NotifySend;
            return Ok(p);
        }
        if cfg!(target_os = "windows") {
            for c in ["powershell.exe", "pwsh.exe"] {
                if let Some(path) = proc::look_path(c) {
                    p.binary = path.to_string_lossy().into_owned();
                    p.mode = PopupMode::PowerShell;
                    return Ok(p);
                }
            }
            anyhow::bail!("no PowerShell found in PATH for popups");
        }
        if let Some(path) = proc::look_path("powershell.exe") {
            p.binary = path.to_string_lossy().into_owned();
            p.mode = PopupMode::PowerShell;
            return Ok(p);
        }
        anyhow::bail!("no popup backend found (need notify-send or powershell.exe in PATH)")
    }

    /// Wires the optional third toast line (space + fleet summary)
    /// (`Popup.SetExtra`).
    pub fn set_extra(&mut self, f: Box<dyn Fn(&Event) -> String + Send + Sync>) {
        self.extra = Some(f);
    }

    /// Reports the resolved mechanism (`Popup.Mode`). Adds "native" to the
    /// Go values "powershell" | "notify-send".
    pub fn mode(&self) -> &'static str {
        match self.mode {
            PopupMode::Native => "native",
            PopupMode::NotifySend => "notify-send",
            PopupMode::PowerShell => "powershell",
        }
    }
}

impl Sink for Popup {
    fn name(&self) -> &'static str {
        "popup"
    }

    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()> {
        let (title, body) = self.r.render(ev);
        let extra = self.extra.as_ref().map_or(String::new(), |f| f(ev));
        let timeout = deadline.min(POPUP_TIMEOUT);

        match self.mode {
            PopupMode::NotifySend => {
                let mut body = body;
                if !extra.is_empty() {
                    body.push('\n');
                    body.push_str(&extra);
                }
                let mut argv = vec![self.binary.clone(), "-a".into(), "agent-notify".into()];
                if !self.expire.is_zero() {
                    argv.push("-t".into());
                    argv.push(self.expire.as_millis().to_string());
                }
                argv.push(title);
                argv.push(body);
                proc::run(&argv, &[], "", timeout).map(|_| ())
            }
            PopupMode::PowerShell => self.deliver_powershell(ev, &title, &body, &extra, timeout),
            #[cfg(target_os = "windows")]
            PopupMode::Native => self.deliver_native(&title, &body, &extra),
            #[cfg(not(target_os = "windows"))]
            PopupMode::Native => {
                let _ = (title, body, extra);
                unreachable!("native popup mode only resolves on Windows")
            }
        }
    }
}

impl Popup {
    /// PowerShell toast path — byte-for-byte the Go Deliver branch: the
    /// toast script with the payload in environment variables, run in the
    /// interop dir, stderr captured for the error snippet.
    fn deliver_powershell(
        &self,
        ev: &Event,
        title: &str,
        body: &str,
        extra: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let img = if self.image { self.toast_image(kind_severity(ev.kind)) } else { String::new() };

        let mut cmd = ProcessCommand::new(&self.binary);
        cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
            .arg(TOAST_SCRIPT);
        proc::child_attrs(&mut cmd); // windows: no console flash
        for kv in self.toast_env(title, body, extra, &img) {
            match kv.split_once('=') {
                Some((k, v)) => {
                    cmd.env(k, v);
                }
                None => anyhow::bail!("toast: malformed env entry {kv:?}"),
            }
        }
        if !self.dir.is_empty() {
            cmd.current_dir(&self.dir);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped()).stdin(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(toast_err(&e.to_string(), "")),
        };
        // Drain stderr on a thread so a chatty child cannot deadlock the pipe.
        let mut stderr_pipe = child.stderr.take();
        let stderr_t = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = stderr_pipe.as_mut() {
                use std::io::Read as _;
                let _ = p.read_to_end(&mut buf);
            }
            buf
        });

        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(toast_err(&e.to_string(), ""));
                }
            }
        };
        let stderr = String::from_utf8_lossy(&stderr_t.join().unwrap_or_default()).into_owned();

        match status {
            None => Err(toast_err("context deadline exceeded", &stderr)),
            Some(st) if !st.success() => {
                Err(toast_err(&go_exit_status(&st), &stderr))
            }
            Some(_) => Ok(()),
        }
    }

    /// Native toast path (Windows only): rune-notify posts the toast under
    /// the registered identity. The third toast line folds into the body
    /// (newline-joined, like the notify-send path) because rune-notify's
    /// toast has title+body only.
    #[cfg(target_os = "windows")]
    fn deliver_native(&self, title: &str, body: &str, extra: &str) -> anyhow::Result<()> {
        static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);
        let mut body = body.to_string();
        if !extra.is_empty() {
            body.push('\n');
            body.push_str(extra);
        }
        // A unique id per post: Go's PowerShell toasts never replace each
        // other, and rune-notify replaces by id.
        let id = format!("agent-notify-{}", TOAST_SEQ.fetch_add(1, Ordering::Relaxed));
        let n = rune_notify::Notification::new(id, title).body(body);
        if !rune_notify::post(&n) {
            anyhow::bail!("toast: post failed");
        }
        Ok(())
    }

    /// Builds the child process environment additions (`Popup.toastEnv`).
    /// Unlike Go (which returns the whole environ), this returns only the
    /// entries added on top of the inherited environment; Rust's Command
    /// inherits the parent environment unless overridden, so the result is
    /// the same child environment. Under WSL interop only WSLENV-listed
    /// variables cross the boundary, so ours are added there.
    fn toast_env(&self, title: &str, body: &str, extra: &str, img: &str) -> Vec<String> {
        let mut env = vec![
            format!("AN_TITLE={title}"),
            format!("AN_BODY={body}"),
            format!("AN_APPID={}", self.app_id),
        ];
        if !extra.is_empty() {
            env.push(format!("AN_EXTRA={extra}"));
        }
        if !img.is_empty() {
            env.push(format!("AN_IMG={img}"));
        }
        if !cfg!(target_os = "windows") {
            let existing = std::env::var("WSLENV").unwrap_or_default();
            let merged = if existing.is_empty() {
                "AN_TITLE:AN_BODY:AN_APPID:AN_EXTRA:AN_IMG".to_string()
            } else {
                format!("{existing}:AN_TITLE:AN_BODY:AN_APPID:AN_EXTRA:AN_IMG")
            };
            env.push(format!("WSLENV={merged}"));
        }
        env
    }

    /// Renders (and caches) the severity logo as a PNG in the temp
    /// directory (`Popup.toastImage`). Only used by the explicit-binary
    /// PowerShell path on native Windows, where temp_dir is a
    /// Windows-visible path the toast can read.
    fn toast_image(&self, severity: &str) -> String {
        let mut map = self.img_path.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = map.get(severity) {
            return path.clone();
        }
        let path = std::env::temp_dir().join(format!("agent-notify-toast-{severity}.png"));
        // Go writes with 0o644; std::fs::write uses 0o666 &~ umask, which is
        // 0o644 under the usual umask 022.
        if std::fs::write(&path, crate::icon::render_png(crate::icon::color(severity), TOAST_IMAGE_SIZE))
            .is_err()
        {
            return String::new();
        }
        let path = path.to_string_lossy().into_owned();
        map.insert(severity.to_string(), path.clone());
        path
    }
}

/// Formats a popup failure the Go way: `toast: {err}` or
/// `toast: {err}: {stderr-snippet}` trimmed and capped at 300 chars
/// (popup.go Deliver).
fn toast_err(err: &str, stderr: &str) -> anyhow::Error {
    let mut snip = stderr.trim();
    if snip.len() > 300 {
        snip = &snip[..300];
    }
    if snip.is_empty() {
        anyhow::anyhow!("toast: {err}")
    } else {
        anyhow::anyhow!("toast: {err}: {snip}")
    }
}

/// Go's `ExitStatus` error text: "exit status N" (Rust's Display says
/// "exit status: N").
fn go_exit_status(st: &std::process::ExitStatus) -> String {
    match st.code() {
        Some(code) => format!("exit status {code}"),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                format!("signal: {}", st.signal().unwrap_or(0))
            }
            #[cfg(not(unix))]
            {
                st.to_string()
            }
        }
    }
}

/// Resolves the native (rune-notify) toast path: true when the process
/// already owns a registered identity, or when one can be registered now
/// with the configured (or port-default) AppUserModelID. Never true off
/// Windows.
fn native_available(cfg: &config::Sink) -> bool {
    #[cfg(target_os = "windows")]
    {
        native_available_windows(cfg)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cfg;
        false
    }
}

#[cfg(target_os = "windows")]
fn native_available_windows(cfg: &config::Sink) -> bool {
    if rune_notify::available() {
        return true;
    }
    // The configured app_id only becomes the native identity when it is a
    // usable id (no path separator — PowerShell's AUMID default carries one
    // and must not be overwritten); otherwise the port's own id.
    let id = match cfg.app_id.as_str() {
        "" => NATIVE_APP_ID,
        a if a.contains('\\') => NATIVE_APP_ID,
        a => a,
    };
    let Some(icon) = identity_icon_path() else {
        return false;
    };
    let identity = rune_notify::Identity::new(id, "agent-notify").icon(icon);
    rune_notify::register(&identity).is_ok()
}

/// Renders the static identity icon (the "working"-blue severity disc) into
/// the temp directory for the registered identity's IconUri.
#[cfg(target_os = "windows")]
fn identity_icon_path() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("agent-notify-identity.png");
    std::fs::write(
        &path,
        crate::icon::render_png(crate::icon::color("working"), TOAST_IMAGE_SIZE),
    )
    .ok()?;
    Some(path)
}

/// Maps an event kind to the icon color it represents (`kindSeverity`).
fn kind_severity(k: event::Kind) -> &'static str {
    match k {
        event::Kind::AgentBlocked => "blocked",
        event::Kind::AgentWorking => "working",
        event::Kind::AgentIdle | event::Kind::AgentDone => "waiting",
        event::Kind::SessionDown => "down",
        event::Kind::Test => "working",
        _ => "idle",
    }
}

// ---------------------------------------------------------------------------
// Command (command.go)

/// Bounds a command sink execution when no explicit timeout is configured
/// (`defaultCommandTimeout`).
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs an external command per event. Each argv element is a template over
/// the event (Go: text/template). With Shell = true the single command
/// string runs through the OS shell (`sink.Command`).
pub struct Command {
    argv: Vec<render::Template>,
    shell: bool,
    timeout: Duration,
}

impl Command {
    /// Builds the sink from config, parsing templates eagerly
    /// (`sink.NewCommand`).
    pub fn new(cfg: &config::Sink) -> anyhow::Result<Command> {
        let mut timeout = cfg.timeout.d();
        if timeout.is_zero() {
            timeout = DEFAULT_COMMAND_TIMEOUT;
        }
        let mut argv = Vec::new();
        for a in &cfg.command {
            let t = render::Template::parse("arg", a)
                .map_err(|e| anyhow::anyhow!("command template: {e}"))?;
            argv.push(t);
        }
        Ok(Command { argv, shell: cfg.shell, timeout })
    }
}

impl Sink for Command {
    fn name(&self) -> &'static str {
        "command"
    }

    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()> {
        let mut args: Vec<String> = Vec::with_capacity(self.argv.len());
        for t in &self.argv {
            let s = t
                .execute(ev)
                .map_err(|e| anyhow::anyhow!("command template: {e}"))?;
            args.push(s);
        }
        if args.is_empty() {
            // Unreachable through validated config (Validate requires a
            // non-empty command); Go would panic on args[0] here.
            anyhow::bail!("run \"\": exec: no command");
        }

        let timeout = deadline.min(self.timeout);
        let (name, rest): (&str, Vec<String>) = if self.shell {
            if cfg!(target_os = "windows") {
                ("cmd", vec!["/c".into(), args[0].clone()])
            } else {
                ("sh", vec!["-c".into(), args[0].clone()])
            }
        } else {
            (&args[0], args[1..].to_vec())
        };

        let (out, err) = combined_output(name, &rest, timeout);
        match err {
            Some(err) => {
                // Go: snip = string(out) cut at 200 bytes (not trimmed).
                let mut snip = out.as_slice();
                if snip.len() > 200 {
                    snip = &snip[..200];
                }
                let snip = String::from_utf8_lossy(snip);
                if snip.is_empty() {
                    anyhow::bail!("run {:?}: {err}", args[0]);
                }
                anyhow::bail!("run {:?}: {err}: {snip}", args[0]);
            }
            None => Ok(()),
        }
    }
}

/// Runs argv capturing stdout+stderr and killing the child after `timeout`
/// (Go's `CommandContext(...).CombinedOutput()` with a timeout context).
/// Returns the output and, on failure, the Go-style error text ("exit
/// status N", "context deadline exceeded", or the exec lookup failure).
///
/// PORT-NOTE: Go's CombinedOutput interleaves both streams through one
/// pipe; std has no shared pipe, so the output is stdout followed by
/// stderr — the bytes are the same, the interleaving order is not.
fn combined_output(name: &str, args: &[String], timeout: Duration) -> (Vec<u8>, Option<String>) {
    let mut cmd = ProcessCommand::new(name);
    cmd.args(args);
    proc::child_attrs(&mut cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), Some(format!("exec: {name:?}: executable file not found in $PATH")));
        }
        Err(e) => return (Vec::new(), Some(e.to_string())),
    };

    use std::io::Read as _;
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

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return (Vec::new(), Some(e.to_string()));
            }
        }
    };
    let mut out = stdout_t.join().unwrap_or_default();
    out.extend_from_slice(&stderr_t.join().unwrap_or_default());

    match status {
        None => (out, Some("context deadline exceeded".to_string())),
        Some(st) if !st.success() => (out, Some(go_exit_status(&st))),
        Some(_) => (out, None),
    }
}

// ---------------------------------------------------------------------------
// Webhook (webhook.go)

/// Bounds a webhook request when no explicit timeout is configured
/// (`defaultWebhookTimeout`).
const DEFAULT_WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// POSTs every event to an HTTP endpoint: the JSON event by default, or a
/// rendered body_template for services with fixed payload shapes (Discord,
/// Slack, ntfy, Home Assistant, ...) (`sink.Webhook`).
pub struct Webhook {
    url: String,
    method: String,
    headers: HashMap<String, String>,
    content_type: String,
    body_tmpl: Option<render::Template>,
    client: ureq::Agent,
    timeout: Duration,
}

impl Webhook {
    /// Builds the sink from config (`sink.NewWebhook`).
    pub fn new(cfg: &config::Sink) -> anyhow::Result<Webhook> {
        let timeout = {
            let t = cfg.timeout.d();
            if t.is_zero() {
                DEFAULT_WEBHOOK_TIMEOUT
            } else {
                t
            }
        };
        let mut w = Webhook {
            url: cfg.url.clone(),
            method: if cfg.method.is_empty() { "POST".to_string() } else { cfg.method.clone() },
            headers: cfg.headers.clone(),
            content_type: cfg.content_type.clone(),
            body_tmpl: None,
            client: ureq::AgentBuilder::new().timeout(timeout).build(),
            timeout,
        };
        if !cfg.body_template.is_empty() {
            let t = render::Template::parse("body", &cfg.body_template)
                .map_err(|e| anyhow::anyhow!("webhook body_template: {e}"))?;
            w.body_tmpl = Some(t);
            if w.content_type.is_empty() {
                w.content_type = "application/json".to_string();
            }
        }
        Ok(w)
    }
}

impl Sink for Webhook {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()> {
        let (body, content_type) = if let Some(t) = &self.body_tmpl {
            let s = t
                .execute(ev)
                .map_err(|e| anyhow::anyhow!("webhook body_template: {e}"))?;
            let ct = if !self.content_type.is_empty() {
                self.content_type.clone()
            } else {
                "application/json".to_string()
            };
            (s.into_bytes(), ct)
        } else {
            (serde_json::to_vec(ev)?, "application/json".to_string())
        };

        let mut req = self
            .client
            .request(&self.method, &self.url)
            .set("Content-Type", &content_type)
            .set("User-Agent", "agent-notify")
            // Go bounds the request by both the client timeout and the
            // manager's ctx; the effective bound is the tighter one.
            .timeout(deadline.min(self.timeout));
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        match req.send_bytes(&body) {
            Ok(_) => Ok(()),
            // Go gets the response and errors on StatusCode >= 400; ureq
            // surfaces the same condition as Error::Status.
            Err(ureq::Error::Status(code, resp)) => {
                anyhow::bail!("webhook {}: HTTP {} {}", self.url, code, resp.status_text())
            }
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Log & Bell (log.go)

/// Prints one line per event to a writer: stdout for `monitor`, stderr as a
/// config sink (`sink.Log`).
pub struct Log {
    w: Mutex<Box<dyn Write + Send>>,
    json: bool,
}

impl Log {
    /// Builds the log sink (`sink.NewLog`).
    pub fn new(w: Box<dyn Write + Send>, json_mode: bool) -> Log {
        Log { w: Mutex::new(w), json: json_mode }
    }
}

impl Sink for Log {
    fn name(&self) -> &'static str {
        "log"
    }

    fn deliver(&self, ev: &Event, _deadline: Duration) -> anyhow::Result<()> {
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        if self.json {
            let data = serde_json::to_string(ev)?;
            writeln!(w, "{data}")?;
            return Ok(());
        }
        let arrow = if !ev.from.is_empty() || !ev.to.is_empty() {
            format!("{}→{}", ev.from, ev.to)
        } else {
            String::new()
        };
        write!(
            w,
            "{}  {:<16} {:<14} {:<8} {:<18} {}\n",
            local_hhmmss(ev.time),
            format!("{}/{}", ev.host, ev.session),
            ev.kind.as_str(),
            ev.agent,
            arrow,
            ev.title,
        )?;
        Ok(())
    }
}

/// Writes an ASCII BEL to a writer on every event (`sink.Bell`).
pub struct Bell {
    w: Mutex<Box<dyn Write + Send>>,
}

impl Bell {
    /// Builds the bell sink (`sink.NewBell`).
    pub fn new(w: Box<dyn Write + Send>) -> Bell {
        Bell { w: Mutex::new(w) }
    }
}

impl Sink for Bell {
    fn name(&self) -> &'static str {
        "bell"
    }

    fn deliver(&self, _ev: &Event, _deadline: Duration) -> anyhow::Result<()> {
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(b"\x07")?; // ASCII BEL
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// local-time formatting for the Log sink

/// Formats t as the local wall clock "HH:MM:SS" — Go's
/// `ev.Time.Format("15:04:05")`, which renders in the event's location
/// (local). std has no timezone access, so this goes through libc's
/// localtime; it falls back to UTC when the lookup fails.
fn local_hhmmss(t: event::Time) -> String {
    let secs = t.secs as i64; // fits i64 until year 292 billion
    #[cfg(unix)]
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&secs, &mut tm).is_null() {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    #[cfg(windows)]
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_s(&mut tm, &secs) == 0 {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    let s = t.secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

// ---------------------------------------------------------------------------
// Sink construction (cli.buildSinks, cli.go:218)

/// The result of [`build_sinks`]: the sink list plus the tray sink typed
/// out. The CLI's tray command has to reach `Tray`'s UI surface (`run`,
/// `init_flyout`, the update/setup hooks) and a `Arc<dyn Sink>` list can't
/// hand those over — Go type-asserts the `*tray.Tray` out of the sink slice
/// in cmdTray instead.
pub struct BuiltSinks {
    /// All sinks in config order (`cli.buildSinks`'s return value).
    pub sinks: Vec<Arc<dyn Sink>>,
    /// The tray sink, when `with_tray` built one.
    pub tray: Option<Arc<crate::tray::Tray>>,
}

/// Builds the sink list from config, in order (`cli.buildSinks`). `with_tray`
/// adds/keeps the tray sink; headless commands pass false and tray sinks are
/// skipped with a note. Warnings go to stderr (Go logs through slog).
pub fn build_sinks(
    cfg: &config::Config,
    eng: &std::sync::Arc<crate::engine::Engine>,
    with_tray: bool,
) -> anyhow::Result<BuiltSinks> {
    let r = Arc::new(
        render::Renderer::new(&cfg.render.title, &cfg.render.body)
            .map_err(|e| anyhow::anyhow!("render templates: {e}"))?,
    );
    let mut tray_added = false;
    let mut tray_sink: Option<Arc<crate::tray::Tray>> = None;
    let mut sinks: Vec<Arc<dyn Sink>> = Vec::new();
    for sc in &cfg.sink {
        if sc.disabled {
            continue;
        }
        match sc.sink_type.as_str() {
            "tray" => {
                if !with_tray {
                    eprintln!("agent-notify: skipping tray sink in headless mode");
                    continue;
                }
                if tray_added {
                    continue;
                }
                tray_added = true;
                let mut pop = None;
                if sc.popup.is_none() || sc.popup == Some(true) {
                    match Popup::new(sc, Arc::clone(&r)) {
                        Ok(mut p) => {
                            p.set_extra(crate::cli::fleet_extra(eng));
                            pop = Some(Arc::new(p) as Arc<dyn Sink>);
                        }
                        Err(err) => {
                            eprintln!(
                                "agent-notify: attention popups unavailable; tray will be icon-only error={err}"
                            );
                        }
                    }
                }
                let tray = Arc::new(crate::tray::Tray::new(Arc::clone(eng), pop));
                sinks.push(Arc::clone(&tray) as Arc<dyn Sink>);
                tray_sink = Some(tray);
            }
            "popup" => {
                let mut pop = Popup::new(sc, Arc::clone(&r))?;
                pop.set_extra(crate::cli::fleet_extra(eng));
                sinks.push(Arc::new(pop));
            }
            "bell" => {
                sinks.push(Arc::new(Bell::new(Box::new(std::io::stderr()))));
            }
            "command" => {
                sinks.push(Arc::new(Command::new(sc)?));
            }
            "webhook" => {
                sinks.push(Arc::new(Webhook::new(sc)?));
            }
            "log" => {
                sinks.push(Arc::new(Log::new(Box::new(std::io::stderr()), false)));
            }
            // Go's switch has no default; config.Validate rejects unknown
            // types before we get here.
            _ => {}
        }
    }
    if with_tray && !tray_added {
        // The tray command always shows the tray, config or not.
        let pop = match Popup::new(&config::Sink::default(), Arc::clone(&r)) {
            Ok(mut p) => {
                p.set_extra(crate::cli::fleet_extra(eng));
                Some(Arc::new(p) as Arc<dyn Sink>)
            }
            Err(err) => {
                eprintln!(
                    "agent-notify: attention popups unavailable; tray will be icon-only error={err}"
                );
                None
            }
        };
        let tray = Arc::new(crate::tray::Tray::new(Arc::clone(eng), pop));
        sinks.push(Arc::clone(&tray) as Arc<dyn Sink>);
        tray_sink = Some(tray);
    }
    Ok(BuiltSinks { sinks, tray: tray_sink })
}

// ---------------------------------------------------------------------------
// Tests (ports of internal/sink/popup_test.go and webhook_test.go)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Kind, Time};

    fn test_renderer() -> Arc<render::Renderer> {
        Arc::new(render::Renderer::new("", "").unwrap())
    }

    fn test_popup(app_id: &str) -> Popup {
        Popup {
            mode: PopupMode::PowerShell,
            binary: String::new(),
            app_id: app_id.to_string(),
            expire: Duration::ZERO,
            dir: String::new(),
            image: false,
            r: test_renderer(),
            extra: None,
            img_path: Mutex::new(HashMap::new()),
        }
    }

    /// testEvent (webhook_test.go)
    fn test_event() -> Event {
        Event {
            kind: Kind::AgentBlocked,
            time: Time::now(),
            source: String::new(),
            host: "local".into(),
            session: "work".into(),
            agent: "claude".into(),
            from: "working".into(),
            to: "blocked".into(),
            title: "Refactor auth module".into(),
            project: "api".into(),
            pane_id: "w1:p1".into(),
            focused: false,
        }
    }

    /// One captured HTTP request (stands in for httptest's introspection).
    struct CapturedRequest {
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// A one-shot local HTTP server standing in for net/http/httptest.
    /// Returns the base URL and a channel delivering the captured request.
    fn serve_once(status_line: &'static str) -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = stream;
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            use std::io::{BufRead as _, Read as _};
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let method = request_line.split_whitespace().next().unwrap_or("").to_string();
            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                    if k.eq_ignore_ascii_case("content-length") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let _ = tx.send(CapturedRequest { method, headers, body });
            let _ = stream.write_all(
                format!("HTTP/1.1 {status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
        });
        (format!("http://{addr}"), rx)
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// TestToastEnvCarriesPayload
    #[test]
    fn toast_env_carries_payload() {
        let p = test_popup("appid");
        let env = p.toast_env("title", "body", "extra", "img");
        let joined = env.join("\n");
        for want in ["AN_TITLE=title", "AN_BODY=body", "AN_APPID=appid", "AN_EXTRA=extra", "AN_IMG=img"] {
            assert!(joined.contains(want), "env missing {want}:\n{joined}");
        }
    }

    /// TestToastEnvOmitsAbsentParts
    #[test]
    fn toast_env_omits_absent_parts() {
        let p = test_popup("appid");
        let env = p.toast_env("t", "b", "", "");
        for s in &env {
            assert!(
                !s.starts_with("AN_EXTRA=") && !s.starts_with("AN_IMG="),
                "unexpected entry {s:?}"
            );
        }
    }

    /// TestExtraLineComposition
    #[test]
    fn extra_line_composition() {
        // the CLI wires this closure; assert its shape
        let summary = "2 working · 1 blocked";
        let extra = |ev: &Event| {
            let mut line = format!("{}/{}", ev.host, ev.session);
            if !ev.project.is_empty() {
                line = format!("{} · {}", ev.project, line);
            }
            format!("{line} — {summary}")
        };
        let mut e = test_event();
        e.project = "api".into();
        let got = extra(&e);
        assert_eq!(got, "api · local/work — 2 working · 1 blocked");
    }

    /// TestKindSeverity
    #[test]
    fn kind_severity_table() {
        let cases = [
            (Kind::AgentBlocked, "blocked"),
            (Kind::AgentWorking, "working"),
            (Kind::AgentIdle, "waiting"),
            (Kind::AgentDone, "waiting"),
            (Kind::SessionDown, "down"),
            (Kind::Test, "working"),
        ];
        for (kind, want) in cases {
            assert_eq!(kind_severity(kind), want, "kindSeverity({kind:?})");
        }
    }

    /// TestWebhookJSONBody
    #[test]
    fn webhook_json_body() {
        let (url, rx) = serve_once("200 OK");
        let mut headers = HashMap::new();
        headers.insert("X-Token".to_string(), "s3cret".to_string());
        let wh = Webhook::new(&config::Sink { url, headers, ..config::Sink::default() }).unwrap();
        wh.deliver(&test_event(), DELIVER_TIMEOUT).expect("deliver");

        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(req.method, "POST", "method");
        assert_eq!(header(&req.headers, "Content-Type"), "application/json", "content-type");
        assert_eq!(header(&req.headers, "User-Agent"), "agent-notify", "user-agent");
        assert_eq!(header(&req.headers, "X-Token"), "s3cret");
        let got: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(got["kind"], "agent_blocked");
        assert_eq!(got["agent"], "claude");
        assert_eq!(got["from"], "working");
        assert_eq!(got["to"], "blocked");
        assert_eq!(got["project"], "api");
        assert_eq!(got["session"], "work");
        assert_eq!(got["host"], "local");
    }

    /// TestWebhookTemplateBodyAndHeaders
    #[test]
    fn webhook_template_body_and_headers() {
        let (url, rx) = serve_once("200 OK");
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer tok".to_string());
        let wh = Webhook::new(&config::Sink {
            url,
            method: "PUT".into(),
            headers,
            body_template: r#"{"text": "{{.Agent}} {{.Verb}}"}"#.into(),
            content_type: "application/json".into(),
            ..config::Sink::default()
        })
        .unwrap();
        wh.deliver(&test_event(), DELIVER_TIMEOUT).expect("deliver");

        let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(String::from_utf8_lossy(&req.body), r#"{"text": "claude is blocked"}"#);
        assert_eq!(header(&req.headers, "Authorization"), "Bearer tok");
        assert_eq!(header(&req.headers, "Content-Type"), "application/json");
    }

    /// TestWebhookErrorStatus
    #[test]
    fn webhook_error_status() {
        let (url, _rx) = serve_once("403 Forbidden");
        let wh = Webhook::new(&config::Sink { url, ..config::Sink::default() }).unwrap();
        assert!(
            wh.deliver(&test_event(), DELIVER_TIMEOUT).is_err(),
            "expected error on 403"
        );
    }

    /// TestCommandSinkArgvTemplate
    #[test]
    fn command_sink_argv_template() {
        let dir = test_dir();
        let marker = dir.join("{{.Kind}}.marker");
        let c = Command::new(&config::Sink {
            command: vec!["touch".into(), marker.to_string_lossy().into_owned()],
            ..config::Sink::default()
        })
        .unwrap();
        c.deliver(&test_event(), DELIVER_TIMEOUT).expect("deliver");
        assert!(
            dir.join("agent_blocked.marker").is_file(),
            "templated argv not executed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TestCommandSinkShell
    #[test]
    fn command_sink_shell() {
        let dir = test_dir();
        let out = dir.join("out.txt");
        let c = Command::new(&config::Sink {
            shell: true,
            // (the doubled braces are format!'s escapes — the shell string is
            // `printf {{.Agent}} > …`, like the Go test literal)
            command: vec![format!("printf {{{{.Agent}}}} > {}", out.display())],
            ..config::Sink::default()
        })
        .unwrap();
        c.deliver(&test_event(), DELIVER_TIMEOUT).expect("deliver");
        let data = std::fs::read(&out).unwrap();
        assert_eq!(String::from_utf8_lossy(&data), "claude", "shell output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TestCommandSinkFailure
    #[test]
    fn command_sink_failure() {
        let c = Command::new(&config::Sink {
            command: vec!["sh".into(), "-c".into(), "exit 3".into()],
            ..config::Sink::default()
        })
        .unwrap();
        assert!(
            c.deliver(&test_event(), DELIVER_TIMEOUT).is_err(),
            "expected failure surfaced"
        );
    }

    /// A unique temp directory for one test (stands in for t.TempDir).
    fn test_dir() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-notify-sink-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Manager.Deliver never blocks: QUEUE_SIZE sends buffer, further ones
    /// drop (logging) instead of blocking, like Go's select-default
    /// (sink.go:59). A hang fails the test by timeout.
    #[test]
    fn manager_deliver_drops_on_full_queue() {
        struct Noop;
        impl Sink for Noop {
            fn name(&self) -> &'static str {
                "noop"
            }
            fn deliver(&self, _ev: &Event, _deadline: Duration) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let mut mgr = Manager::new();
        mgr.add(Arc::new(Noop));
        assert_eq!(mgr.names(), vec!["noop".to_string()]);

        let ev = test_event();
        for _ in 0..=QUEUE_SIZE {
            mgr.deliver(&ev); // the last one finds the queue full and drops
        }
    }

    /// Manager.Run delivers queued events on a worker thread, tolerates a
    /// failing sink, and stops cleanly (sink.go:70).
    #[test]
    fn manager_run_delivers_and_stops() {
        struct Recording {
            got: Mutex<Vec<String>>,
        }
        impl Sink for Recording {
            fn name(&self) -> &'static str {
                "recording"
            }
            fn deliver(&self, ev: &Event, _deadline: Duration) -> anyhow::Result<()> {
                self.got.lock().unwrap().push(ev.kind.as_str().to_string());
                Ok(())
            }
        }
        struct Failing;
        impl Sink for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn deliver(&self, _ev: &Event, _deadline: Duration) -> anyhow::Result<()> {
                anyhow::bail!("boom")
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut mgr = Manager::new();
        let rec = Arc::new(Recording { got: Mutex::new(Vec::new()) });
        mgr.add(Arc::clone(&rec) as Arc<dyn Sink>);
        mgr.add(Arc::new(Failing)); // errors are logged, not fatal

        let ev = test_event();
        std::thread::scope(|s| {
            let m = &mgr;
            let stop2 = Arc::clone(&stop);
            s.spawn(move || m.run(stop2));
            mgr.deliver(&ev);
            for _ in 0..200 {
                if !rec.got.lock().unwrap().is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::Relaxed);
        });
        // scope exit means run() returned after joining its workers

        assert_eq!(*rec.got.lock().unwrap(), vec!["agent_blocked".to_string()]);
    }
}
