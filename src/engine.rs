//! Polls source backends, diffs consecutive snapshots into events, applies
//! filters, and exposes both an event stream and a live state view. Port of
//! `internal/engine/engine.go`, `tracker.go` and `view.go`.
//!
//! SCAFFOLD STATUS: the shared types ([`Tracker`], [`View`],
//! [`SessionView`], [`AgentView`], [`Counts`], [`Config`]) carry their
//! complete signatures — these are the contract the tray, flyout and cli
//! ports build against. The bodies are `todo!()` and documented; the
//! polling loop itself ([`Engine`]) is the next unit of work.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config;
use crate::event::{self, Event, Kind};
use crate::herdr;

// ---------------------------------------------------------------------------
// View types (view.go) — the shape the tray, flyout and probe consume.

/// One agent row in a [`View`] (`engine.AgentView`). `wants` is the
/// attention latch: true while an observed finish (working → idle/done) has
/// not been followed by new work; idle/done agents without it are resting,
/// not waiting for you.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentView {
    pub name: String,
    pub status: String,
    pub wants: bool,
    pub title: String,
    pub project: String,
    pub pane_id: String,
    pub focused: bool,
}

/// One session (last known state) in a [`View`] (`engine.SessionView`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionView {
    pub host: String,
    pub name: String,
    pub up: bool,
    pub agents: Vec<AgentView>,
}

/// A point-in-time copy of everything the engine knows (`engine.View`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub sessions: Vec<SessionView>,
}

/// Summarizes a [`View`]: working agents, agents waiting for a human (idle
/// or done), blocked agents, and offline sessions (`engine.Counts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub working: usize,
    pub waiting: usize,
    pub blocked: usize,
    pub down: usize,
}

impl View {
    /// Aggregates agent statuses across reachable sessions
    /// (`View.Counts`). Skips down sessions (they contribute to
    /// `Counts::down` only); only latched idle/done agents count as
    /// waiting.
    pub fn counts(&self) -> Counts {
        todo!("port of engine.View.Counts (view.go:48)")
    }

    /// The view state driving the tray icon color. Precedence: blocked
    /// (red) > offline (amber) > working (blue) > waiting (green) > idle
    /// (gray). Working beats waiting so a busy fleet reads as "busy";
    /// green is transition-latched via [`AgentView::wants`]
    /// (`View.Severity`, view.go:76).
    pub fn severity(&self) -> &'static str {
        todo!("port of engine.View.Severity (view.go:76)")
    }

    /// Renders counts for tooltips and menus, e.g.
    /// "2 working · 1 waiting · 1 blocked · 1 offline"; "all quiet" when
    /// everything is empty (`View.Summary`, view.go:94).
    pub fn summary(&self) -> String {
        todo!("port of engine.View.Summary (view.go:94)")
    }
}

/// The color class for one agent row (tray flyout dot and wash, menu
/// label). done and a latched idle read as waiting: the strongest
/// "finished, needs you" signal; an unlatched idle/done pane is quiet
/// gray, and unknown stays gray (`engine.RowSeverity`, view.go:193).
pub fn row_severity(status: &str, wants: bool) -> &'static str {
    let _ = (status, wants);
    todo!("port of engine.RowSeverity (view.go:193)")
}

// ---------------------------------------------------------------------------
// Tracker (tracker.go)

/// How many consecutive failed polls mark a session offline. One failed
/// poll is usually a slow wsl.exe invocation or a transient timeout, not
/// an outage; flapping the session (and the tray severity with it) on
/// every spike makes the icon lie (`engine.downAfter`).
pub const DOWN_AFTER: u32 = 3;

/// Holds the last known state of one session and turns snapshot diffs into
/// events (`engine.tracker`). Fields are private because the diffing rules
/// (sticky unknown status, the wants latch, hysteresis) are invariant the
/// tracker owns.
pub struct Tracker {
    host: String,
    name: String,
    /// session socket path and distro home, for CLI calls.
    ref_: String,
    home: String,
    /// had at least one successful snapshot.
    seen: bool,
    /// last fetch succeeded.
    up: bool,
    /// consecutive failed fetches (hysteresis before down).
    fails: u32,

    /// agents keyed by pane key ([`agent_key`]).
    agents: HashMap<String, herdr::Agent>,
    /// pane keys in last snapshot order (stable UI).
    order: Vec<String>,

    /// panes whose last finish (working → idle/done) has not been followed
    /// by new work (the attention latch). It drives the green severity:
    /// agents already idle at startup never latch, so a fresh tray boots
    /// gray instead of green.
    wants: HashSet<String>,
}

