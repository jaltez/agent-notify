//! agent-notify: tray notifier for AI coding agents (herdr today).
//! Rust port scaffold — see the module docs for the port contract.
//!
//! Companion Go tree lives in `internal/` until the port completes;
//! module-by-module mapping is documented in each Rust file's header.

// Scaffold stage: the stub modules (sink, render, setup, update, tray,
// flyout, cli) and the shared types are intentionally unused from `main`
// until each port lands. Tighten this once the engine + tray are wired.
#![allow(dead_code)]

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
    // The tray and flyout windows live on the main OS thread's message
    // queue; rune's own run loop takes over the main thread once
    // `rune::App::run` is called, so cli must not spawn windows off-thread.
    let code = cli::run(
        std::env::args().skip(1).collect(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
