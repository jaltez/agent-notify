//! The command-line surface. Port of `internal/cli/cli.go` plus the whole
//! per-platform helper set of that package (console_*, singleinstance_*,
//! setuplaunch_*, tty_*) and the `setup`/`uninstall` wizard
//! (`setup_cmd.go`). `main.rs` carries `main.go`'s entry contract.
//!
//! Go threads `context.Context` through commands; the Rust port uses an
//! `Arc<AtomicBool>` stop flag that the SIGINT/SIGTERM handlers (Go:
//! `signal.NotifyContext`) set through a relay thread, and explicit
//! timeouts where Go used `context.WithTimeout`.
//!
//! PORT-NOTE (engine → manager): the Go CLI never bridges `eng.Events()`
//! into `mgr.Deliver` — verified in cli.go at every revision (`git log -S
//! "eng.Events()" -- internal/cli` is empty) and empirically: the released
//! Go binary's `monitor` prints only the two log lines, and tray popups
//! fire solely for the synthetic events below (update/setup/test). Engine
//! events pile up in the engine's buffered channel and drop. This port
//! replicates that exactly: no pump is added. If the Go grows one, port it
//! alongside.
//!
//! PORT-NOTE (flytest): Go drives `tray.NewFlyoutForTest(eng).SelfTest()`,
//! which on Windows dumps hwnd/rect/foreground/paint counters. The Rust
//! panel is a rune App on every platform, so the port runs a simplified
//! driver: open the panel once, log the visibility diagnostics, close it.
//! The Win32-only window introspection is not replicated.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rune::{AnyWindowSpec, Display, RunOptions, WindowSpec};

use crate::buildinfo;
use crate::config;
use crate::engine::{self, Engine};
use crate::event::{self, Event, Kind, Time};
use crate::flyout::Flyout;
use crate::render;
use crate::setup;
use crate::sink::{self, Manager};
use crate::tray::Tray;
use crate::update::{self, Checker, Status};

/// Bounds one update check (update.go bounds Check with a 20s context).
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// The daemon/tray background update cadence (cli.go wireUpdates /
/// daemonUpdateLoop's 24h ticker).
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// The setup nudge delay (cli.cmdTray's 10s goroutine).
const NUDGE_DELAY: Duration = Duration::from_secs(10);

/// The usage text (cli.go's `usage` const), byte-for-byte.
const USAGE: &str = "agent-notify: tray notifier for AI coding agents (herdr today)

Usage:
  agent-notify [flags] [command]

Commands:
  (default)   tray icon + attention popups (the normal way to run it)
  run         headless daemon (no tray; configured sinks only)
  monitor     print events as they happen; no notifications
  probe       one pass: show sessions, agents and sink availability
  test        send a test notification through every configured sink
  init        write an annotated example config
  setup       first-run wizard: install, PATH, autostart/service, WSL
              (--status shows the current state; --yes takes defaults)
  uninstall   undo setup (autostart, service, binaries; --purge-config)
  config      path | edit | validate the configuration
  update      self-update from GitHub releases (--check to only look)
  version     print version information
  flytest     diagnostics: drive the flyout show/hide cycle
  help        show this help

Flags:
  --config PATH   config file (default: $AGENT_NOTIFY_CONFIG or
                  <user config dir>/agent-notify/config.toml)
  -v, --verbose   debug logging
";

/// version is sourced from buildinfo (injected at build time)
/// (cli.version).
fn version() -> String {
    buildinfo::VERSION.to_string()
}

/// The compiler identity `cmd_version` prints where Go prints
/// runtime.Version(). build.rs captures `rustc --version` at build time
/// (Go bakes runtime.Version() into the binary); without it the fallback
/// is the bare toolchain name.
fn rustc_version() -> &'static str {
    option_env!("RUSTC_VERSION").unwrap_or("rustc")
}

/// runtime.GOOS for the version line: Go prints "darwin" where Rust's
/// const is "macos" (linux/windows match).
fn goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// runtime.GOARCH for the version line: Go prints "amd64"/"arm64"/"386"
/// where Rust's consts are "x86_64"/"aarch64"/"x86".
fn goarch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Logger (cli.newLogger + Go slog.NewTextHandler's output shape)

/// The two levels the CLI actually uses (slog.LevelInfo / LevelDebug);
/// Warn/Error exist for the attr-bearing call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Debug = 0,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

struct LoggerInner {
    w: Mutex<Box<dyn Write + Send>>,
    level: Level,
}

/// The CLI logger (`cli.newLogger`): slog TextHandler-format lines, Info
/// level, Debug with -v. Clones share one writer; hook threads capture
/// their own clone (Go passes the *slog.Logger around).
#[derive(Clone)]
pub struct Logger {
    inner: Arc<LoggerInner>,
}

impl Logger {
    /// newLogger(stderr, verbose): Info level, Debug when -v.
    pub fn new(w: Box<dyn Write + Send>, verbose: bool) -> Logger {
        Logger {
            inner: Arc::new(LoggerInner {
                w: Mutex::new(w),
                level: if verbose { Level::Debug } else { Level::Info },
            }),
        }
    }

    fn log(&self, level: Level, msg: &str, attrs: &[(&str, String)]) {
        if level < self.inner.level {
            return;
        }
        let mut line = format!(
            "time={} level={} msg={}",
            slog_time(),
            level.as_str(),
            quote_slog(msg)
        );
        for (k, v) in attrs {
            line.push(' ');
            line.push_str(&quote_slog(k));
            line.push('=');
            line.push_str(&quote_slog(v));
        }
        line.push('\n');
        let mut w = self.inner.w.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(line.as_bytes());
    }

    pub fn debug(&self, msg: &str) {
        self.log(Level::Debug, msg, &[]);
    }

    pub fn debug_attrs(&self, msg: &str, attrs: &[(&str, String)]) {
        self.log(Level::Debug, msg, attrs);
    }

    pub fn info(&self, msg: &str) {
        self.log(Level::Info, msg, &[]);
    }

    pub fn info_attrs(&self, msg: &str, attrs: &[(&str, String)]) {
        self.log(Level::Info, msg, attrs);
    }

    pub fn warn(&self, msg: &str) {
        self.log(Level::Warn, msg, &[]);
    }

    pub fn warn_attrs(&self, msg: &str, attrs: &[(&str, String)]) {
        self.log(Level::Warn, msg, attrs);
    }

    pub fn error(&self, msg: &str) {
        self.log(Level::Error, msg, &[]);
    }

    pub fn error_attrs(&self, msg: &str, attrs: &[(&str, String)]) {
        self.log(Level::Error, msg, attrs);
    }
}

/// slog's TextHandler quotes a key/value when it holds anything outside its
/// safe set: JSON-unsafe ASCII plus space and '='; valid UTF-8 multibyte
/// runes stay raw (needsQuoting in log/slog). The quoted form mirrors
/// strconv.Quote via Rust's Debug formatting.
fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    for b in s.bytes() {
        if b == b' ' || b == b'=' || b == b'"' || b == b'\\' || b < 0x20 || b == 0x7f {
            return true;
        }
    }
    false
}

fn quote_slog(s: &str) -> String {
    if needs_quoting(s) {
        format!("{s:?}")
    } else {
        s.to_string()
    }
}

/// The TextHandler timestamp: local RFC 3339 with millisecond precision
/// (`2026-11-11T22:07:52.478+01:00`). std has no timezone access, so libc's
/// localtime supplies the wall clock and UTC offset (the log sink's local
/// stamps do the same — see sink.rs); without an offset (MSVCRT's tm)
/// the stamp falls back to UTC.
fn slog_time() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    #[cfg(unix)]
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&secs, &mut tm).is_null() {
            let (sign, off) = if tm.tm_gmtoff < 0 {
                ('-', -tm.tm_gmtoff)
            } else {
                ('+', tm.tm_gmtoff)
            };
            return format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                ms,
                sign,
                off / 3600,
                (off % 3600) / 60
            );
        }
    }
    #[cfg(windows)]
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_s(&mut tm, &secs) == 0 {
            // PORT-NOTE: MSVCRT's tm carries no UTC offset, so the stamp
            // falls back to Z where Go prints the local offset.
            return format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                ms
            );
        }
    }
    let _ = secs;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// Days since epoch → (year, month, day). Howard Hinnant's civil_from_days,
/// for the UTC fallback stamp.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Run + dispatch (cli.Run, cli.go:62)

