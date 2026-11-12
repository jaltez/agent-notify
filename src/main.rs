//! agent-notify: tray notifier for AI coding agents (herdr today).
//! Port of `main.go` plus the entry contract it shares with `cli.rs`.
//!
//! - `cli::run` receives the full argv (Go: os.Args) and returns the
//!   process exit code, which `main` exits with (os.Exit).
//! - Port of `runtime.LockOSThread` (main.go): the tray and flyout windows
//!   live on the main OS thread's message queue. Rust has no goroutine
//!   scheduler, so the initial thread never migrates — the pin is
//!   inherent. In `tray` mode the rune event loop takes over this thread
//!   (`cli::cmd_tray` → `Tray::run`) and the engine/sinks run on their own
//!   threads; in `run` (headless daemon) mode no rune UI is created at all.

#![allow(dead_code)]
// Release builds (`--features windowsgui`): no console flash when the tray
// exe launches from Explorer; CLI subcommands reattach via
// `attach_parent_console` (cli.rs). The installer's console twin is the
// same source built without this feature.
#![cfg_attr(all(windows, feature = "windowsgui"), windows_subsystem = "windows")]

mod buildinfo;
mod cli;
mod config;
mod engine;
mod event;
mod flyout;
mod herdr;
mod icon;
mod proc;
mod render;
mod setup;
mod sink;
mod tray;
mod update;

fn main() {
    // console_windows.go: windowsgui builds reattach to the parent console
    // for CLI subcommands. This must run before the std writers are first
    // used — std resolves its console handles lazily on first
    // stdout/stderr use, so attaching here (before cli::run evaluates its
    // arguments) lets probe/test/setup output reach the terminal the
    // binary was launched from. See cli.rs's attach_parent_console note.
    cli::attach_parent_console();
    let code = cli::run(
        std::env::args().collect(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
