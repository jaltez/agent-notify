//! The command-line surface. Port of `internal/cli/cli.go` (+ the console,
//! single-instance and setup-launch helpers in that package) — scaffold:
//! signatures only, bodies `todo!()`.

use std::io::Write;

use crate::config;
use crate::engine::Engine;
use crate::event::{Event, Kind};
use crate::update::Status;
use std::sync::Arc;

pub const USAGE: &str = "agent-notify: tray notifier for AI coding agents (herdr today)

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

/// Executes the CLI and returns the process exit code (`cli.Run`,
/// cli.go:62). On Windows the tray/flyout windows live on the main OS
/// thread's message queue — pin to one thread before any window is created
/// (Go: runtime.LockOSThread in main).
pub fn run(argv: Vec<String>, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let _ = (argv, stdout, stderr);
    todo!("port of cli.Run (cli.go:62): flag parsing + command dispatch")
}

/// Loads and validates the config for the resolved path, warning on
/// unknown keys (`cli.loadConfig`, cli.go:140).
pub fn load_config(path: &str) -> config::Config {
    let _ = path;
    todo!("port of cli.loadConfig (cli.go:140)")
}

/// Resolves the enabled backends into an engine (`cli.buildEngine`,
/// cli.go:170). `kinds_override` replaces the configured event set.
pub fn build_engine(
    cfg: config::Config,
    kinds_override: &[String],
) -> anyhow::Result<Arc<Engine>> {
    let _ = (cfg, kinds_override);
    todo!("port of cli.buildEngine (cli.go:170)")
}

/// The kind allow-set for the engine (`cli.kindSet`, cli.go:188).
pub fn kind_set(ks: &[Kind]) -> std::collections::HashSet<Kind> {
    ks.iter().copied().collect()
}

/// Extra fleet summary appended to popups (`cli.fleetExtra`, cli.go:200).
pub fn fleet_extra(eng: &Engine) -> Box<dyn Fn(&Event) -> String + Send + Sync> {
    let _ = eng;
    todo!("port of cli.fleetExtra (cli.go:200)")
}

/// The tray command: builds sinks, runs the tray + engine until exit
/// (`cli.cmdTray`, cli.go:289). Wires the background update check through
/// `cli.wireUpdates` (cli.go:369) and the first-run setup nudge.
pub fn cmd_tray(config_path: &str, stderr: &mut dyn Write) -> i32 {
    let _ = (config_path, stderr);
    todo!("port of cli.cmdTray (cli.go:289)")
}

/// The headless daemon (`cli.cmdRun`, cli.go:489), with the self-update +
/// restart loop (`cli.daemonUpdateLoop`, cli.go:524 /
/// `cli.restartDaemon`, cli.go:576).
pub fn cmd_run(config_path: &str) -> i32 {
    let _ = config_path;
    todo!("port of cli.cmdRun (cli.go:489)")
}

/// Prints events as they happen (`cli.cmdMonitor`, cli.go:586).
pub fn cmd_monitor(config_path: &str, args: &[String], stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, args, stdout);
    todo!("port of cli.cmdMonitor (cli.go:586)")
}

/// One pass: show sessions, agents and sink availability
/// (`cli.cmdProbe`, cli.go:615); sink availability via
/// `cli.printSinkAvailability` (cli.go:655).
pub fn cmd_probe(config_path: &str, stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, stdout);
    todo!("port of cli.cmdProbe (cli.go:615)")
}

/// Sends a test notification through every configured sink
/// (`cli.cmdTest`, cli.go:667; the event via `cli.testEvent`, cli.go:708).
pub fn cmd_test(config_path: &str, stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, stdout);
    todo!("port of cli.cmdTest (cli.go:667)")
}

/// Writes the annotated example config (`cli.cmdInit`, cli.go:720;
/// contents: [`config::EXAMPLE`]).
pub fn cmd_init(config_path: &str, args: &[String], stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, args, stdout);
    todo!("port of cli.cmdInit (cli.go:720)")
}

/// Diagnostics: drives the flyout show/hide cycle (`cli.cmdFlytest`,
/// cli.go:743).
pub fn cmd_flytest(config_path: &str, stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, stdout);
    todo!("port of cli.cmdFlytest (cli.go:743)")
}

/// Prints version information (`cli.cmdVersion`, cli.go:759).
pub fn cmd_version(stdout: &mut dyn Write) -> i32 {
    let _ = stdout;
    todo!("port of cli.cmdVersion (cli.go:759)")
}

/// Self-update from GitHub releases (`cli.cmdUpdate`, cli.go:765), over
/// [`crate::update::Checker`].
pub fn cmd_update(args: &[String], stdout: &mut dyn Write) -> i32 {
    let _ = (args, stdout);
    todo!("port of cli.cmdUpdate (cli.go:765)")
}

/// path | edit | validate the configuration (`cli.cmdConfig`, cli.go:796).
pub fn cmd_config(config_path: &str, args: &[String], stdout: &mut dyn Write) -> i32 {
    let _ = (config_path, args, stdout);
    todo!("port of cli.cmdConfig (cli.go:796)")
}

/// The update_available event the background check synthesizes
/// (`cli.updateEvent`, cli.go:452).
pub fn update_event(latest: &str) -> Event {
    let _ = latest;
    todo!("port of cli.updateEvent (cli.go:452)")
}

/// The setup nudge event (`cli.setupNudgeEvent`, cli.go:461) and its help
/// variant (`cli.setupHelpEvent`, cli.go:470).
pub fn setup_nudge_event() -> Event {
    todo!("port of cli.setupNudgeEvent (cli.go:461)")
}

/// The status toast event after an update cycle (`cli.statusToastEvent`,
/// cli.go:479).
pub fn status_toast_event(st: &Status) -> Event {
    let _ = st;
    todo!("port of cli.statusToastEvent (cli.go:479)")
}

/// Whether a GUI session is available (a tray can exist at all)
/// (`cli.guiAvailable`, cli.go:859).
pub fn gui_available() -> bool {
    todo!("port of cli.guiAvailable (cli.go:859)")
}

// Windows-only helpers (each is one Go file in internal/cli):
//
// - console_windows.go / console_other.go — attach the parent console and
//   hide the window for the windowsgui build.
// - singleinstance_windows.go / singleinstance_other.go — one tray
//   instance per user; forward new launches to the running one.
// - setuplaunch_windows.go / setuplaunch_other.go — relaunch after setup
//   with the console hidden.
// - tty_linux.go / tty_other.go / tty_windows.go — TTY detection for the
//   wizard.