/// Executes the CLI and returns the process exit code (`cli.Run`,
/// cli.go:62). `argv` carries the full argument vector including the
/// program name (Go: os.Args); it is skipped here like Go's argv[1:].
/// On Windows the tray/flyout windows live on the main OS thread's message
/// queue — `cmd_tray` keeps the rune event loop on the calling (main)
/// thread, which `main` guarantees (Go pins it with runtime.LockOSThread;
/// Rust's initial thread never migrates).
pub fn run(argv: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    attach_parent_console(); // windowsgui builds: reattach for CLI subcommands
    let args: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };
    let mut config_path = String::new();
    let mut verbose = false;
    let mut i = 0usize;
    while i < args.len() {
        let a = &args[i];
        if a == "--config" && i + 1 < args.len() {
            config_path = args[i + 1].clone();
            i += 2;
        } else if let Some(v) = a.strip_prefix("--config=") {
            config_path = v.to_string();
            i += 1;
        } else if a == "-v" || a == "--verbose" {
            verbose = true;
            i += 1;
        } else if a == "-h" || a == "--help" || a == "help" {
            let _ = write!(stdout, "{USAGE}");
            return 0;
        } else if a == "--version" {
            return cmd_version(stdout);
        } else {
            break; // subcommand (or stray flag): Go's goto subcommand
        }
    }
    let rest: Vec<String> = args[i.min(args.len())..].to_vec();
    let mut cmd = "tray".to_string();
    let mut sub: Vec<String> = Vec::new();
    if !rest.is_empty() {
        cmd = rest[0].clone();
        sub = rest[1..].to_vec();
    }

    let log = Logger::new(Box::new(std::io::stderr()), verbose);
    match cmd.as_str() {
        "tray" => cmd_tray(&config_path, &log, stderr),
        "run" => cmd_run(&config_path, &log),
        "monitor" => cmd_monitor(&config_path, &sub, stdout, &log),
        "probe" => cmd_probe(&config_path, stdout, &log),
        "test" => cmd_test(&config_path, stdout, &log),
        "init" => cmd_init(&config_path, &sub, stdout),
        "setup" => cmd_setup(&config_path, &sub, stdout, stderr, &log),
        "uninstall" => cmd_uninstall(&config_path, &sub, stdout, stderr),
        "version" => cmd_version(stdout),
        "flytest" => cmd_flytest(&config_path, stdout, &log),
        "update" => cmd_update(&sub, stdout),
        "config" => cmd_config(&config_path, &sub, stdout),
        _ => {
            let _ = write!(stderr, "unknown command {cmd:?}\n\n{USAGE}");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// Config + engine + sinks (cli.loadConfig / buildEngine / buildSinks)

/// Resolves and loads the config, logging hints on the way
/// (`cli.loadConfig`, cli.go:140).
pub fn load_config(path: &str, log: &Logger) -> config::Config {
    let (p, source) = config::resolve(path);
    let Some(p) = p else {
        log.info("using built-in defaults (no config file)");
        return config::Config::default_config();
    };
    if !p.exists() {
        log.info_attrs(
            "config not found; using defaults",
            &[("path", p.display().to_string())],
        );
        return config::Config::default_config();
    }
    if source == "windows-shared" {
        log.info_attrs(
            "using Windows-side config (WSL auto-share)",
            &[("path", p.display().to_string())],
        );
    }
    match config::Config::load(&p) {
        Ok((cfg, warnings)) => {
            for w in warnings {
                log.warn(&w);
            }
            cfg
        }
        Err(err) => {
            log.error_attrs(
                "failed to load config; using defaults",
                &[("error", err.to_string())],
            );
            config::Config::default_config()
        }
    }
}

/// Assembles the engine from config (`cli.buildEngine`, cli.go:170).
/// `kinds_override` (Go: nil | []string) replaces the configured event set.
/// PORT-NOTE: Go passes `engine.WithLogger(log)`; the Rust engine has no
/// logger injection and logs through its own warnf/debugf to stderr.
pub fn build_engine(
    cfg: &config::Config,
    kinds_override: Option<&[String]>,
) -> anyhow::Result<Arc<Engine>> {
    let names: &[String] = kinds_override.unwrap_or(&cfg.events);
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let kinds = event::parse_set(&name_refs).map_err(|e| anyhow::anyhow!("{e}"))?;
    let eng = Engine::new(engine::Config {
        kinds: kind_set(&kinds),
        cooldown: cfg.cooldown.d(),
        include: cfg.herdr.include.clone(),
        exclude: cfg.herdr.exclude.clone(),
        herdr: cfg.herdr.clone(),
    })?;
    Ok(Arc::new(eng))
}

/// The kind allow-set for the engine (`cli.kindSet`, cli.go:188).
pub fn kind_set(ks: &[Kind]) -> std::collections::HashSet<Kind> {
    ks.iter().copied().collect()
}

/// Extra fleet summary appended to popups (`cli.fleetExtra`, cli.go:200).
/// Takes the shared engine handle because the returned closure must be
/// 'static (popup sinks outlive the call).
pub fn fleet_extra(eng: &Arc<Engine>) -> Box<dyn Fn(&Event) -> String + Send + Sync> {
    let eng = Arc::clone(eng);
    Box::new(move |ev: &Event| {
        let mut parts: Vec<String> = Vec::new();
        if !ev.project.is_empty() {
            parts.push(ev.project.clone());
        }
        if !ev.host.is_empty() || !ev.session.is_empty() {
            parts.push(format!("{}/{}", ev.host, ev.session));
        }
        let line = parts.join(" \u{b7} "); // " · "
        let summary = eng.view().summary();
        if line.is_empty() {
            return summary;
        }
        format!("{line} — {summary}")
    })
}

// ---------------------------------------------------------------------------
// Tray command (cli.cmdTray, cli.go:289 + wireUpdates, cli.go:369)

/// The tray command: builds sinks, runs the tray + engine until quit
/// (`cli.cmdTray`, cli.go:289). Wires the background update check through
/// [`wire_updates`] and the first-run setup nudge.
fn cmd_tray(config_path: &str, log: &Logger, stderr: &mut dyn Write) -> i32 {
    if !gui_available() {
        let _ = writeln!(
            stderr,
            "no display available for the tray icon; use `agent-notify run` or `agent-notify monitor`"
        );
        return 1;
    }
    let _lock = match acquire_tray_lock() {
        Ok(l) => l,
        Err(err) => {
            let _ = writeln!(stderr, "{err}");
            return 1;
        }
    };
    let cfg = load_config(config_path, log);
    let eng = match build_engine(&cfg, None) {
        Ok(e) => e,
        Err(err) => {
            log.error_attrs("cannot start", &[("error", err.to_string())]);
            return 1;
        }
    };
    let built = match sink::build_sinks(&cfg, &eng, true) {
        Ok(b) => b,
        Err(err) => {
            log.error_attrs("invalid sink configuration", &[("error", err.to_string())]);
            return 1;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    install_signals(Arc::clone(&stop));

    let mut mgr = Manager::new();
    for s in &built.sinks {
        mgr.add(Arc::clone(s));
    }
    let mgr = Arc::new(mgr);
    // PORT-NOTE (engine → manager): no pump — see the module docs.
    let eng_run = Arc::clone(&eng);
    let stop_engine = Arc::clone(&stop);
    std::thread::spawn(move || eng_run.run(stop_engine));
    let mgr_run = Arc::clone(&mgr);
    let stop_mgr = Arc::clone(&stop);
    std::thread::spawn(move || mgr_run.run(stop_mgr));
    log.info_attrs(
        "agent-notify tray started",
        &[
            ("version", version()),
            ("backends", eng.backends().join(",")),
            ("sinks", mgr.names().join(",")),
        ],
    );

    let tray = built.tray.expect("with_tray builds a tray sink");
    // Self-update lifecycle: silent check at startup + daily; toast and a
    // menu item when a release lands; one-click apply + restart.
    wire_updates(&stop, &tray, &mgr, log);

    // First-run onboarding: a "Set up…" menu item while the deployment
    // looks unfinished, plus a single toast pointing at it.
    if setup::detect().needs_setup() {
        let mgr_hook = Arc::clone(&mgr);
        let log_hook = log.clone();
        tray.set_setup_hook(Box::new(move || {
            if let Err(err) = launch_setup_wizard() {
                log_hook.warn_attrs(
                    "could not open a terminal for setup",
                    &[("error", err.to_string())],
                );
                mgr_hook.deliver(&setup_help_event());
            }
        }));
        if setup::nudge_due() {
            let mgr_nudge = Arc::clone(&mgr);
            let log_nudge = log.clone();
            let stop_nudge = Arc::clone(&stop);
            std::thread::spawn(move || {
                if !wait_or_stop(&stop_nudge, NUDGE_DELAY) {
                    return;
                }
                mgr_nudge.deliver(&setup_nudge_event());
                if let Err(err) = setup::mark_nudged() {
                    log_nudge.warn_attrs(
                        "cannot record the setup nudge",
                        &[("error", err.to_string())],
                    );
                }
            });
        }
    }

    // The flyout window must be created on this (locked, main) thread
    // before the message loop starts pumping.
    tray.init_flyout();
    // Agent clicks focus the agent in an open client (flyout.focusAgent).
    // Go's flyout builds that herdr call itself (newFlyout(t.eng, t.log));
    // the Rust panel keeps the invocation with the CLI, so it is wired
    // here. The argv/raise steps drive wsl.exe, herdr.exe and Win32 (the
    // window raise runs natively on Windows, over PowerShell interop
    // elsewhere — the wt clients live on the Windows side).
    if let Some(ctl) = tray.flyout_control() {
        wire_focus_agent(&eng, &ctl, log);
    }
    let mgr_test = Arc::clone(&mgr);
    tray.run(stop, Box::new(move || mgr_test.deliver(&test_event())));
    0
}

/// The focus invocation timeout (`flyout.focusAgent`'s 5 s context).
const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Installs the flyout agent-focus callback (`flyout.focusAgent`,
/// flyout_windows.go:396): run `herdr agent focus <pane>` against the
/// clicked row's session — `herdr.exe` for windows-hosted sessions, else
/// `wsl.exe` targeting the session's discovered socket, mirroring how the
/// wsl backend itself invokes herdr (no login shell, explicit PATH,
/// HERDR_SOCKET_PATH, `herdr.WSLHerdrArgs`) — then raise the already-open
/// herdr client window showing that session
/// (`flyout.raiseHerdrWindow`). Best effort: a failed run logs at debug
/// and the raise still happens.
fn wire_focus_agent(eng: &Arc<Engine>, ctl: &crate::flyout::FlyControl, log: &Logger) {
    use crate::herdr;
    use crate::proc;

    let eng = Arc::clone(eng);
    let log = log.clone();
    ctl.set_focus_agent(move |hit| {
        let (host, sess, pane) = (hit.host.as_str(), hit.sess.as_str(), hit.pane.as_str());
        let argv: Vec<String> = if host == "windows" {
            let mut argv = vec!["herdr.exe".to_owned()];
            if sess != "default" {
                argv.extend(["--session".to_owned(), sess.to_owned()]);
            }
            argv.extend(["agent".to_owned(), "focus".to_owned(), pane.to_owned()]);
            argv
        } else {
            let (reference, home) = eng.session_socket(host, sess);
            let mut argv = vec!["wsl.exe".to_owned()];
            let distro = eng.wsl_distro().to_owned();
            if !distro.is_empty() {
                argv.extend(["-d".to_owned(), distro]);
            }
            argv.extend(herdr::wsl_herdr_args(
                &reference,
                &home,
                &eng.wsl_extra_path(),
                &["agent", "focus", pane],
            ));
            argv
        };
        if let Err(err) = proc::run(&argv, &[], "", FOCUS_TIMEOUT) {
            log.debug(&format!("focusAgent {sess}/{pane}: {err}"));
        }
        raise_herdr_window(&log, sess);
    });
}

/// Brings an already-open herdr client window to the front
/// (`flyout.raiseHerdrWindow`, flyout_windows.go:427), upgraded for
/// multi-client setups: the agent lives in one session (say `pods`) while
/// several terminals run herdr clients (a WSL-projects window, a pods
/// window…), so the first "herdr"-titled window is often the wrong one.
/// The raise now prefers a visible window whose title mentions the
/// clicked session, falling back to the first herdr-titled one. A client
/// whose window title carries neither string stays unfindable — same
/// best-effort caveat as the Go.
#[cfg(target_os = "windows")]
fn raise_herdr_window(log: &Logger, sess: &str) {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    use windows::core::BOOL;

    let prefer = sess.to_lowercase();

    // Enumeration stops at the session match (Go's `found` + `return 0`,
    // which EnumWindows reports as an error; ignored like the Go does).
    // A bare-herdr hit is remembered, not taken: a later window naming the
    // session is the better target.
    struct EnumState {
        prefer: String,
        any: Option<HWND>,
        best: Option<HWND>,
    }
    unsafe extern "system" fn raise_enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: EnumWindows is synchronous; the state pointer outlives
        // the call (it lives in the caller's frame, as in the Go).
        let st = unsafe { &mut *(lparam.0 as *mut EnumState) };
        if st.best.is_some() {
            return BOOL(0);
        }
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let mut buf = [0u16; 128];
            let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
            let title = String::from_utf16_lossy(&buf[..n]).to_lowercase();
            if !title.contains("herdr") {
                return BOOL(1);
            }
            if !st.prefer.is_empty() && title.contains(&st.prefer) {
                st.best = Some(hwnd);
                return BOOL(0);
            }
            if st.any.is_none() {
                st.any = Some(hwnd);
            }
        }
        BOOL(1)
    }

    let mut state = EnumState {
        prefer,
        any: None,
        best: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(raise_enum_cb),
            LPARAM(&mut state as *mut EnumState as isize),
        );
    }
    let target = state.best.or(state.any);
    match target {
        Some(hwnd) => unsafe {
            // SW_RESTORE (9) then foreground; both return values are
            // ignored, as in the Go.
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        },
        None => log.debug("raiseHerdrWindow: no window titled herdr"),
    }
}

/// The window raise on every other host (the Rust tray runs under WSLg,
/// but the herdr clients are Windows Terminal windows on the Windows
/// side): the same visible-title scan with session preference, done by a
/// short PowerShell script over WSL interop (`-EncodedCommand` keeps the
/// payload free of quoting rules). Best effort: any failure logs at debug.
#[cfg(not(target_os = "windows"))]
fn raise_herdr_window(log: &Logger, sess: &str) {
    use crate::proc;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    // The window scan, as a C# payload for Add-Type (C# 5: no string
    // interpolation, classic delegate). The visible-title enumeration
    // mirrors the Windows-native path: prefer a herdr window whose title
    // names the session, else the first herdr window; SW_RESTORE (9) +
    // SetForegroundWindow.
    const RAISE_CS: &str = r#"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class HerdrRaise {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder sb, int n);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    public static string Find(string needle, string prefer) {
        IntPtr best = IntPtr.Zero, any = IntPtr.Zero;
        EnumWindows(delegate(IntPtr h, IntPtr l) {
            if (!IsWindowVisible(h)) return true;
            StringBuilder sb = new StringBuilder(256);
            GetWindowText(h, sb, 256);
            string t = sb.ToString().ToLower();
            if (t.IndexOf(needle, StringComparison.Ordinal) < 0) return true;
            if (any == IntPtr.Zero) any = h;
            if (prefer != "" && t.IndexOf(prefer, StringComparison.Ordinal) >= 0) { best = h; return false; }
            return true;
        }, IntPtr.Zero);
        IntPtr target = best != IntPtr.Zero ? best : any;
        if (target == IntPtr.Zero) return "";
        ShowWindow(target, 9);
        SetForegroundWindow(target);
        return "raised";
    }
}
"#;

    // Session names come from herdr's socket dirs; keep the match string
    // alphanumeric so it cannot break the embedded C# string literal.
    let prefer: String = sess
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // -EncodedCommand takes base64(UTF-16LE); the here-string terminator
    // must sit at column 0, which RAISE_CS's trailing newline guarantees.
    let script = format!(
        "$ErrorActionPreference = 'SilentlyContinue'\nAdd-Type -TypeDefinition @'{RAISE_CS}'@\nif (-not [HerdrRaise]::Find('herdr', '{prefer}')) {{ exit 2 }}\n"
    )
    // Windows PowerShell 5.1 only honors the here-string terminator with
    // CRLF line endings; LF-only payloads fail to parse (pwsh takes both).
    .replace('\n', "\r\n");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = STANDARD.encode(utf16);
    let argv: Vec<String> = [
        "powershell.exe",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        encoded.as_str(),
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Err(err) = proc::run(&argv, &[], "", FOCUS_TIMEOUT) {
        log.debug(&format!("raiseHerdrWindow: {err}"));
    }
}

/// The checker/status pair the update hooks share (`wireUpdates`'s mu +
/// locals). The Checker sits in an Arc so the fast path clones it out and
/// runs the (slow) network check without holding the lock, like Go's
/// `c := checker; mu.Unlock()`.
#[derive(Default)]
struct UpdateWiring {
    checker: Mutex<Option<Arc<Checker>>>,
    status: Mutex<Option<Status>>,
}

/// Wires the tray's update menu to the GitHub release check
/// (`cli.wireUpdates`, cli.go:369).
fn wire_updates(stop: &Arc<AtomicBool>, tray: &Arc<Tray>, mgr: &Arc<Manager>, log: &Logger) {
    let wiring = Arc::new(UpdateWiring::default());

    // checkOnce(quiet): one shared closure, cloned into every caller.
    let check_once = {
        let wiring = Arc::clone(&wiring);
        let tray = Arc::clone(tray);
        let mgr = Arc::clone(mgr);
        let log = log.clone();
        move |quiet: bool| {
            let c = {
                let mut g = wiring.checker.lock().unwrap();
                if g.is_none() {
                    match Checker::new() {
                        Ok(c) => *g = Some(Arc::new(c)),
                        Err(err) => {
                            log.warn_attrs("updater unavailable", &[("error", err.to_string())]);
                            return;
                        }
                    }
                }
                Arc::clone(g.as_ref().expect("checker set above"))
            };
            let st = match c.check(CHECK_TIMEOUT) {
                Ok(st) => st,
                Err(err) => {
                    log.warn_attrs("update check failed", &[("error", err.to_string())]);
                    return;
                }
            };
            *wiring.status.lock().unwrap() = Some(st.clone());
            log.info_attrs("update check", &[("result", st.describe())]);
            if st.update_avail {
                tray.set_update_known(true);
                if !quiet {
                    mgr.deliver(&update_event(&st.latest));
                }
            } else if !quiet {
                mgr.deliver(&status_toast_event(&st));
            }
        }
    };

    tray.set_update_hooks(
        {
            let check_once = check_once.clone();
            Box::new(move || {
                let c = check_once.clone();
                std::thread::spawn(move || c(false));
            })
        },
        {
            let wiring = Arc::clone(&wiring);
            let tray = Arc::clone(tray);
            let log = log.clone();
            Box::new(move || {
                let wiring = Arc::clone(&wiring);
                let tray = Arc::clone(&tray);
                let log = log.clone();
                std::thread::spawn(move || {
                    let c = wiring.checker.lock().unwrap().clone();
                    let st = wiring.status.lock().unwrap().clone();
                    let (Some(c), Some(st)) = (c, st) else { return };
                    let Some(rel) = &st.release else { return };
                    if let Err(err) = c.apply(rel) {
                        log.error_attrs("self-update failed", &[("error", err.to_string())]);
                        return;
                    }
                    log.info_attrs(
                        "self-update applied; restarting",
                        &[("version", st.latest.clone())],
                    );
                    if let Err(err) = update::restart_self() {
                        log.error_attrs(
                            "restart failed; start agent-notify again",
                            &[("error", err.to_string())],
                        );
                    }
                    tray.quit_app();
                });
            })
        },
    );

    // Background lifecycle: silent check 30s after startup, then daily.
    let stop_bg = Arc::clone(stop);
    std::thread::spawn(move || {
        if !wait_or_stop(&stop_bg, Duration::from_secs(30)) {
            return;
        }
        check_once(true);
        loop {
            if !wait_or_stop(&stop_bg, DAY) {
                return;
            }
            check_once(true);
        }
    });
}

/// The update_available event the background check synthesizes
/// (`cli.updateEvent`, cli.go:452).
pub fn update_event(latest: &str) -> Event {
    Event {
        kind: Kind::UpdateAvail,
        time: Time::now(),
        source: String::new(),
        host: String::new(),
        session: "agent-notify".into(),
        agent: String::new(),
        from: String::new(),
        to: String::new(),
        title: format!("v{latest} available: tray menu → Update & restart"),
        project: String::new(),
        pane_id: String::new(),
        focused: false,
    }
}

/// The setup nudge event (`cli.setupNudgeEvent`, cli.go:461).
pub fn setup_nudge_event() -> Event {
    Event {
        kind: Kind::Setup,
        time: Time::now(),
        source: String::new(),
        host: String::new(),
        session: "agent-notify".into(),
        agent: String::new(),
        from: String::new(),
        to: String::new(),
        title: "agent-notify isn't set up yet: tray menu → Set up agent-notify…".into(),
        project: String::new(),
        pane_id: String::new(),
        focused: false,
    }
}

/// The help toast when the setup wizard could not be opened
/// (`cli.setupHelpEvent`, cli.go:470).
pub fn setup_help_event() -> Event {
    Event {
        kind: Kind::Setup,
        time: Time::now(),
        source: String::new(),
        host: String::new(),
        session: "agent-notify".into(),
        agent: String::new(),
        from: String::new(),
        to: String::new(),
        title: "run `agent-notify setup` in a terminal to install".into(),
        project: String::new(),
        pane_id: String::new(),
        focused: false,
    }
}

/// The status toast after a quiet update check (`cli.statusToastEvent`,
/// cli.go:479).
pub fn status_toast_event(st: &Status) -> Event {
    Event {
        kind: Kind::Test,
        time: Time::now(),
        source: String::new(),
        host: String::new(),
        session: "agent-notify".into(),
        agent: "update check".into(),
        from: String::new(),
        to: String::new(),
        title: st.describe(),
        project: String::new(),
        pane_id: String::new(),
        focused: false,
    }
}

// ---------------------------------------------------------------------------
// Run command (cli.cmdRun, cli.go:489 + daemonUpdateLoop / restartDaemon)

/// The headless daemon (`cli.cmdRun`, cli.go:489): no UI of any kind — the
/// engine and sink manager run on their own threads and rune is never
/// touched. With the self-update + restart loop
/// (`cli.daemonUpdateLoop`, cli.go:524).
fn cmd_run(config_path: &str, log: &Logger) -> i32 {
    let cfg = load_config(config_path, log);
    let eng = match build_engine(&cfg, None) {
        Ok(e) => e,
        Err(err) => {
            log.error_attrs("cannot start", &[("error", err.to_string())]);
            return 1;
        }
    };
    let built = match sink::build_sinks(&cfg, &eng, false) {
        Ok(b) => b,
        Err(err) => {
            log.error_attrs("invalid sink configuration", &[("error", err.to_string())]);
            return 1;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    install_signals(Arc::clone(&stop));
    // A resident daemon keeps itself current: check shortly after start
    // and daily, apply, then hand over to the new binary.
    {
        let stop_loop = Arc::clone(&stop);
        let log_loop = log.clone();
        std::thread::spawn(move || daemon_update_loop(stop_loop, log_loop));
    }

    let mut mgr = Manager::new();
    for s in &built.sinks {
        mgr.add(Arc::clone(s));
    }
    let mgr = Arc::new(mgr);
    // PORT-NOTE (engine → manager): no pump — see the module docs.
    let eng_run = Arc::clone(&eng);
    let stop_engine = Arc::clone(&stop);
    std::thread::spawn(move || eng_run.run(stop_engine));
    let mgr_run = Arc::clone(&mgr);
    let stop_mgr = Arc::clone(&stop);
    std::thread::spawn(move || mgr_run.run(stop_mgr));
    log.info_attrs(
        "agent-notify running",
        &[
            ("version", version()),
            ("backends", eng.backends().join(",")),
            ("sinks", mgr.names().join(",")),
        ],
    );
    // Go: <-ctx.Done().
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    0
}

/// `run` mode's counterpart of the tray's update wiring: no human is
/// watching, so updates apply on their own. When one does, the loop signals
/// the daemon to exit (stop) after initiating the handover
/// (`cli.daemonUpdateLoop`, cli.go:524).
fn daemon_update_loop(stop: Arc<AtomicBool>, log: Logger) {
    let check = |log: &Logger| -> bool {
        let c = match Checker::new() {
            Ok(c) => c,
            Err(err) => {
                log.warn_attrs("updater unavailable", &[("error", err.to_string())]);
                return false;
            }
        };
        let st = match c.check(CHECK_TIMEOUT) {
            Ok(st) => st,
            Err(err) => {
                log.warn_attrs("update check failed", &[("error", err.to_string())]);
                return false;
            }
        };
        if !st.update_avail {
            return false;
        }
        let Some(rel) = &st.release else { return false };
        if let Err(err) = c.apply(rel) {
            log.error_attrs("self-update failed", &[("error", err.to_string())]);
            return false;
        }
        log.info_attrs(
            "self-update applied; restarting",
            &[("version", st.latest.clone())],
        );
        if let Err(err) = restart_daemon() {
            log.error_attrs(
                "restart failed; the next launch runs the new version",
                &[("error", err.to_string())],
            );
        }
        true
    };
    if !wait_or_stop(&stop, Duration::from_secs(30)) {
        return;
    }
    if check(&log) {
        stop.store(true, Ordering::SeqCst);
        return;
    }
    loop {
        if !wait_or_stop(&stop, DAY) {
            return;
        }
        if check(&log) {
            stop.store(true, Ordering::SeqCst);
            return;
        }
    }
}

/// Hands the process over to the (already swapped) binary
/// (`cli.restartDaemon`, cli.go:576). Under systemd the restart goes
/// through systemctl so the unit tracks the process; INVOCATION_ID is
/// systemd's marker for "running as a unit".
fn restart_daemon() -> anyhow::Result<()> {
    if std::env::var("INVOCATION_ID").is_ok_and(|v| !v.is_empty()) {
        // .output() discards systemctl's own output, like Go's cmd.Run().
        let st = std::process::Command::new("systemctl")
            .args(["--user", "restart", setup::SERVICE_NAME])
            .output()?;
        if !st.status.success() {
            anyhow::bail!("exit status {}", st.status.code().unwrap_or(-1));
        }
        return Ok(());
    }
    update::restart_self()
}

// ---------------------------------------------------------------------------
// Monitor / probe / test

/// Prints events as they happen (`cli.cmdMonitor`, cli.go:586).
fn cmd_monitor(config_path: &str, args: &[String], _stdout: &mut dyn Write, log: &Logger) -> i32 {
    let mut err = std::io::stderr();
    let f = match parse_flags("monitor", args, &["json", "all"], &[], &mut err) {
        Ok(f) => f,
        Err(()) => return 2,
    };
    let (json, all) = (f.boolean("json"), f.boolean("all"));
    let cfg = load_config(config_path, log);
    let kinds_override = all.then(|| vec!["all".to_string()]);
    let eng = match build_engine(&cfg, kinds_override.as_deref()) {
        Ok(e) => e,
        Err(err) => {
            log.error_attrs("cannot start", &[("error", err.to_string())]);
            return 1;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    install_signals(Arc::clone(&stop));

    let mut mgr = Manager::new();
    // PORT-NOTE: Go hands the command's stdout writer to sink.NewLog; the
    // Rust Log sink must own its writer, so it gets the process stdout —
    // the same stream `run` passes in.
    mgr.add(Arc::new(sink::Log::new(Box::new(std::io::stdout()), json)));
    let mgr = Arc::new(mgr);
    // PORT-NOTE (engine → manager): no pump — see the module docs.
    let eng_run = Arc::clone(&eng);
    let stop_engine = Arc::clone(&stop);
    std::thread::spawn(move || eng_run.run(stop_engine));
    let mgr_run = Arc::clone(&mgr);
    let stop_mgr = Arc::clone(&stop);
    std::thread::spawn(move || mgr_run.run(stop_mgr));
    log.info_attrs(
        "monitoring",
        &[("backends", eng.backends().join(","))],
    );
    // Go: <-ctx.Done().
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    0
}

/// One pass: show sessions, agents and sink availability
/// (`cli.cmdProbe`, cli.go:615; sink availability via
/// `cli.printSinkAvailability`, cli.go:655). PORT-NOTE: Go bounds
/// ProbeOnce with a signal-cancelled ctx; the Rust probe_once() has no
/// stop handle, so ^C keeps its default behavior and kills the process.
fn cmd_probe(config_path: &str, stdout: &mut dyn Write, log: &Logger) -> i32 {
    let cfg = load_config(config_path, log);
    let eng = match build_engine(&cfg, Some(&["all".to_string()])) {
        Ok(e) => e,
        Err(err) => {
            let _ = writeln!(stdout, "no backends available: {err}");
            return 1;
        }
    };

    let _ = writeln!(stdout, "backends: {}", eng.backends().join(", "));
    let errs = eng.probe_once();
    let v = eng.view();
    if v.sessions.is_empty() {
        let _ = writeln!(stdout, "sessions: none seen (is a herdr server running?)");
    }
    for s in &v.sessions {
        let state = if s.up { "up" } else { "DOWN" };
        let _ = writeln!(stdout, "\n{}/{}  [{}]", s.host, s.name, state);
        if s.agents.is_empty() {
            let _ = writeln!(stdout, "  (no agents)");
        }
        for a in &s.agents {
            let mut title = a.title.clone();
            if title.len() > 48 {
                // Go slices bytes (title[:48]); Rust strings are valid
                // UTF-8, so the cut lands on the last char boundary ≤ 48.
                let mut cut = 48;
                while !title.is_char_boundary(cut) {
                    cut -= 1;
                }
                title.truncate(cut);
                title.push('\u{2026}'); // …
            }
            let _ = writeln!(
                stdout,
                "  {:<8} {:<10} {:<8} {}",
                a.status, a.name, a.pane_id, title
            );
        }
    }
    for e in &errs {
        let _ = writeln!(stdout, "\nerror: {e}");
    }
    print_sink_availability(&cfg, stdout);
    0
}

/// The probe's sink availability footer (`cli.printSinkAvailability`,
/// cli.go:655).
fn print_sink_availability(cfg: &config::Config, stdout: &mut dyn Write) {
    let _ = writeln!(stdout, "\nsinks:");
    // Go ignores the render.New error here; fall back to the default
    // templates so the popup probe still runs.
    let r = Arc::new(
        render::Renderer::new(&cfg.render.title, &cfg.render.body)
            .or_else(|_| render::Renderer::new("", ""))
            .expect("default renderer"),
    );
    match sink::Popup::new(&config::Sink::default(), Arc::clone(&r)) {
        Err(err) => {
            let _ = writeln!(stdout, "  popup: unavailable ({err})");
        }
        Ok(_) => {
            let _ = writeln!(stdout, "  popup: available");
        }
    }
    let _ = writeln!(
        stdout,
        "  tray: {}",
        if gui_available() {
            "available"
        } else {
            "no display"
        }
    );
    let _ = writeln!(stdout, "  bell, command, webhook: available (config-driven)");
}

/// Sends a test notification through every configured sink
/// (`cli.cmdTest`, cli.go:667; the event via `cli.testEvent`, cli.go:708).
pub fn cmd_test(config_path: &str, stdout: &mut dyn Write, log: &Logger) -> i32 {
    let cfg = load_config(config_path, log);
    let eng = match build_engine(&cfg, None) {
        Ok(e) => e,
        Err(err) => {
            log.error_attrs("cannot start", &[("error", err.to_string())]);
            return 1;
        }
    };
    if let Err(err) = render::Renderer::new(&cfg.render.title, &cfg.render.body) {
        let _ = writeln!(stdout, "render templates: {err}");
        return 1;
    }
    // withTray: the tray sink's Deliver forwards to its popup, which works
    // headless, so testing it shows exactly what the tray would fire.
    let built = match sink::build_sinks(&cfg, &eng, true) {
        Ok(b) => b,
        Err(err) => {
            let _ = writeln!(stdout, "invalid sink configuration: {err}");
            return 1;
        }
    };
    if built.sinks.is_empty() {
        let _ = writeln!(stdout, "no sinks configured; nothing to test");
        return 0;
    }
    let ev = test_event();
    let mut failed = false;
    for s in &built.sinks {
        // Go: context.WithTimeout(ctx, 20*time.Second) around each Deliver.
        let deadline = Duration::from_secs(20);
        match s.deliver(&ev, deadline) {
            Err(err) => {
                failed = true;
                let _ = writeln!(stdout, "{:<8} FAIL: {err}", s.name());
            }
            Ok(()) => {
                let _ = writeln!(stdout, "{:<8} ok", s.name());
            }
        }
    }
    if failed {
        return 1;
    }
    0
}

/// The event `agent-notify test` sends (`cli.testEvent`, cli.go:708).
pub fn test_event() -> Event {
    Event {
        kind: Kind::Test,
        time: Time::now(),
        source: "herdr".into(),
        host: "local".into(),
        session: "default".into(),
        agent: "demo-agent".into(),
        from: String::new(),
        to: String::new(),
        title: "agent-notify test notification".into(),
        project: String::new(),
        pane_id: String::new(),
        focused: false,
    }
}

// ---------------------------------------------------------------------------
// init / flytest / version / update / config

/// Writes the annotated example config (`cli.cmdInit`, cli.go:720).
pub fn cmd_init(config_path: &str, args: &[String], stdout: &mut dyn Write) -> i32 {
    let mut err = std::io::stderr();
    let f = match parse_flags("init", args, &["force"], &[], &mut err) {
        Ok(f) => f,
        Err(()) => return 2,
    };
    let force = f.boolean("force");
    let Some(path) = config::path(config_path) else {
        let _ = writeln!(stdout, "cannot determine a config path; pass --config");
        return 1;
    };
    if path.exists() && !force {
        let _ = writeln!(
            stdout,
            "{} already exists (use --force to overwrite)",
            path.display()
        );
        return 1;
    }
    if let Err(e) = write_example_config(&path) {
        let _ = writeln!(stdout, "write config: {e}");
        return 1;
    }
    let _ = writeln!(stdout, "wrote {}", path.display());
    0
}

/// Diagnostics: drives the flyout show/hide cycle (`cli.cmdFlytest`,
/// cli.go:743). Simplified port — see the module header's flytest note.
fn cmd_flytest(config_path: &str, stdout: &mut dyn Write, log: &Logger) -> i32 {
    let cfg = load_config(config_path, log);
    let eng = match build_engine(&cfg, None) {
        Ok(e) => e,
        Err(err) => {
            let _ = writeln!(stdout, "engine: {err}");
            return 1;
        }
    };
    // Go: tray.NewFlyoutForTest(eng, log).SelfTest() — the Windows version
    // pumps messages around show/hide and dumps window introspection. The
    // Rust panel runs its rune event loop on this (main) thread and the
    // driver thread drives one open/close cycle through FlyControl.
    let fly = Flyout::new(Arc::clone(&eng));
    let ctl = fly.control();
    let app = fly.into_app();
    let ctl_driver = ctl.clone();
    let driver = std::thread::spawn(move || {
        let ctl = ctl_driver;
        std::thread::sleep(Duration::from_millis(300));
        println!("flytest: showing");
        ctl.show();
        std::thread::sleep(Duration::from_millis(1200));
        println!("flytest: after show  visible={}", ctl.is_visible());
        ctl.hide();
        std::thread::sleep(Duration::from_millis(300));
        println!("flytest: after hide  visible={}", ctl.is_visible());
        ctl.quit();
    });
    let windows: Vec<AnyWindowSpec> = vec![WindowSpec::from_app("flyout", app)
        .options(crate::flyout::window_options())
        .into()];
    let ctl_area = Some(ctl);
    let result = rune::try_run_windows_with_args(RunOptions::default(), [], move |displays: &[Display]| {
        // Detect the work area once, before the first frame (same factory
        // shape as the tray's run loop).
        let area = crate::flyout::work_area(displays);
        if let Some(c) = &ctl_area {
            c.apply_work_area(area);
        }
        windows
    });
    let _ = driver.join();
    match result {
        Ok(()) => 0,
        Err(err) => {
            let _ = writeln!(stdout, "flytest: event loop failed: {err}");
            1
        }
    }
}

/// Prints version information (`cli.cmdVersion`, cli.go:759). PORT-NOTE:
/// the third parenthesized field is the compiler (Go: runtime.Version(),
/// e.g. go1.26.3); build.rs captures `rustc --version` for it.
pub fn cmd_version(stdout: &mut dyn Write) -> i32 {
    let _ = writeln!(
        stdout,
        "agent-notify {} ({}/{}, {})",
        version(),
        goos(),
        goarch(),
        rustc_version()
    );
    0
}

/// Checks GitHub releases and optionally self-updates
/// (`cli.cmdUpdate`, cli.go:765). PORT-NOTE: Go wraps the check in a
/// signal-cancelled ctx; the port uses the fixed 20s timeout and lets ^C
/// keep its default (kill) instead of cancelling mid-check.
fn cmd_update(args: &[String], stdout: &mut dyn Write) -> i32 {
    let mut err = std::io::stderr();
    let f = match parse_flags("update", args, &["check"], &[], &mut err) {
        Ok(f) => f,
        Err(()) => return 2,
    };
    let check_only = f.boolean("check");
    let checker = match Checker::new() {
        Ok(c) => c,
        Err(err) => {
            let _ = writeln!(stdout, "updater: {err}");
            return 1;
        }
    };
    let st = match checker.check(CHECK_TIMEOUT) {
        Ok(st) => st,
        Err(err) => {
            let _ = writeln!(stdout, "update check failed: {err}");
            return 1;
        }
    };
    let _ = writeln!(stdout, "{}", st.describe());
    if !st.update_avail || check_only {
        return 0;
    }
    let Some(rel) = &st.release else {
        return 0;
    };
    if let Err(err) = checker.apply(rel) {
        let _ = writeln!(stdout, "update failed: {err}");
        return 1;
    }
    let _ = writeln!(stdout, "updated to {}; restart agent-notify to apply", st.latest);
    0
}

/// path | edit | validate the configuration (`cli.cmdConfig`, cli.go:796).
fn cmd_config(config_path: &str, args: &[String], stdout: &mut dyn Write) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("path");
    match sub {
        "path" => {
            let (p, source) = config::resolve(config_path);
            let Some(p) = p else {
                let _ = writeln!(stdout, "no config path resolvable");
                return 1;
            };
            let _ = writeln!(stdout, "{} (source: {})", p.display(), source);
        }
        "edit" => {
            let Some(p) = config::path(config_path) else {
                let _ = writeln!(stdout, "cannot determine a config path");
                return 1;
            };
            if !p.exists() {
                let _ = writeln!(
                    stdout,
                    "{} does not exist; run `agent-notify init` first",
                    p.display()
                );
                return 1;
            }
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "notepad.exe".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            // exec.Command(editor, p) with stdio inherited; Go's cmd.Run()
            // also fails on a non-zero exit, which the port reproduces.
            let st = std::process::Command::new(&editor)
                .arg(&p)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();
            match st {
                Err(err) => {
                    let _ = writeln!(stdout, "editor: {err}");
                    return 1;
                }
                Ok(st) if !st.success() => {
                    let _ = writeln!(stdout, "editor: exit status {}", st.code().unwrap_or(-1));
                    return 1;
                }
                Ok(_) => {}
            }
        }
        "validate" => {
            let (p, _) = config::resolve(config_path);
            let Some(p) = p.filter(|p| p.exists()) else {
                let _ = writeln!(stdout, "config: none found (defaults in effect); ok");
                return 0;
            };
            match config::Config::load(&p) {
                Err(err) => {
                    let _ = writeln!(stdout, "INVALID: {err}");
                    return 1;
                }
                Ok((_, warnings)) => {
                    for w in warnings {
                        let _ = writeln!(stdout, "warning: {w}");
                    }
                }
            }
            let _ = writeln!(stdout, "{}: ok", p.display());
        }
        _ => {
            let _ = writeln!(stdout, "unknown config subcommand {sub:?} (path|edit|validate)");
            return 2;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Setup / uninstall wizard (setup_cmd.go)

/// Flags `cmd_setup` parses (`setup`'s FlagSet).
struct SetupOpts {
    yes: bool,
    no_autostart: bool,
    service_mode: bool,
    with_wsl: bool,
    wsl_distro: String,
}

/// The first-run wizard (`cli.cmdSetup`, setup_cmd.go:25). It walks a
/// freshly downloaded binary through deploying itself; `--status` only
/// prints, `--yes` takes the defaults.
fn cmd_setup(
    config_path: &str,
    args: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    log: &Logger,
) -> i32 {
    let f = match parse_flags(
        "setup",
        args,
        &["status", "yes", "no-autostart", "service", "with-wsl"],
        &["wsl-distro"],
        stderr, // Go: fs.SetOutput(stderr)
    ) {
        Ok(f) => f,
        Err(()) => return 2,
    };
    let o = SetupOpts {
        yes: f.boolean("yes"),
        no_autostart: f.boolean("no-autostart"),
        service_mode: f.boolean("service"),
        with_wsl: f.boolean("with-wsl"),
        wsl_distro: f.string("wsl-distro"),
    };
    if o.no_autostart && o.service_mode {
        let _ = writeln!(stderr, "setup: --no-autostart and --service conflict");
        return 2;
    }

    let st = setup::detect();
    print_status_lines(stdout, &st);
    if f.boolean("status") {
        return 0;
    }

    if !is_terminal() && !o.yes {
        let _ = writeln!(
            stdout,
            "setup: no terminal attached; re-run with --yes to apply the defaults"
        );
        pause_if_windowed(stdout);
        return 0;
    }
    let rc = cmd_setup_flow(config_path, stdout, log, &o, st);
    // Go: defer pauseIfWindowed(stdout) — keeps the window spawned by the
    // tray's "Set up…" item open until Enter.
    pause_if_windowed(stdout);
    rc
}

/// The wizard body (setup_cmd.go cmdSetup's steps 1–7). Everything after
/// the wizard is built goes through its writer — in Go both are the same
/// stdout.
fn cmd_setup_flow(
    config_path: &str,
    stdout: &mut dyn Write,
    log: &Logger,
    o: &SetupOpts,
    mut st: setup::Status,
) -> i32 {
    let mut w = Wizard::new(Box::new(std::io::stdin()), stdout, o.yes);

    // 1: install the binary into its canonical home
    let mut stopped = false;
    let mut installed_now = false;
    if st.install_dir.is_empty() {
        w.say("setup: no canonical install location here; continuing in place");
    } else if st.installed {
        w.say(&format!("setup: already installed ({})", st.install_exe));
    } else if w.confirm(&format!("Install agent-notify to {}?", st.install_dir), true) {
        match setup::install() {
            Err(err) => {
                w.say(&format!("setup: install failed: {err}"));
                return 1;
            }
            Ok(did_stop) => {
                stopped = did_stop;
                w.say(&format!("setup: installed {}", st.install_exe));
                installed_now = true;
                // From here the wizard speaks for the installed copy, not
                // the (possibly Downloads-folder) process running it.
                st = setup::detect();
                st.exe = st.install_exe.clone();
                st.installed = true;
            }
        }
    } else {
        w.say("setup: keeping the binary where it is (portable mode)");
    }

    // 2: PATH
    let mut path_fixed = false;
    if st.installed && !st.path_ok && w.confirm(&format!("Add {} to your PATH?", st.install_dir), true)
    {
        match setup::ensure_path() {
            Err(err) => {
                w.say(&format!("setup: PATH update failed: {err}"));
            }
            Ok(()) => {
                path_fixed = true;
                if cfg!(windows) {
                    w.say("setup: added to the user PATH (new terminals pick it up)");
                } else {
                    w.say("setup: PATH export added to ~/.profile (new shells pick it up)");
                }
            }
        }
    }

    // 3: residency (start at login)
    if o.no_autostart {
        w.say("setup: autostart skipped (--no-autostart)");
    } else if cfg!(windows) {
        if st.autostart {
            w.say("setup: autostart already configured (Startup shortcut)");
        } else if w.confirm("Start agent-notify automatically at login?", true) {
            match setup::ensure_tray_autostart() {
                Err(err) => {
                    w.say(&format!("setup: autostart failed: {err}"));
                }
                Ok(()) => {
                    w.say("setup: Startup shortcut created (see shell:startup)");
                }
            }
        }
    } else {
        match residency_mode(&mut w, &st, o.service_mode, gui_available()) {
            "tray" => match setup::ensure_tray_autostart() {
                Err(err) => {
                    w.say(&format!("setup: autostart failed: {err}"));
                }
                Ok(()) => {
                    w.say("setup: tray will start at login (XDG autostart)");
                }
            },
            "service" => {
                if !st.systemd {
                    w.say("setup: no systemd user manager here; service skipped");
                    if !st.service_hint.is_empty() {
                        w.say(&format!("setup: {}", st.service_hint));
                    }
                } else if let Err(err) = setup::ensure_service() {
                    w.say(&format!("setup: service install failed: {err}"));
                } else {
                    w.say(&format!(
                        "setup: systemd service enabled (logs: journalctl --user -u {})",
                        setup::SERVICE_NAME
                    ));
                    if w.confirm(
                        "Enable lingering so the service runs without a login session?",
                        false,
                    ) {
                        match setup::enable_linger() {
                            Err(err) => {
                                w.say(&format!("setup: linger failed: {err}"));
                            }
                            Ok(()) => {
                                w.say("setup: lingering enabled");
                            }
                        }
                    }
                }
            }
            "skip" => {
                w.say("setup: no residency configured on this machine");
            }
            "done" => {
                w.say("setup: residency already configured");
            }
            _ => unreachable!(),
        }
    }

    // 4: WSL daemon (Windows hosts)
    if cfg!(windows) {
        if o.with_wsl {
            run_wsl_step(&mut w, o);
        } else if setup::wsl_available()
            && w.confirm(
                "Also install the headless daemon inside WSL? (needs systemd in the distro)",
                false,
            )
        {
            run_wsl_step(&mut w, o);
        }
    }

    // 5: bring the tray back up if the install replaced it
    if stopped && w.confirm("Start agent-notify now?", true) {
        match setup::start_installed() {
            Err(err) => {
                w.say(&format!("setup: could not start: {err}"));
            }
            Ok(()) => {
                w.say("setup: started");
            }
        }
    }

    // 6: config and a test popup
    match config::path(config_path) {
        None => {
            w.say("setup: cannot determine a config path; pass --config");
        }
        Some(p) if p.exists() => {
            w.say(&format!("setup: config exists ({})", p.display()));
        }
        Some(p) => {
            if w.confirm("Write an annotated config now? (zero config works too)", true) {
                match write_example_config(&p) {
                    Err(err) => {
                        w.say(&format!("setup: config write failed: {err}"));
                    }
                    Ok(()) => {
                        w.say(&format!("setup: wrote {}", p.display()));
                    }
                }
            }
        }
    }
    if w.confirm("Send a test notification through the sinks?", true) {
        cmd_test(config_path, w.writer(), log);
    }

    // 7: summary
    w.say("");
    let mut st = setup::detect();
    if installed_now {
        st.exe = st.install_exe.clone();
        st.installed = true;
    }
    if path_fixed {
        st.path_ok = true; // the live process PATH can't show it yet
    }
    print_status_lines(w.writer(), &st);
    w.say("setup: done; updates arrive automatically (daily check, `agent-notify update` to force)");
    w.say("setup: everything is reversible with `agent-notify uninstall`");
    0
}

/// The WSL daemon step, shared by --with-wsl and the interactive prompt
/// (cmdSetup's runWSL closure).
fn run_wsl_step(w: &mut Wizard, o: &SetupOpts) {
    w.say("setup: installing the WSL daemon (this takes a moment)…");
    if let Err(err) = setup::install_wsl(&o.wsl_distro, w.writer()) {
        w.say(&format!("setup: WSL daemon install failed: {err}"));
        w.say("setup: the Windows tray watches WSL sessions on its own either way");
    }
}

/// Picks how a posix install stays resident: tray autostart on desktop
/// sessions, the systemd service on headless boxes, and an explicit choice
/// when both would work (`cli.residencyMode`, setup_cmd.go:203). PORT-NOTE:
/// `gui` is a parameter (Go reads guiAvailable() inline) so the decision
/// tree is testable without a display.
fn residency_mode(w: &mut Wizard, st: &setup::Status, service_flag: bool, gui: bool) -> &'static str {
    if service_flag {
        return "service";
    }
    if st.autostart || st.service_enabled {
        return "done";
    }
    let sysd = st.systemd;
    if !gui && !sysd {
        return "skip";
    }
    if !gui {
        return "service";
    }
    if !sysd {
        return "tray";
    }
    if !w.yes
        && w.choose(
            "Keep agent-notify resident how?",
            &["tray at login (XDG autostart)", "headless systemd service"],
            0,
        ) == 1
    {
        return "service";
    }
    "tray"
}

/// Undoes setup (`cli.cmdUninstall`, setup_cmd.go:229).
fn cmd_uninstall(
    config_path: &str,
    args: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let f = match parse_flags("uninstall", args, &["yes", "purge-config"], &[], stderr) {
        Ok(f) => f,
        Err(()) => return 2,
    };
    let rc = cmd_uninstall_flow(config_path, stdout, f.boolean("yes"), f.boolean("purge-config"));
    pause_if_windowed(stdout);
    rc
}

fn cmd_uninstall_flow(config_path: &str, stdout: &mut dyn Write, yes: bool, purge: bool) -> i32 {
    let st = setup::detect();
    let mut items: Vec<String> = Vec::new();
    if st.service_enabled {
        items.push("the systemd user service".into());
    }
    if st.autostart {
        items.push("login autostart".into());
    }
    if Path::new(&st.install_exe).exists() {
        items.push(format!("the binaries in {}", st.install_dir));
    }
    if purge && st.config_exists {
        items.push(format!("the config {}", st.config_path));
    }
    if items.is_empty() {
        let _ = writeln!(stdout, "uninstall: nothing installed to remove");
        return 0;
    }
    let _ = writeln!(stdout, "uninstall: this will remove:");
    for it in &items {
        let _ = writeln!(stdout, "  - {it}");
    }
    if !yes {
        if !is_terminal() {
            let _ = writeln!(stdout, "uninstall: no terminal attached; re-run with --yes");
            return 0;
        }
        let proceed = {
            let mut w = Wizard::new(Box::new(std::io::stdin()), stdout, false);
            w.confirm("Proceed?", true)
        };
        if !proceed {
            let _ = writeln!(stdout, "uninstall: aborted");
            return 0;
        }
    }
    let (notes, result) = setup::uninstall(purge, config_path);
    for n in &notes {
        let _ = writeln!(stdout, "uninstall: {n}");
    }
    if let Err(err) = result {
        let _ = writeln!(stdout, "uninstall: error: {err}");
        return 1;
    }
    if setup::wsl_available() {
        let _ = writeln!(
            stdout,
            "uninstall: a WSL-side daemon (if any) is untouched; inside the distro run:"
        );
        let _ = writeln!(
            stdout,
            "  systemctl --user disable --now {} && rm -f ~/.local/bin/{}",
            setup::SERVICE_NAME,
            setup::SERVICE_NAME
        );
    }
    let _ = writeln!(stdout, "uninstall: done");
    0
}

/// Prompts on a terminal. With yes set it answers every question with the
/// default, so the same flow drives the install scripts
/// (`setup_cmd.go`'s wizard). Owns the wizard's stdout reborrow so every
/// step writes through one writer.
struct Wizard<'a> {
    inp: std::io::BufReader<Box<dyn Read>>,
    out: &'a mut dyn Write,
    yes: bool,
}

impl<'a> Wizard<'a> {
    fn new(inp: Box<dyn Read>, out: &'a mut dyn Write, yes: bool) -> Wizard<'a> {
        Wizard {
            inp: std::io::BufReader::new(inp),
            out,
            yes,
        }
    }

    /// The writer, for the plain `fmt.Fprintf(stdout, …)` call sites.
    fn writer(&mut self) -> &mut dyn Write {
        self.out
    }

    /// Writes one line (fmt.Fprintln(stdout, …)).
    fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    /// confirm(q, def): y/n with a default; EOF (piped stdin, including a
    /// final line without a newline, like Go's ReadString + io.EOF) falls
    /// back to the default.
    fn confirm(&mut self, q: &str, def: bool) -> bool {
        if self.yes {
            return def;
        }
        loop {
            let hint = if def { "[Y/n]" } else { "[y/N]" };
            let _ = write!(self.out, "{q} {hint} ");
            let _ = self.out.flush();
            match self.read_line() {
                None => {
                    let _ = writeln!(self.out);
                    return def;
                }
                Some(line) => match line.trim().to_lowercase().as_str() {
                    "" => return def,
                    "y" | "yes" => return true,
                    "n" | "no" => return false,
                    _ => {
                        let _ = writeln!(self.out, "please answer y or n");
                    }
                },
            }
        }
    }

    /// choose(q, options, def): a numbered pick; EOF falls back to the
    /// default like confirm does.
    fn choose(&mut self, q: &str, options: &[&str], def: usize) -> usize {
        if self.yes {
            return def;
        }
        for (i, o) in options.iter().enumerate() {
            let _ = writeln!(self.out, "  {}) {}", i + 1, o);
        }
        loop {
            let _ = write!(self.out, "{} [1-{}, default {}] ", q, options.len(), def + 1);
            let _ = self.out.flush();
            match self.read_line() {
                None => {
                    let _ = writeln!(self.out);
                    return def;
                }
                Some(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        return def;
                    }
                    if let Ok(n) = line.parse::<usize>() {
                        if n >= 1 && n <= options.len() {
                            return n - 1;
                        }
                    }
                    let _ = writeln!(self.out, "enter a number 1-{}", options.len());
                }
            }
        }
    }

    /// One input line; None on EOF before a newline. Go's
    /// `ReadString('\n')` returns (partial, io.EOF) for a final unterminated
    /// line, which the wizard treats as EOF — the port matches.
    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.inp.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) if !line.ends_with('\n') => None,
            Ok(_) => Some(line),
            Err(_) => None,
        }
    }
}

/// Renders the status as aligned rows (`cli.printStatusLines`,
/// setup_cmd.go:347).
fn print_status_lines(w: &mut dyn Write, st: &setup::Status) {
    let rows = st.lines();
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    for r in &rows {
        let _ = writeln!(w, "{:<width$}  {}", r.0, r.1, width = width);
    }
}

/// Keeps the console open when the wizard was spawned in its own window by
/// the tray's "Set up…" item, so its output isn't lost the instant it
/// finishes (`cli.pauseIfWindowed`, setup_cmd.go:363).
fn pause_if_windowed(stdout: &mut dyn Write) {
    if std::env::var("AGENT_NOTIFY_SETUP_WINDOW").map_or(true, |v| v.is_empty()) || !is_terminal() {
        return;
    }
    let _ = write!(stdout, "\nPress Enter to close this window\u{2026}");
    let _ = stdout.flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Creates the annotated config, shared by `init` and the wizard's config
/// step (`cli.writeExampleConfig`, setup_cmd.go:373).
fn write_example_config(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, config::EXAMPLE)?;
    // Go's os.WriteFile(..., 0o644); fs::write uses the process umask, so
    // the mode is set explicitly to match.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-platform helpers (the per-GOOS files of internal/cli)

/// attachParentConsole (console_other.go): only meaningful for windowsgui
/// builds.
#[cfg(not(windows))]
pub fn attach_parent_console() {}

/// attachParentConsole (console_windows.go): lets a windowsgui-subsystem
/// binary print to the terminal it was launched from (probe/test/... from
/// cmd or PowerShell). A windowless launch has no parent console and the
/// call fails silently; under WSL interop the std pipes are inherited
/// directly and the call is a no-op.
///
/// PORT-NOTE (deviation): console_windows.go then rebinds
/// os.Stdout/os.Stderr/os.Stdin via GetStdHandle + os.NewFile — Rust cannot
/// swap the process-wide std handles after they are initialized. Instead
/// `main` calls this BEFORE cli::run evaluates the std writers: std
/// resolves its console handles on first use, so the attach above is what
/// the first GetStdHandle sees.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return; // windowless launch (Explorer double-click): no window
        }
    }
}

