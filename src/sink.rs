//! Delivers events to notifiers: the tray, desktop popups, the terminal
//! bell, arbitrary commands, HTTP webhooks, and log output. Port of
//! `internal/sink/` — scaffold: signatures only, bodies `todo!()`.
//!
//! - `sink.go` — the [`Sink`] trait and fan-out [`Manager`]
//! - `popup.go` — toast/notify-send popups (severity logo from [`crate::icon`])
//! - `webhook.go` — POST the event (JSON or a rendered body template)
//! - `command.go` — run an argv/shell command per event
//! - `log.go` — log-line sink

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::config;
use crate::event::Event;

/// Bounds a single Deliver call (`sink.deliverTimeout`).
pub const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-sink queue size; a full queue drops (and logs) rather than blocking
/// (`sink.queueSize`).
pub const QUEUE_SIZE: usize = 16;

/// Consumes events (`sink.Sink`). `deliver` should honor cancellation; the
/// [`Manager`] bounds every call with [`DELIVER_TIMEOUT`] anyway. Go's
/// `context.Context` timeout becomes the `deadline` argument.
pub trait Sink: Send + Sync {
    fn name(&self) -> &'static str;
    fn deliver(&self, ev: &Event, deadline: Duration) -> anyhow::Result<()>;
}

/// Builds the sink list from config, in order (`cli.buildSinks`,
/// cli.go:218). Each flavor below is the port of its Go file.
pub fn build_sinks(
    cfg: &config::Config,
    with_tray: bool,
) -> anyhow::Result<Vec<Arc<dyn Sink>>> {
    let _ = (cfg, with_tray);
    todo!("port of cli.buildSinks (cli.go:218) + popup/webhook/command/log sinks")
}

/// Fans events out to sinks, one worker thread each (`sink.Manager`).
pub struct Manager;

impl Manager {
    /// Builds an empty Manager (`sink.NewManager`).
    pub fn new() -> Manager {
        Manager
    }

    /// Registers a sink and starts its worker. Call before [`Manager::run`]
    /// (`Manager.Add`).
    pub fn add(&mut self, sink: Arc<dyn Sink>) {
        let _ = sink;
        todo!("port of sink.Manager.Add (sink.go:45)")
    }

    /// Lists registered sink names in order (`Manager.Names`).
    pub fn names(&self) -> Vec<String> {
        todo!("port of sink.Manager.Names (sink.go:50)")
    }

    /// Fans one event out to every sink (non-blocking; full queues drop and
    /// log) (`Manager.Deliver`, sink.go:59).
    pub fn deliver(&self, ev: &Event) {
        let _ = ev;
        todo!("port of sink.Manager.Deliver (sink.go:59)")
    }

    /// Drives the workers until `stop` is set (`Manager.Run`, sink.go:70).
    pub fn run(&self, stop: Arc<AtomicBool>) {
        let _ = stop;
        todo!("port of sink.Manager.Run (sink.go:70)")
    }
}