impl Tracker {
    pub fn new(host: &str, name: &str) -> Tracker {
        Tracker {
            host: host.to_string(),
            name: name.to_string(),
            ref_: String::new(),
            home: String::new(),
            seen: false,
            up: false,
            fails: 0,
            agents: HashMap::new(),
            order: Vec::new(),
            wants: HashSet::new(),
        }
    }

    /// The session socket ref the backend discovered, for one-shot CLI
    /// calls (agent focus) targeted at the right server.
    pub fn session_ref(&self) -> (&str, &str) {
        (&self.ref_, &self.home)
    }

    /// Folds a successful snapshot in and returns the transition events.
    /// The first snapshot is a baseline: it populates state without events.
    /// `changed` is true only when the view-relevant state actually moved;
    /// the tray rebuilds its menu on it and must not do that every poll
    /// (`tracker.apply`, tracker.go:44).
    pub fn apply(&mut self, snap: &herdr::Snapshot, now: event::Time) -> (Vec<Event>, bool) {
        let _ = (snap, now, &mut self.seen);
        todo!("port of tracker.apply (tracker.go:44)")
    }

    /// Records a failed fetch, emitting session_down on the up→down edge
    /// after [`DOWN_AFTER`] consecutive failures; transient spikes stay
    /// silent (`tracker.fail`, tracker.go:143).
    pub fn fail(&mut self, now: event::Time) -> (Vec<Event>, bool) {
        let _ = (now, &mut self.fails);
        todo!("port of tracker.fail (tracker.go:143)")
    }

    /// Bridges unknown flaps: herdr's screen detection can briefly report
    /// unknown for a live agent, which would drop it from every count. An
    /// unknown reading keeps the previous status; title, project and focus
    /// still refresh (`tracker.stickyStatus`, tracker.go:100).
    fn sticky_status(&self, prev: herdr::Agent, cur: herdr::Agent) -> herdr::Agent {
        let _ = (prev, cur);
        todo!("port of tracker.stickyStatus (tracker.go:100)")
    }

    /// Maintains the attention latch on status transitions: an observed
    /// finish (working → idle/done) latches, any departure from idle/done
    /// (new work, blocked) unlatches (`tracker.trackWants`,
    /// tracker.go:111).
    fn track_wants(&mut self, key: &str, prev_status: &str, cur_status: &str) {
        let _ = (key, prev_status, cur_status);
        todo!("port of tracker.trackWants (tracker.go:111)")
    }

    /// Maps a status change to an event, or None for noise (unknown
    /// flapping or idle ↔ done reshuffles) (`tracker.transition`,
    /// tracker.go:123).
    fn transition(
        &self,
        prev: &herdr::Agent,
        cur: &herdr::Agent,
        now: event::Time,
    ) -> Option<Event> {
        let _ = (prev, cur, now);
        todo!("port of tracker.transition (tracker.go:123)")
    }

    fn session_event(&self, kind: Kind, now: event::Time) -> Event {
        let _ = (kind, now);
        todo!("port of tracker.sessionEvent (tracker.go:153)")
    }

    fn agent_event(
        &self,
        kind: Kind,
        prev: &herdr::Agent,
        cur: &herdr::Agent,
        now: event::Time,
    ) -> Event {
        let _ = (kind, prev, cur, now);
        todo!("port of tracker.agentEvent (tracker.go:163)")
    }
}

/// Panes are keyed by pane id when present, else "name#index"
/// (`tracker.agentKey`, tracker.go:193).
fn agent_key(a: &herdr::Agent, i: usize) -> String {
    if !a.pane_id.is_empty() {
        return a.pane_id.clone();
    }
    format!("{}#{}", a.name, i)
}

// ---------------------------------------------------------------------------
// Engine (engine.go) — polling loop; scaffold signatures only.

/// Parameterizes an [`Engine`] (`engine.Config`).
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// event kinds allowed through to sinks (Go: map[event.Kind]bool).
    pub kinds: HashSet<Kind>,
    /// min spacing between identical (kind,session,pane) events.
    pub cooldown: std::time::Duration,
    /// session-name globs to keep; empty = all.
    pub include: Vec<String>,
    /// session-name globs to drop.
    pub exclude: Vec<String>,
    pub herdr: config::Herdr,
}