/// Keeps the single-instance lock alive for the process duration
/// (acquireTrayLock's release func). On unix dropping the guard closes the
/// lock file and releases the flock; on Windows the handle is kept and
/// never closed (the Go release func only retains it).
struct TrayLock {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(windows)]
    _handle: windows::Win32::Foundation::HANDLE,
}

/// acquireTrayLock (singleinstance_other.go): prevents multiple tray
/// instances via a non-blocking flock on a per-user lock file.
#[cfg(unix)]
fn acquire_tray_lock() -> anyhow::Result<TrayLock> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path =
        std::env::temp_dir().join(format!("agent-notify-tray-{}.lock", unsafe { libc::getuid() }));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(anyhow::anyhow!("another agent-notify tray is already running"));
    }
    Ok(TrayLock { file: f })
}

/// acquireTrayLock (singleinstance_windows.go): a second exe launch would
/// otherwise double every toast. The named mutex lives for the process
/// duration.
#[cfg(windows)]
fn acquire_tray_lock() -> anyhow::Result<TrayLock> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name = HSTRING::from("Local\\agent-notify-tray");
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|err| anyhow::anyhow!("create mutex: {err}"))?;
    // An existing mutex is reported as a "success" whose last error is
    // ERROR_ALREADY_EXISTS — that is the second-instance signal.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(anyhow::anyhow!("another agent-notify tray is already running"));
    }
    Ok(TrayLock { _handle: handle })
}

/// isTerminal (tty_*.go): whether stdin is an interactive terminal. The Go
/// ships three per-OS probes (TCGETS ioctl on Linux, GetConsoleMode on
/// Windows, a ModeCharDevice check elsewhere); the port uses
/// std::io::IsTerminal, which performs the same per-OS checks — including
/// the /dev/null distinction the Go comments warn about.
pub fn is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

/// launchSetupWizard (setuplaunch_other.go): opens a terminal running
/// `setup`. Best effort — there is no single API for spawning terminals on
/// Linux desktops; when nothing is found the caller falls back to a toast
/// pointing at the command.
#[cfg(not(windows))]
fn launch_setup_wizard() -> anyhow::Result<()> {
    /// Terminal emulators and how each runs a command (termLaunchers).
    const TERM_LAUNCHERS: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xfce4-terminal", &["-x"]), // -e is deprecated in newer releases
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("xterm", &["-e"]),
    ];
    let self_exe = std::env::current_exe()?;
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        if !t.is_empty() {
            candidates.push((t, Vec::new()));
        }
    }
    for (bin, pre) in TERM_LAUNCHERS {
        candidates.push(((*bin).to_string(), pre.iter().map(|s| (*s).to_string()).collect()));
    }
    for (bin, pre) in candidates {
        let Some(path) = crate::proc::look_path(&bin) else {
            continue;
        };
        let mut cmd = std::process::Command::new(path);
        cmd.args(&pre).arg(&self_exe).arg("setup");
        if cmd.spawn().is_ok() {
            return Ok(());
        }
    }
    Err(anyhow::anyhow!(
        "no terminal emulator found (set $TERMINAL); run `{}` setup",
        self_exe.display()
    ))
}