/// Watches herdr sessions and emits filtered events (`engine.Engine`). The
/// Go version fans one poller thread per backend with channels for events
/// and state changes; the Rust port keeps that shape with
/// `std::sync::mpsc` and an `Arc<AtomicBool>` stop flag in place of
/// `context.Context`.
pub struct Engine {
    cfg: Config,
    /// enabled backends with their poll/timeout budgets.
    backends: Vec<BackendRun>,
    /// last known tracker per session, keyed "host/name".
    sessions: Mutex<HashMap<String, Tracker>>,
    /// last emission per "kind|host|session|pane" cooldown key.
    cooldowns: Mutex<HashMap<String, Instant>>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    state_tx: mpsc::Sender<()>,
    state_rx: mpsc::Receiver<()>,
}

struct BackendRun {
    backend: Box<dyn herdr::Backend>,
    poll: std::time::Duration,
    timeout: std::time::Duration,
}

impl Engine {
    /// Resolves the configured backends and returns a ready engine. It
    /// errors when no backend could be enabled at all (`engine.New`,
    /// engine.go:71). The auto/on/off resolution of `herdr.local`,
    /// `herdr.windows` and `herdr.wsl` happens here.
    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let _ = cfg;
        todo!("port of engine.New (engine.go:71)")
    }

    /// Overrides backend resolution — tests (`engine.WithBackends`). The
    /// Go builder-Option becomes a constructor variant.
    pub fn with_backends(cfg: Config, backends: Vec<Box<dyn herdr::Backend>>) -> Engine {
        let _ = backends;
        Engine::from_cfg(cfg)
    }

    fn from_cfg(cfg: Config) -> Engine {
        let (events_tx, events_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        Engine {
            cfg,
            backends: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashMap::new()),
            events_tx,
            events_rx,
            state_tx,
            state_rx,
        }
    }

    /// The configured WSL distro for the wsl backend ("" = wsl.exe's
    /// default distro) (`engine.WSLDistro`).
    pub fn wsl_distro(&self) -> &str {
        todo!("port of engine.WSLDistro (engine.go:118)")
    }

    /// The socket reference and distro home for a session as its backend
    /// discovered it; used to target one-shot CLI calls (agent focus) at
    /// the right server (`engine.SessionSocket`, engine.go:123). Returns
    /// ("", "") for unknown sessions.
    pub fn session_socket(&self, host: &str, name: &str) -> (String, String) {
        let _ = (host, name);
        todo!("port of engine.SessionSocket (engine.go:123)")
    }

    /// The configured extra PATH entries for herdr calls inside the WSL
    /// distro (`engine.WSLExtraPath`).
    pub fn wsl_extra_path(&self) -> Vec<String> {
        todo!("port of engine.WSLExtraPath (engine.go:135)")
    }

    /// Lists the enabled backend names (`engine.Backends`).
    pub fn backends(&self) -> Vec<String> {
        todo!("port of engine.Backends (engine.go:138)")
    }

    /// The filtered event stream (`engine.Events`); buffered to 64, full
    /// queue drops.
    pub fn events(&self) -> &mpsc::Receiver<Event> {
        &self.events_rx
    }

    /// Signals (coalesced) that [`Engine::view`] would return something
    /// new (`engine.StateChange`).
    pub fn state_change(&self) -> &mpsc::Receiver<()> {
        &self.state_rx
    }

    /// Polls every backend until `stop` is set (`engine.Run`).
    pub fn run(&self, stop: Arc<std::sync::atomic::AtomicBool>) {
        let _ = stop;
        todo!("port of engine.Run (engine.go:153)")
    }

    /// Performs a single synchronous discovery+fetch pass per backend (for
    /// `agent-notify probe`); returns per-session fetch errors
    /// (`engine.ProbeOnce`, engine.go:238).
    pub fn probe_once(&self) -> Vec<anyhow::Error> {
        todo!("port of engine.ProbeOnce (engine.go:238)")
    }

    /// Returns a sorted, deep copy of the current state. Sessions appear
    /// once they have had at least one successful snapshot; agents keep
    /// their last known state while a session is offline (`engine.View()`,
    /// view.go:125). Sessions sort by attention priority (blocked,
    /// offline, waiting, working, quiet — `engine.spaceRank`), then host,
    /// then name.
    pub fn view(&self) -> View {
        todo!("port of engine.View (view.go:125)")
    }

    /// Resolves an enabled = auto|on|off value against its auto condition
    /// (`engine.want`, engine.go:331). Returns (use, explicit).
    pub(crate) fn want(enabled: &str, auto_on: bool) -> (bool, bool) {
        match enabled {
            "on" => (true, true),
            "off" => (false, false),
            // "", "auto"
            _ => (auto_on, false),
        }
    }
}