/// launchSetupWizard (setuplaunch_windows.go): opens the setup wizard in a
/// fresh console window via the console twin — the tray exe is windowsgui
/// and has no window to host an interactive prompt. cmd's `start` returns
/// immediately; the marker env makes the wizard hold its window open when
/// it finishes.
#[cfg(windows)]
fn launch_setup_wizard() -> anyhow::Result<()> {
    let self_exe = std::env::current_exe()?;
    let mut target = match self_exe.parent() {
        Some(dir) => dir.join("agent-notify-console.exe"),
        None => self_exe.clone(),
    };
    if !target.exists() {
        target = self_exe; // go install builds ship no twin: attach to the caller's console
    }
    // The empty "" is start's window-title argument; without it a quoted
    // target path would be taken as the title.
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", ""])
        .arg(&target)
        .arg("setup")
        .env("AGENT_NOTIFY_SETUP_WINDOW", "1");
    // Go sets SysProcAttr.HideWindow = false — Rust's default: cmd's own
    // console window must appear.
    cmd.spawn()?;
    Ok(())
}

/// guiAvailable (cli.go:859): whether a tray icon can be shown.
pub fn gui_available() -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        true
    } else {
        std::env::var("DISPLAY").is_ok_and(|v| !v.is_empty())
            || std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Signals + stop-flag waiting (Go: signal.NotifyContext + select on ctx)

/// The flag the signal handlers store to (async-signal-safe: a single
/// atomic store); a relay thread copies it into the command's stop flag.
static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    SIGNAL_STOP.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

#[cfg(windows)]
unsafe extern "system" fn on_ctrl_event(_t: u32) -> windows::core::BOOL {
    SIGNAL_STOP.store(true, Ordering::SeqCst);
    true.into()
}

#[cfg(windows)]
fn install_signal_handlers() {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe {
        // Covers CTRL_C_EVENT and friends (Go's os.Interrupt on Windows).
        let _ = SetConsoleCtrlHandler(Some(on_ctrl_event), true);
    }
}

/// Replaces Go's `signal.NotifyContext(ctx, os.Interrupt, syscall.SIGTERM)`:
/// installs the handlers and relays SIGNAL_STOP into `stop` (the loops poll
/// it; signal handlers may only touch async-signal-safe state, so the Arc
/// hand-off happens on a thread).
fn install_signals(stop: Arc<AtomicBool>) {
    SIGNAL_STOP.store(false, Ordering::SeqCst);
    install_signal_handlers();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if SIGNAL_STOP.load(Ordering::SeqCst) {
                stop.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

/// Waits `d` for the stop flag; false once stop fired first — Go's
/// `select { case <-time.After(d): case <-ctx.Done(): }`.
fn wait_or_stop(stop: &AtomicBool, d: Duration) -> bool {
    let deadline = std::time::Instant::now() + d;
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return !stop.load(Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(50).min(deadline - now));
    }
}

// ---------------------------------------------------------------------------
// Flag parsing (the slice of the Go `flag` package each subcommand uses)

/// The parsed result of one subcommand's flags (`flag.FlagSet`).
struct Flags {
    vals: HashMap<String, String>,
    args: Vec<String>,
}

impl Flags {
    /// A bool flag's value (Go's boolValue default false).
    fn boolean(&self, name: &str) -> bool {
        self.vals.get(name).map(|v| v == "true").unwrap_or(false)
    }

    /// A string flag's value (Go's stringValue default "").
    fn string(&self, name: &str) -> String {
        self.vals.get(name).cloned().unwrap_or_default()
    }
}

/// Parses subcommand flags like Go's flag package: `-x`/`--x`, `--x=v`;
/// bool flags take no separate value, string flags consume the next
/// argument; parsing stops at the first positional argument or `--`.
/// Unknown flags (and `-h`, which the Go flag package rejects as ErrHelp)
/// print to `err` and yield Err → exit code 2. PORT-NOTE: Go's FlagSet
/// also dumps a full "Usage of x:" block on error; the port prints the
/// error line only.
fn parse_flags(
    cmd: &str,
    args: &[String],
    bools: &[&str],
    strings: &[&str],
    err: &mut dyn Write,
) -> Result<Flags, ()> {
    let mut vals: HashMap<String, String> = HashMap::new();
    let mut i = 0usize;
    while i < args.len() {
        let s = &args[i];
        let b = s.as_bytes();
        if b.len() < 2 || b[0] != b'-' {
            break; // positional argument: Go stops parsing here
        }
        let mut num_minuses = 1usize;
        if b[1] == b'-' {
            num_minuses = 2;
            if b.len() == 2 {
                i += 1; // "--" terminates the flags
                break;
            }
        }
        let name_full = &s[num_minuses..];
        if name_full.is_empty() || name_full.starts_with('-') || name_full.starts_with('=') {
            let _ = writeln!(err, "bad flag syntax: {s}");
            return Err(());
        }
        let (name, value) = match name_full.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (name_full.to_string(), None),
        };
        let is_bool = bools.iter().any(|c| *c == name);
        let is_str = strings.iter().any(|c| *c == name);
        if !is_bool && !is_str {
            if name == "help" || name == "h" {
                let _ = writeln!(err, "flag: help requested (usage of {cmd})");
                return Err(());
            }
            let _ = writeln!(err, "flag provided but not defined: -{name}");
            return Err(());
        }
        i += 1;
        if is_bool {
            match value {
                Some(v) => match go_parse_bool(&v) {
                    Some(bv) => {
                        vals.insert(name, bv.to_string());
                    }
                    None => {
                        let _ = writeln!(err, "invalid boolean value {v:?} for -{name}");
                        return Err(());
                    }
                },
                None => {
                    vals.insert(name, "true".to_string());
                }
            }
        } else {
            let v = match value {
                Some(v) => v,
                None if i < args.len() => {
                    let v = args[i].clone();
                    i += 1;
                    v
                }
                None => {
                    let _ = writeln!(err, "flag needs an argument: -{name}");
                    return Err(());
                }
            };
            vals.insert(name, v);
        }
    }
    Ok(Flags {
        vals,
        args: args[i.min(args.len())..].to_vec(),
    })
}

/// strconv.ParseBool's accepted spellings.
fn go_parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests (internal/cli ships no *_test.go; the builders and helpers here are
// pinned against their Go literals and decision trees)

#[cfg(test)]
mod tests {
    use super::*;

    /// testEvent (cli.go:708).
    #[test]
    fn test_event_matches_the_go_literal() {
        let ev = test_event();
        assert_eq!(ev.kind, Kind::Test);
        assert_eq!(ev.source, "herdr");
        assert_eq!(ev.host, "local");
        assert_eq!(ev.session, "default");
        assert_eq!(ev.agent, "demo-agent");
        assert_eq!(ev.title, "agent-notify test notification");
        assert!(ev.project.is_empty());
        assert!(ev.pane_id.is_empty());
    }

    /// updateEvent / setupNudgeEvent / setupHelpEvent strings (cli.go:452-477).
    #[test]
    fn synthetic_events_carry_the_go_strings() {
        let ev = update_event("0.6.0");
        assert_eq!(ev.kind, Kind::UpdateAvail);
        assert_eq!(ev.session, "agent-notify");
        assert_eq!(ev.title, "v0.6.0 available: tray menu → Update & restart");

        let ev = setup_nudge_event();
        assert_eq!(ev.kind, Kind::Setup);
        assert_eq!(ev.session, "agent-notify");
        assert_eq!(ev.title, "agent-notify isn't set up yet: tray menu → Set up agent-notify…");

        let ev = setup_help_event();
        assert_eq!(ev.kind, Kind::Setup);
        assert_eq!(ev.session, "agent-notify");
        assert_eq!(ev.title, "run `agent-notify setup` in a terminal to install");
    }

    /// statusToastEvent (cli.go:479): KindTest + "update check" agent +
    /// Status.Describe as the title.
    #[test]
    fn status_toast_event_carries_describe() {
        let st = Status {
            current: "0.5.0".into(),
            latest: "0.6.0".into(),
            update_avail: true,
            release: None,
        };
        let ev = status_toast_event(&st);
        assert_eq!(ev.kind, Kind::Test);
        assert_eq!(ev.session, "agent-notify");
        assert_eq!(ev.agent, "update check");
        assert_eq!(ev.title, st.describe());
    }

    /// fleetExtra (cli.go:200): project + host/session joined with " · ",
    /// then " — " + the live view summary; empty parts → summary alone.
    #[test]
    fn fleet_extra_composes_project_host_and_summary() {
        let eng = Arc::new(Engine::with_backends(engine::Config::default(), vec![]));
        let extra = fleet_extra(&eng);
        let mut ev = test_event();

        assert_eq!(extra(&ev), format!("local/default — {}", eng.view().summary()));

        ev.project = "api".into();
        assert_eq!(extra(&ev), format!("api · local/default — {}", eng.view().summary()));

        ev.host.clear();
        ev.session.clear();
        assert_eq!(extra(&ev), format!("api — {}", eng.view().summary()));
    }

    /// kindSet (cli.go:188).
    #[test]
    fn kind_set_collects_the_parsed_kinds() {
        let ks = event::parse_set(&["attention", "agent_blocked"]).expect("parse");
        let set = kind_set(&ks);
        assert!(set.contains(&Kind::AgentIdle) && set.contains(&Kind::AgentDone));
        assert!(set.contains(&Kind::AgentBlocked));
        assert_eq!(set.len(), 3); // duplicates deduped, order-independent
    }

    /// cli.Run's dispatch: help/usage, unknown command, --version.
    #[test]
    fn run_prints_usage_for_help() {
        let mut out = Vec::new();
        let code = run(
            vec!["agent-notify".into(), "help".into()],
            &mut out,
            &mut Vec::new(),
        );
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).unwrap(), USAGE);
    }

    #[test]
    fn run_rejects_unknown_commands_with_usage() {
        let mut err = Vec::new();
        let code = run(
            vec!["agent-notify".into(), "bogus".into()],
            &mut Vec::new(),
            &mut err,
        );
        assert_eq!(code, 2);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            format!("unknown command \"bogus\"\n\n{USAGE}")
        );
    }

    #[test]
    fn run_version_flag_prints_the_version_line() {
        let mut out = Vec::new();
        let code = run(
            vec!["agent-notify".into(), "--version".into()],
            &mut out,
            &mut Vec::new(),
        );
        assert_eq!(code, 0);
        let mut expected = Vec::new();
        cmd_version(&mut expected);
        assert_eq!(out, expected);
    }

    /// cmdVersion (cli.go:759): `agent-notify VERSION (os/arch, compiler)`
    /// with Go's GOOS/GOARCH spellings (darwin/amd64, not macos/x86_64).
    #[test]
    fn cmd_version_format_matches_go() {
        let mut out = Vec::new();
        assert_eq!(cmd_version(&mut out), 0);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.starts_with(&format!("agent-notify {} (", buildinfo::VERSION)),
            "{s}"
        );
        assert!(
            s.contains(&format!("({}/{}, ", goos(), goarch())),
            "{s}"
        );
        assert!(s.ends_with(")\n"), "{s}");
    }

    /// The Go flag-package slice the subcommands rely on.
    #[test]
    fn parse_flags_matches_go_flag_semantics() {
        let mut err = Vec::new();
        let a = ["--json".to_string(), "--all=true".to_string(), "extra".to_string()];
        let f = parse_flags("monitor", &a, &["json", "all"], &[], &mut err).unwrap();
        assert!(f.boolean("json") && f.boolean("all"));
        assert_eq!(f.args, ["extra"]);

        let f = parse_flags(
            "monitor",
            &["--json=false".to_string()],
            &["json", "all"],
            &[],
            &mut err,
        )
        .unwrap();
        assert!(!f.boolean("json"));

        // string flags take the next argument; the = form works too
        let a = ["-wsl-distro".to_string(), "Ubuntu-22.04".to_string(), "x".to_string()];
        let f = parse_flags("setup", &a, &[], &["wsl-distro"], &mut err).unwrap();
        assert_eq!(f.string("wsl-distro"), "Ubuntu-22.04");
        assert_eq!(f.args, ["x"]);

        let a = ["--wsl-distro=Debian".to_string()];
        let f = parse_flags("setup", &a, &[], &["wsl-distro"], &mut err).unwrap();
        assert_eq!(f.string("wsl-distro"), "Debian");

        // unknown flag → Err (exit 2 at the call site)
        assert!(
            parse_flags("monitor", &["--nope".to_string()], &["json"], &[], &mut err).is_err()
        );
        // missing string argument → Err
        assert!(
            parse_flags("setup", &["-wsl-distro".to_string()], &[], &["wsl-distro"], &mut err)
                .is_err()
        );
        // invalid bool value → Err (Go: "invalid boolean value")
        assert!(parse_flags(
            "monitor",
            &["--json=maybe".to_string()],
            &["json"],
            &[],
            &mut err
        )
        .is_err());
        // parsing stops at the first positional (Go flag behavior)
        let a = ["x".to_string(), "--json".to_string()];
        let f = parse_flags("monitor", &a, &["json"], &[], &mut err).unwrap();
        assert!(!f.boolean("json"));
        assert_eq!(f.args, ["x", "--json"]);
        // "--" terminates the flags
        let a = ["--".to_string(), "--json".to_string()];
        let f = parse_flags("monitor", &a, &["json"], &[], &mut err).unwrap();
        assert!(!f.boolean("json"));
        assert_eq!(f.args, ["--json"]);
    }

    /// go_parse_bool covers strconv.ParseBool's accepted spellings.
    #[test]
    fn go_parse_bool_covers_strconv() {
        for v in ["1", "t", "T", "true", "TRUE", "True"] {
            assert_eq!(go_parse_bool(v), Some(true), "{v}");
        }
        for v in ["0", "f", "F", "false", "FALSE", "False"] {
            assert_eq!(go_parse_bool(v), Some(false), "{v}");
        }
        assert_eq!(go_parse_bool("yes"), None);
        assert_eq!(go_parse_bool(""), None);
    }

    /// printStatusLines (setup_cmd.go:347): labels padded to the longest,
    /// two separating spaces. With no install dir, Status::lines() emits
    /// version/binary/install only ("unsupported on this platform").
    #[test]
    fn status_lines_align_the_label_column() {
        let mut st = setup::Status::default();
        st.version = "dev".into();
        st.exe = "agent-notify".into();
        st.install_dir = String::new();
        let mut out = Vec::new();
        print_status_lines(&mut out, &st);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "version  dev\nbinary   agent-notify\ninstall  unsupported on this platform\n"
        );
    }

    /// residencyMode (setup_cmd.go:203)'s decision tree; `gui` is injected
    /// (see the fn's PORT-NOTE) so the branches are deterministic.
    #[test]
    fn residency_mode_follows_the_go_decision_tree() {
        let mut st = setup::Status::default();
        let mut out = Vec::new();
        let mut w = Wizard::new(Box::new(std::io::empty()), &mut out, true);

        // --service wins outright
        assert_eq!(residency_mode(&mut w, &st, true, true), "service");
        // already configured → done
        st.autostart = true;
        assert_eq!(residency_mode(&mut w, &st, false, true), "done");
        st.autostart = false;
        st.service_enabled = true;
        assert_eq!(residency_mode(&mut w, &st, false, true), "done");
        // neither gui nor systemd → skip
        st.service_enabled = false;
        assert_eq!(residency_mode(&mut w, &st, false, false), "skip");
        // no gui, systemd present → service
        st.systemd = true;
        assert_eq!(residency_mode(&mut w, &st, false, false), "service");
        // gui, no systemd → tray
        st.systemd = false;
        assert_eq!(residency_mode(&mut w, &st, false, true), "tray");

        // both available, interactive: the EOF stdin answers the prompt
        // with the default (0 → tray), like a piped wizard would
        st.systemd = true;
        let mut out2 = Vec::new();
        let mut w2 = Wizard::new(Box::new(std::io::empty()), &mut out2, false);
        assert_eq!(residency_mode(&mut w2, &st, false, true), "tray");
    }

    /// The wizard's confirm/choose fall back to the default on EOF,
    /// including a final line without a newline (Go's ReadString + io.EOF).
    #[test]
    fn wizard_falls_back_to_defaults_on_eof() {
        let mut out = Vec::new();
        let mut w = Wizard::new(Box::new(std::io::empty()), &mut out, false);
        assert!(w.confirm("Proceed?", true));
        assert!(!w.confirm("Proceed?", false));
        assert_eq!(w.choose("Pick", &["a", "b"], 1), 1);

        // --yes short-circuits everything: answers with the default
        let mut out2 = Vec::new();
        let mut w2 = Wizard::new(Box::new(std::io::empty()), &mut out2, true);
        assert!(!w2.confirm("Anything?", false));
        assert!(w2.confirm("Anything?", true));
        assert_eq!(w2.choose("Pick", &["a", "b"], 0), 0);
        assert!(out2.is_empty(), "yes mode prints no prompts");
    }

    /// writeExampleConfig (setup_cmd.go:373): the annotated example at
    /// 0o644, parents created.
    #[test]
    fn write_example_config_writes_the_example() {
        let dir = std::env::temp_dir().join(format!("an-cli-test-{}", std::process::id()));
        let path = dir.join("agent-notify").join("config.toml");
        write_example_config(&path).expect("write");
        let data = std::fs::read_to_string(&path).unwrap();
        assert_eq!(data, config::EXAMPLE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// gui_available runs without panicking; the Windows/macOS rule is
    /// compile-time, the X11/Wayland rule is env-driven.
    #[test]
    fn gui_available_follows_the_go_rule() {
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(gui_available());
        } else if std::env::var("DISPLAY").map_or(true, |v| v.is_empty())
            && std::env::var("WAYLAND_DISPLAY").map_or(true, |v| v.is_empty())
        {
            assert!(!gui_available());
        }
    }

    /// slog quoting: space/'='/quote/control chars force quoting; plain
    /// words and paths don't (needsQuoting in log/slog).
    #[test]
    fn quote_slog_matches_the_text_handler() {
        assert_eq!(quote_slog("monitoring"), "monitoring");
        assert_eq!(quote_slog("/mnt/c/Users/jalte/config.toml"), "/mnt/c/Users/jalte/config.toml");
        assert_eq!(
            quote_slog("using Windows-side config (WSL auto-share)"),
            "\"using Windows-side config (WSL auto-share)\""
        );
        assert_eq!(quote_slog(""), "\"\"");
        assert_eq!(quote_slog("a=b"), "\"a=b\"");
    }

    /// wait_or_stop honors the deadline and an already-set stop flag.
    #[test]
    fn wait_or_stop_respects_stop_and_deadline() {
        let stop = Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        assert!(wait_or_stop(&stop, Duration::from_millis(60)));
        assert!(started.elapsed() >= Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
        assert!(!wait_or_stop(&stop, Duration::from_secs(60)));
    }
}
