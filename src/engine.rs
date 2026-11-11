//! Polls source backends, diffs consecutive snapshots into events, applies
//! filters, and exposes both an event stream and a live state view. Port of
//! `internal/engine/engine.go`, `tracker.go` and `view.go`.
//!
//! THREAD MODEL (Go goroutines/tickers → std::thread + mpsc, no tokio):
//!
//!   - [`Engine::run`] blocks the calling thread until `stop` is set, with
//!     one poller thread per backend spawned through `std::thread::scope`;
//!     leaving the scope joins them all (Go's `Run` + `wg.Wait()`).
//!   - The stop flag (`Arc<AtomicBool>`) replaces `context.Context`
//!     cancellation. Pollers check it between polls and in ≤50 ms slices
//!     inside the tick wait, so shutdown latency is bounded by ~50 ms plus
//!     one in-flight fetch timeout.
//!   - Each poller keeps Go's `time.NewTicker` semantics: a fixed schedule
//!     at the backend's configured poll interval (zero configs fall back to
//!     1 s), and ticks missed during slow iterations are dropped.
//!   - Events travel on a bounded `mpsc::sync_channel(64)` (Go:
//!     `make(chan event.Event, 64)`); a full queue drops the event, exactly
//!     like Go's select-with-default. State-change signals use
//!     `sync_channel(1)` (Go: `chan struct{}, 1`), which coalesces.
//!   - Shared state (session trackers, cooldowns) sits behind `Mutex`es
//!     where Go used `sync.Mutex`, so [`Engine::view`], [`Engine::events`]
//!     and [`Engine::state_change`] are usable from any thread while the
//!     pollers run (the tray holds an `Arc<Engine>`). The consumer-side
//!     `mpsc::Receiver` endpoints are themselves `Mutex`-wrapped — std's
//!     `Receiver` is `!Sync`, so a raw `&Receiver` could not be used from
//!     another thread the way Go's receive-only channel can.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    /// (`View.Counts`, view.go:48). Skips down sessions (they contribute to
    /// `down` only); only latched idle/done agents count as waiting.
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.sessions {
            if !s.up {
                c.down += 1;
                continue;
            }
            for a in &s.agents {
                if a.status == herdr::STATUS_BLOCKED {
                    // FIX(go-port): the Go switch (view.go:48) omits
                    // StatusBlocked, so c.Blocked stayed 0 and Severity()
                    // could never return "blocked" — the tray icon never
                    // went red off severity and Summary never showed the
                    // blocked count its own docstring promises. The panel
                    // redesign's header dot exposed it; count it.
                    c.blocked += 1;
                } else if a.status == herdr::STATUS_WORKING {
                    c.working += 1;
                } else if a.status == herdr::STATUS_IDLE || a.status == herdr::STATUS_DONE {
                    if a.wants {
                        c.waiting += 1;
                    }
                }
            }
        }
        c
    }

    /// The view state driving the tray icon color. Precedence: blocked
    /// (red) > offline (amber) > working (blue) > waiting (green) > idle
    /// (gray). Working beats waiting so a busy fleet reads as "busy";
    /// green is transition-latched via [`AgentView::wants`]
    /// (`View.Severity`, view.go:76).
    pub fn severity(&self) -> &'static str {
        let c = self.counts();
        if c.blocked > 0 {
            "blocked"
        } else if c.down > 0 {
            "down"
        } else if c.working > 0 {
            "working"
        } else if c.waiting > 0 {
            "waiting"
        } else {
            "idle"
        }
    }

    /// Renders counts for tooltips and menus, e.g.
    /// "2 working · 1 waiting · 1 blocked · 1 offline"; "all quiet" when
    /// everything is empty (`View.Summary`, view.go:94).
    pub fn summary(&self) -> String {
        let c = self.counts();
        let mut parts: Vec<String> = Vec::new();
        if c.working > 0 {
            parts.push(plural(c.working, "working"));
        }
        if c.waiting > 0 {
            parts.push(plural(c.waiting, "waiting"));
        }
        if c.blocked > 0 {
            parts.push(plural(c.blocked, "blocked"));
        }
        if c.down > 0 {
            parts.push(plural(c.down, "offline"));
        }
        if parts.is_empty() {
            return "all quiet".to_string();
        }
        parts.join(" · ")
    }
}

/// `engine.plural` (view.go:115): "1 working" / "2 working".
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}")
    }
}

/// The color class for one agent row (tray flyout dot and wash, menu
/// label). done and a latched idle read as waiting: the strongest
/// "finished, needs you" signal; an unlatched idle/done pane is quiet
/// gray, and unknown stays gray (`engine.RowSeverity`, view.go:193).
pub fn row_severity(status: &str, wants: bool) -> &'static str {
    if status == herdr::STATUS_BLOCKED {
        return "blocked";
    }
    if status == herdr::STATUS_WORKING {
        return "working";
    }
    if (status == herdr::STATUS_IDLE || status == herdr::STATUS_DONE) && wants {
        return "waiting";
    }
    "idle"
}

/// Orders spaces by attention priority: blocked, offline, waiting (needs
/// you), working, quiet (`engine.spaceRank`, view.go:167).
fn space_rank(s: &SessionView) -> i32 {
    if !s.up {
        return 1;
    }
    let mut r = 4; // idle
    for a in &s.agents {
        if a.status == herdr::STATUS_BLOCKED {
            return 0;
        } else if a.status == herdr::STATUS_IDLE || a.status == herdr::STATUS_DONE {
            if a.wants && r > 2 {
                r = 2;
            }
        } else if a.status == herdr::STATUS_WORKING {
            if r > 3 {
                r = 3;
            }
        }
    }
    r
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
/// (sticky unknown status, the wants latch, hysteresis) are invariants the
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

    /// Records the socket ref and distro home the backend discovered for
    /// this session. Go assigns `t.ref`/`t.home` directly in pollSession;
    /// the fields are private here, so [`Engine::poll_session`] goes
    /// through this setter.
    fn set_session_ref(&mut self, ref_: &str, home: &str) {
        self.ref_ = ref_.to_string();
        self.home = home.to_string();
    }

    /// Folds a successful snapshot in and returns the transition events.
    /// The first snapshot is a baseline: it populates state without events.
    /// `changed` is true only when the view-relevant state actually moved;
    /// the tray rebuilds its menu on it and must not do that every poll
    /// (`tracker.apply`, tracker.go:44).
    pub fn apply(&mut self, snap: &herdr::Snapshot, now: event::Time) -> (Vec<Event>, bool) {
        let first = !self.seen;
        let was_down = self.seen && !self.up;
        let mut changed = first || was_down;
        let mut evs: Vec<Event> = Vec::new();
        if was_down {
            evs.push(self.session_event(Kind::SessionUp, now));
        }
        self.seen = true;
        self.up = true;
        self.fails = 0;

        let mut next: HashMap<String, herdr::Agent> = HashMap::with_capacity(snap.agents.len());
        let mut next_order: Vec<String> = Vec::with_capacity(snap.agents.len());
        for (i, a_raw) in snap.agents.iter().enumerate() {
            let key = agent_key(a_raw, i);
            let mut a = a_raw.clone();
            let prev = self.agents.get(&key).cloned();
            if let Some(p) = &prev {
                a = self.sticky_status(p.clone(), a);
            }
            match &prev {
                None => {
                    if !first {
                        evs.push(self.agent_event(
                            Kind::AgentSpawned,
                            &herdr::Agent::default(),
                            &a,
                            now,
                        ));
                        changed = true;
                    }
                }
                Some(p) => {
                    if p.status != a.status {
                        if let Some(ev) = self.transition(p, &a, now) {
                            evs.push(ev);
                        }
                        self.track_wants(&key, &p.status, &a.status);
                        changed = true;
                    } else if p != &a {
                        // title/project/focus refresh: view-only change
                        changed = true;
                    }
                }
            }
            next.insert(key.clone(), a);
            next_order.push(key);
        }
        for (key, prev) in self.agents.iter() {
            if !next.contains_key(key) {
                if !first {
                    evs.push(self.agent_event(Kind::AgentLeft, prev, &herdr::Agent::default(), now));
                }
                self.wants.remove(key);
                changed = true;
            }
        }
        self.agents = next;
        self.order = next_order;
        (evs, changed)
    }

    /// Records a failed fetch, emitting session_down on the up→down edge
    /// after [`DOWN_AFTER`] consecutive failures; transient spikes stay
    /// silent (`tracker.fail`, tracker.go:143).
    pub fn fail(&mut self, now: event::Time) -> (Vec<Event>, bool) {
        self.fails += 1;
        let mut evs: Vec<Event> = Vec::new();
        let mut changed = false;
        if self.up && self.fails >= DOWN_AFTER {
            self.up = false;
            evs.push(self.session_event(Kind::SessionDown, now));
            changed = true;
        }
        (evs, changed)
    }

    /// Bridges unknown flaps: herdr's screen detection can briefly report
    /// unknown for a live agent, which would drop it from every count. An
    /// unknown reading keeps the previous status; title, project and focus
    /// still refresh (`tracker.stickyStatus`, tracker.go:100).
    fn sticky_status(&self, prev: herdr::Agent, mut cur: herdr::Agent) -> herdr::Agent {
        if cur.status == herdr::STATUS_UNKNOWN && prev.status != herdr::STATUS_UNKNOWN {
            cur.status = prev.status;
        }
        cur
    }

    /// Maintains the attention latch on status transitions: an observed
    /// finish (working → idle/done) latches, any departure from idle/done
    /// (new work, blocked) unlatches (`tracker.trackWants`,
    /// tracker.go:111).
    fn track_wants(&mut self, key: &str, prev_status: &str, cur_status: &str) {
        if cur_status == herdr::STATUS_IDLE || cur_status == herdr::STATUS_DONE {
            if prev_status == herdr::STATUS_WORKING {
                self.wants.insert(key.to_string());
            }
        } else {
            self.wants.remove(key);
        }
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
        let kind = if cur.status == herdr::STATUS_BLOCKED && prev.status != herdr::STATUS_BLOCKED {
            Kind::AgentBlocked
        } else if prev.status == herdr::STATUS_WORKING && cur.status == herdr::STATUS_IDLE {
            Kind::AgentIdle
        } else if prev.status == herdr::STATUS_WORKING && cur.status == herdr::STATUS_DONE {
            Kind::AgentDone
        } else if cur.status == herdr::STATUS_WORKING && prev.status != herdr::STATUS_WORKING {
            Kind::AgentWorking
        } else {
            return None; // e.g. unknown flapping or idle ↔ done reshuffles
        };
        Some(self.agent_event(kind, prev, cur, now))
    }

    fn session_event(&self, kind: Kind, now: event::Time) -> Event {
        Event {
            kind,
            time: now,
            source: "herdr".to_string(),
            host: self.host.clone(),
            session: self.name.clone(),
            agent: String::new(),
            from: String::new(),
            to: String::new(),
            title: String::new(),
            project: String::new(),
            pane_id: String::new(),
            focused: false,
        }
    }

    fn agent_event(
        &self,
        kind: Kind,
        prev: &herdr::Agent,
        cur: &herdr::Agent,
        now: event::Time,
    ) -> Event {
        let mut ev = Event {
            kind,
            time: now,
            source: "herdr".to_string(),
            host: self.host.clone(),
            session: self.name.clone(),
            agent: cur.name.clone(),
            from: prev.status.clone(),
            to: cur.status.clone(),
            title: cur.title.clone(),
            project: cur.project.clone(),
            pane_id: cur.pane_id.clone(),
            focused: cur.focused,
        };
        if ev.agent.is_empty() {
            ev.agent = prev.name.clone(); // agent_left carries the departed agent
        }
        if ev.title.is_empty() {
            ev.title = prev.title.clone();
        }
        if ev.project.is_empty() {
            ev.project = prev.project.clone();
        }
        if ev.pane_id.is_empty() {
            ev.pane_id = prev.pane_id.clone();
        }
        ev
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
// Session-name globs (engine.matchAny → Go path.Match)

/// Go's `path.Match` over '/'-separated names (go1.26 `path/match.go`,
/// byte-for-byte port: `*` matches any run of non-'/' bytes, `?` one
/// non-'/' byte, `[...]` classes with `^` negation and `\` escapes).
/// `Err(())` is Go's ErrBadPattern (malformed pattern).
fn path_match(pattern: &str, name: &str) -> Result<bool, ()> {
    let mut pattern = pattern.as_bytes();
    let mut name: Vec<u8> = name.as_bytes().to_vec();
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // Trailing * matches the rest of the name unless it has a '/'.
            return Ok(!name.contains(&b'/'));
        }
        let (ok, t) = match_chunk(chunk, name.clone())?;
        // if we're the last chunk, make sure we've exhausted the name,
        // otherwise we could still match using the star
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if star {
            // Look for a match skipping i+1 bytes; cannot skip '/'.
            let cur = name;
            for i in 0..cur.len() {
                if cur[i] == b'/' {
                    break;
                }
                let (ok, t) = match_chunk(chunk, cur[i + 1..].to_vec())?;
                if ok {
                    // if we're the last chunk, make sure we exhausted the name
                    if pattern.is_empty() && !t.is_empty() {
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
            }
        }
        // Before returning false with no error, check that the remainder
        // of the pattern is syntactically valid.
        let mut p = pattern;
        while !p.is_empty() {
            let (_, chunk, rest) = scan_chunk(p);
            p = rest;
            match_chunk(chunk, Vec::new())?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

/// Go path.scanChunk: the next pattern segment, a non-star string possibly
/// preceded by a star. Returns (star, chunk, rest).
fn scan_chunk(pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut pattern = pattern;
    let mut star = false;
    while !pattern.is_empty() && pattern[0] == b'*' {
        pattern = &pattern[1..];
        star = true;
    }
    let mut in_range = false;
    let mut i = 0usize;
    while i < pattern.len() {
        match pattern[i] {
            b'\\' => {
                // error check handled in match_chunk: bad pattern.
                if i + 1 < pattern.len() {
                    i += 1;
                }
            }
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => {
                return (star, &pattern[..i], &pattern[i..]);
            }
            _ => {}
        }
        i += 1;
    }
    (star, pattern, &[])
}

/// Go path.matchChunk: checks whether `chunk` matches the beginning of `s`.
/// Returns (matched, remainder-of-s); Ok((false, _)) on mismatch, Err(())
/// for a malformed pattern. All single-character operators: literals,
/// char classes, and `?`.
fn match_chunk(chunk: &[u8], mut s: Vec<u8>) -> Result<(bool, Vec<u8>), ()> {
    let mut chunk: &[u8] = chunk;
    // failed records whether the match has failed. After the match fails,
    // the loop keeps processing chunk, checking that the pattern is
    // well-formed but no longer reading s.
    let mut failed = false;
    while !chunk.is_empty() {
        failed = failed || s.is_empty();
        match chunk[0] {
            b'[' => {
                // character class
                let mut r: char = '\0';
                if !failed {
                    let (c, n) = decode_rune(&s);
                    r = c;
                    s.drain(..n);
                }
                chunk = &chunk[1..];
                // possibly negated
                let mut negated = false;
                if !chunk.is_empty() && chunk[0] == b'^' {
                    negated = true;
                    chunk = &chunk[1..];
                }
                // parse all ranges
                let mut matched = false;
                let mut nrange = 0usize;
                loop {
                    if !chunk.is_empty() && chunk[0] == b']' && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let lo = get_esc(&mut chunk)?;
                    let mut hi = lo;
                    if chunk[0] == b'-' {
                        chunk = &chunk[1..]; // Go: getEsc(chunk[1:])
                        hi = get_esc(&mut chunk)?;
                    }
                    matched = matched || (lo <= r && r <= hi);
                    nrange += 1;
                }
                failed = failed || matched == negated;
            }
            b'?' => {
                if !failed {
                    failed = s[0] == b'/';
                    let (_, n) = decode_rune(&s);
                    s.drain(..n);
                }
                chunk = &chunk[1..];
            }
            b'\\' => {
                chunk = &chunk[1..];
                if chunk.is_empty() {
                    return Err(());
                }
                // Go's fallthrough into the literal case.
                if !failed {
                    failed = chunk[0] != s[0];
                    s.drain(..1);
                }
                chunk = &chunk[1..];
            }
            _ => {
                if !failed {
                    failed = chunk[0] != s[0];
                    s.drain(..1);
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok((false, Vec::new()));
    }
    Ok((true, s))
}

/// Go path.getEsc: a possibly-escaped character from a character class,
/// consuming it from `chunk`.
fn get_esc(chunk: &mut &[u8]) -> Result<char, ()> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return Err(());
    }
    if chunk[0] == b'\\' {
        *chunk = &(*chunk)[1..];
        if chunk.is_empty() {
            return Err(());
        }
    }
    let (r, n) = decode_rune(chunk);
    if r == '\u{FFFD}' && n == 1 {
        return Err(());
    }
    *chunk = &(*chunk)[n..];
    if chunk.is_empty() {
        return Err(());
    }
    Ok(r)
}

/// Go utf8.DecodeRuneInString: decodes the first rune of `s`, returning
/// (RuneError, 1) for invalid input and (RuneError, 0) when empty.
fn decode_rune(s: &[u8]) -> (char, usize) {
    if s.is_empty() {
        return ('\u{FFFD}', 0);
    }
    let n = match s[0] {
        b if b < 0x80 => 1,
        b if (0xC0..0xE0).contains(&b) => 2,
        b if (0xE0..0xF0).contains(&b) => 3,
        b if (0xF0..0xF8).contains(&b) => 4,
        _ => return ('\u{FFFD}', 1), // continuation byte or invalid lead
    };
    if s.len() >= n {
        if let Ok(st) = std::str::from_utf8(&s[..n]) {
            if let Some(c) = st.chars().next() {
                return (c, n);
            }
        }
    }
    ('\u{FFFD}', 1)
}

/// `engine.matchAny` (engine.go:273): s matches any pattern; malformed
/// patterns never match, like Go's `err == nil && ok` check.
fn match_any(pats: &[String], s: &str) -> bool {
    pats.iter().any(|p| path_match(p, s).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Engine (engine.go) — polling loop.

/// Parameterizes an [`Engine`] (`engine.Config`).
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// event kinds allowed through to sinks (Go: map[event.Kind]bool).
    pub kinds: HashSet<Kind>,
    /// min spacing between identical (kind,session,pane) events.
    pub cooldown: Duration,
    /// session-name globs to keep; empty = all.
    pub include: Vec<String>,
    /// session-name globs to drop.
    pub exclude: Vec<String>,
    pub herdr: config::Herdr,
}

/// One enabled backend with its poll/timeout budgets (`engine.backendRun`).
struct BackendRun {
    backend: Box<dyn herdr::Backend>,
    poll: Duration,
    timeout: Duration,
}

/// Watches herdr sessions and emits filtered events (`engine.Engine`). The
/// Go version fans one poller goroutine per backend with channels for
/// events and state changes; the Rust port keeps that shape with
/// `std::thread::scope` poller threads (spawned by [`Engine::run`], joined
/// when it returns) and an `Arc<AtomicBool>` stop flag in place of
/// `context.Context`. See the module header for the full thread model.
pub struct Engine {
    cfg: Config,
    /// enabled backends with their poll/timeout budgets.
    backends: Vec<BackendRun>,
    /// last known tracker per session, keyed "host/name".
    sessions: Mutex<HashMap<String, Tracker>>,
    /// last emission per "kind|host|session|pane" cooldown key.
    cooldowns: Mutex<HashMap<String, Instant>>,
    events_tx: mpsc::SyncSender<Event>,
    // PORT-NOTE: std's mpsc::Receiver is !Sync, so the receive ends sit in
    // Mutexes to keep Engine Sync and the streams consumable from any
    // thread, matching Go's receive-only channel semantics.
    events_rx: Mutex<mpsc::Receiver<Event>>,
    state_tx: mpsc::SyncSender<()>,
    state_rx: Mutex<mpsc::Receiver<()>>,
}

impl Engine {
    /// Resolves the configured backends and returns a ready engine. It
    /// errors when no backend could be enabled at all (`engine.New`,
    /// engine.go:71). The auto/on/off resolution of `herdr.local`,
    /// `herdr.windows` and `herdr.wsl` happens here: local defaults on
    /// off-Windows, windows defaults on everywhere (a resolve failure
    /// under "auto" just disables it), wsl defaults on only on
    /// Windows-native builds.
    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let mut e = Engine::from_cfg(cfg);
        let h = e.cfg.herdr.clone();
        let mut backends: Vec<BackendRun> = Vec::new();
        // Auto: enabled when the socket directory + binary exist (a config
        // error under "on" still installs the backend, like Go).
        let (use_local, local_explicit) =
            Engine::want(&h.local.enabled, !cfg!(target_os = "windows"));
        if use_local {
            let l = herdr::Local::new(&h.local.socket_dir, &h.local.binary);
            if local_explicit || l.available() {
                backends.push(BackendRun {
                    backend: Box::new(l),
                    poll: h.local.poll.d(),
                    timeout: h.local.timeout.d(),
                });
            }
        }
        // Auto: enabled whenever herdr.exe resolves (native Windows, or the
        // Windows side polled from WSL via interop). A resolve failure under
        // "auto" just disables the backend.
        let (use_windows, windows_explicit) = Engine::want(&h.windows.enabled, true);
        if use_windows {
            match herdr::Windows::new(&h.windows.binary, &h.windows.sessions) {
                Ok(w) => backends.push(BackendRun {
                    backend: Box::new(w),
                    poll: h.windows.poll.d(),
                    timeout: h.windows.timeout.d(),
                }),
                Err(err) => {
                    if windows_explicit {
                        anyhow::bail!("herdr.windows backend: {err}");
                    }
                }
            }
        }
        let (use_wsl, _) = Engine::want(&h.wsl.enabled, cfg!(target_os = "windows"));
        if use_wsl {
            let wsl = herdr::Wsl::new(&h.wsl.distro, &h.wsl.sessions, &h.wsl.extra_path, h.wsl.rescan.d());
            backends.push(BackendRun {
                backend: Box::new(wsl),
                poll: h.wsl.poll.d(),
                timeout: h.wsl.timeout.d(),
            });
        }
        if backends.is_empty() {
            anyhow::bail!(
                "no herdr backends enabled (local: {:?}, windows: {:?}, wsl: {:?})",
                h.local.enabled,
                h.windows.enabled,
                h.wsl.enabled
            );
        }
        e.backends = backends;
        Ok(e)
    }

    /// Overrides backend resolution — tests (`engine.WithBackends`). The
    /// Go builder-Option becomes a constructor variant; injected backends
    /// poll every 10 ms with a 1 s fetch timeout, and config-based
    /// resolution is skipped.
    pub fn with_backends(cfg: Config, backends: Vec<Box<dyn herdr::Backend>>) -> Engine {
        let mut e = Engine::from_cfg(cfg);
        e.backends = backends
            .into_iter()
            .map(|b| BackendRun {
                backend: b,
                poll: Duration::from_millis(10),
                timeout: Duration::from_secs(1),
            })
            .collect();
        e
    }

    fn from_cfg(cfg: Config) -> Engine {
        // Go: make(chan event.Event, 64) and make(chan struct{}, 1).
        let (events_tx, events_rx) = mpsc::sync_channel(64);
        let (state_tx, state_rx) = mpsc::sync_channel(1);
        Engine {
            cfg,
            backends: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashMap::new()),
            events_tx,
            events_rx: Mutex::new(events_rx),
            state_tx,
            state_rx: Mutex::new(state_rx),
        }
    }

    /// The configured WSL distro for the wsl backend ("" = wsl.exe's
    /// default distro) (`engine.WSLDistro`).
    pub fn wsl_distro(&self) -> &str {
        &self.cfg.herdr.wsl.distro
    }

    /// The socket reference and distro home for a session as its backend
    /// discovered it; used to target one-shot CLI calls (agent focus) at
    /// the right server (`engine.SessionSocket`, engine.go:123). Returns
    /// ("", "") for unknown sessions.
    pub fn session_socket(&self, host: &str, name: &str) -> (String, String) {
        let sessions = self.sessions.lock().expect("engine sessions lock");
        match sessions.get(&format!("{host}/{name}")) {
            Some(t) => {
                let (r, h) = t.session_ref();
                (r.to_string(), h.to_string())
            }
            None => (String::new(), String::new()),
        }
    }

    /// The configured extra PATH entries for herdr calls inside the WSL
    /// distro (`engine.WSLExtraPath`).
    pub fn wsl_extra_path(&self) -> Vec<String> {
        self.cfg.herdr.wsl.extra_path.clone()
    }

    /// Lists the enabled backend names (`engine.Backends`).
    pub fn backends(&self) -> Vec<String> {
        self.backends.iter().map(|br| br.backend.name()).map(String::from).collect()
    }

    /// The filtered event stream (`engine.Events`); buffered to 64, full
    /// queue drops. The `Receiver` is behind a `Mutex` because std's
    /// `Receiver` is `!Sync`; a consumer locks it and `recv()`s (blocking,
    /// like Go's `range e.Events()`) or `try_recv()`s.
    pub fn events(&self) -> &Mutex<mpsc::Receiver<Event>> {
        &self.events_rx
    }

    /// Signals (coalesced) that [`Engine::view`] would return something
    /// new (`engine.StateChange`). Behind a `Mutex` like [`Engine::events`];
    /// capacity is 1, so signals coalesce and `try_recv` drains one slot.
    pub fn state_change(&self) -> &Mutex<mpsc::Receiver<()>> {
        &self.state_rx
    }

    /// Polls every backend until `stop` is set (`engine.Run`, engine.go:153).
    /// Blocks the calling thread: it spawns one poller thread per backend
    /// on a `std::thread::scope`, waits for `stop` (Go's `<-ctx.Done()`),
    /// then joins them all (Go's `wg.Wait()`) before returning.
    pub fn run(&self, stop: Arc<AtomicBool>) {
        std::thread::scope(|scope| {
            for br in &self.backends {
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name(format!("engine-poll-{}", br.backend.name()))
                    .spawn_scoped(scope, move || self.poll_loop(br, &stop))
                    .expect("spawn engine poll thread");
            }
            // Go: <-ctx.Done().
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Leaving the scope joins every poller (Go: wg.Wait()).
        });
    }

    /// One backend's poll loop (`engine.loop`, engine.go:166). Ticks on a
    /// fixed schedule at the configured poll interval (a zero config falls
    /// back to 1 s, guarding zero-value configs), re-discovers when the
    /// backend's rescan period has elapsed, and polls every discovered
    /// session each tick. On discovery failure the previous session list
    /// keeps polling; the warning fires only when the error text changes.
    fn poll_loop(&self, br: &BackendRun, stop: &AtomicBool) {
        let poll = if br.poll.is_zero() {
            Duration::from_secs(1) // guard zero-value configs
        } else {
            br.poll
        };
        let mut next_tick = Instant::now() + poll;
        let mut sessions: Vec<herdr::Session> = Vec::new();
        let mut last_err = String::new();
        let mut next_scan: Option<Instant> = None; // Go's zero time: due now
        loop {
            // Go: select { case <-ctx.Done(): return; case <-ticker.C: }.
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let now = Instant::now();
                if now >= next_tick {
                    break;
                }
                std::thread::sleep((next_tick - now).min(Duration::from_millis(50)));
            }
            let now = Instant::now();
            if next_scan.is_none_or(|ns| now > ns) {
                // PORT-NOTE: Go bounds Discover with the caller's context
                // only; the Rust Backend trait maps contexts to explicit
                // timeouts (see herdr.rs), so the backend's configured
                // timeout is used here too.
                match br.backend.discover(br.timeout) {
                    Err(err) => {
                        let msg = err.to_string();
                        if msg != last_err {
                            self.warnf(&format!("session discovery failed: {err}"));
                            last_err = msg;
                        }
                    }
                    Ok(ss) => {
                        last_err = String::new();
                        sessions = self.filter(ss);
                    }
                }
                next_scan = Some(now + br.backend.rescan());
            }
            for s in &sessions {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                self.poll_session(br, s);
            }
            // Fixed tick schedule: skip ticks a slow iteration missed (Go's
            // ticker drops ticks to make up for slow receivers).
            next_tick += poll;
            while next_tick <= Instant::now() {
                next_tick += poll;
            }
        }
    }

    /// Fetches one session and folds the result into its tracker
    /// (`engine.pollSession`, engine.go:208).
    fn poll_session(&self, br: &BackendRun, s: &herdr::Session) {
        let key = format!("{}/{}", s.host, s.name);
        // Go: context.WithTimeout(ctx, br.timeout) around Fetch.
        let fetched = br.backend.fetch(s, br.timeout);
        let now = event::Time::now();

        let (evs, changed) = {
            let mut sessions = self.sessions.lock().expect("engine sessions lock");
            let t = sessions
                .entry(key.clone())
                .or_insert_with(|| Tracker::new(&s.host, &s.name));
            t.set_session_ref(&s.ref_, &s.home);
            match &fetched {
                Err(_) => t.fail(now),
                Ok(snap) => t.apply(snap, now),
            }
        };

        if let Err(err) = &fetched {
            self.debugf(&format!("snapshot {key} failed: {err}"));
        }
        self.emit(evs, changed);
    }

    /// Performs a single synchronous discovery+fetch pass per backend (for
    /// `agent-notify probe`); returns per-session fetch errors
    /// (`engine.ProbeOnce`, engine.go:238). Each session is fetched twice,
    /// once for the error report and once through [`Engine::poll_session`]
    /// — the Go probe does the same.
    pub fn probe_once(&self) -> Vec<anyhow::Error> {
        let mut errs = Vec::new();
        for br in &self.backends {
            // PORT-NOTE: Go bounds Discover with the caller's context only;
            // see poll_loop for the timeout mapping.
            let ss = match br.backend.discover(br.timeout) {
                Ok(ss) => ss,
                Err(err) => {
                    errs.push(anyhow::anyhow!("{}: discover: {err}", br.backend.name()));
                    continue;
                }
            };
            for s in self.filter(ss) {
                if let Err(err) = br.backend.fetch(&s, br.timeout) {
                    errs.push(anyhow::anyhow!("{}: {}: {err}", br.backend.name(), s.name));
                }
                self.poll_session(br, &s);
            }
        }
        errs
    }

    /// Applies the include/exclude session-name globs
    /// (`engine.filter`, engine.go:259).
    fn filter(&self, ss: Vec<herdr::Session>) -> Vec<herdr::Session> {
        ss.into_iter()
            .filter(|s| {
                if !self.cfg.include.is_empty() && !match_any(&self.cfg.include, &s.name) {
                    return false;
                }
                if match_any(&self.cfg.exclude, &s.name) {
                    return false;
                }
                true
            })
            .collect()
    }

    /// Emits kind-filtered, cooldown-throttled events and the coalesced
    /// state-change signal (`engine.emit`, engine.go:282).
    fn emit(&self, evs: Vec<Event>, changed: bool) {
        for ev in evs {
            if !self.cfg.kinds.contains(&ev.kind) {
                continue;
            }
            if !self.cooldown_allow(&ev) {
                continue;
            }
            // Go: select { case e.events <- ev: default: drop } on the
            // 64-buffered channel.
            match self.events_tx.try_send(ev) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(ev)) => {
                    self.debugf(&format!(
                        "event queue full; dropping {} {}/{}",
                        ev.kind.as_str(),
                        ev.host,
                        ev.session
                    ));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        if changed {
            let _ = self.state_tx.try_send(());
        }
    }

    /// Min spacing between identical (kind,host,session,pane) events
    /// (`engine.cooldownAllow`, engine.go:304). Zero disables it.
    fn cooldown_allow(&self, ev: &Event) -> bool {
        if self.cfg.cooldown.is_zero() {
            return true;
        }
        let key = format!(
            "{}|{}|{}|{}",
            ev.kind.as_str(),
            ev.host,
            ev.session,
            ev.pane_id
        );
        let mut cooldowns = self.cooldowns.lock().expect("engine cooldowns lock");
        if let Some(t) = cooldowns.get(&key) {
            if t.elapsed() < self.cfg.cooldown {
                return false;
            }
        }
        cooldowns.insert(key, Instant::now());
        true
    }

    // PORT-NOTE: Go's warnf/debugf write through the optional slog.Logger
    // attached with WithLogger. The scaffold's Engine contract has no
    // logger field, so both are no-ops here — the same silence as a Go
    // Engine built without WithLogger. Deviation listed in the port report.
    fn warnf(&self, _msg: &str) {}

    fn debugf(&self, _msg: &str) {}

    /// Returns a sorted, deep copy of the current state. Sessions appear
    /// once they have had at least one successful snapshot; agents keep
    /// their last known state while a session is offline (`engine.View()`,
    /// view.go:125). Sessions sort by attention priority (blocked, offline,
    /// waiting, working, quiet — [`space_rank`]), then host, then name.
    pub fn view(&self) -> View {
        let sessions_map = self.sessions.lock().expect("engine sessions lock");
        let mut v = View::default();
        for t in sessions_map.values() {
            if !t.seen {
                continue; // never observed: don't show phantom sessions
            }
            let mut sv = SessionView {
                host: t.host.clone(),
                name: t.name.clone(),
                up: t.up,
                agents: Vec::new(),
            };
            for k in &t.order {
                // agents keep the source's stable order, no reordering
                // between refreshes (rows must not cycle in the UI).
                // Go's map read yields a zero Agent when missing; the
                // order/agents pair is kept consistent by apply.
                let a = t.agents.get(k).cloned().unwrap_or_default();
                sv.agents.push(AgentView {
                    name: a.name.clone(),
                    status: a.status.clone(),
                    wants: t.wants.contains(k),
                    title: a.title.clone(),
                    project: a.project.clone(),
                    pane_id: a.pane_id.clone(),
                    focused: a.focused,
                });
            }
            v.sessions.push(sv);
        }
        // spaces sorted by attention priority, then name; only a space
        // whose severity class changes moves position
        v.sessions.sort_by(|a, b| {
            let (ra, rb) = (space_rank(a), space_rank(b));
            if ra != rb {
                return ra.cmp(&rb);
            }
            if a.host != b.host {
                return a.host.cmp(&b.host);
            }
            a.name.cmp(&b.name)
        });
        v
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

// ---------------------------------------------------------------------------
// Tests — ports of internal/engine/tracker_test.go and engine_test.go.

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tracker_test.go helpers ----

    /// tracker_test.go's agent(): name "claude", project "api-server".
    fn agent(status: &str, pane: &str, title: &str) -> herdr::Agent {
        herdr::Agent {
            pane_id: pane.to_string(),
            name: "claude".to_string(),
            status: status.to_string(),
            title: title.to_string(),
            project: "api-server".to_string(),
            focused: false,
        }
    }

    fn snap(agents: Vec<herdr::Agent>) -> herdr::Snapshot {
        herdr::Snapshot { agents }
    }

    /// tracker_test.go's kinds(): the kind of each event.
    fn kinds(evs: &[Event]) -> Vec<Kind> {
        evs.iter().map(|e| e.kind).collect()
    }

    /// A fixed stand-in for time.Now() (tests never assert on times).
    fn now() -> event::Time {
        event::Time { secs: 1_760_000_000, nanos: 0 }
    }

    /// now.Add(i seconds), for the hysteresis test.
    fn now_plus(secs: u64) -> event::Time {
        event::Time { secs: now().secs + secs, nanos: 0 }
    }

    // ---- view.go Counts/Severity — see the FIX(go-port) note in counts() ----

    #[test]
    fn counts_and_severity_count_blocked() {
        let v = View {
            sessions: vec![SessionView {
                host: "wsl".into(),
                name: "s".into(),
                up: true,
                agents: vec![
                    AgentView {
                        name: "a".into(),
                        status: "blocked".into(),
                        pane_id: "p1".into(),
                        ..AgentView::default()
                    },
                    AgentView {
                        name: "b".into(),
                        status: "working".into(),
                        pane_id: "p2".into(),
                        ..AgentView::default()
                    },
                    AgentView {
                        name: "c".into(),
                        status: "done".into(),
                        wants: true,
                        pane_id: "p3".into(),
                        ..AgentView::default()
                    },
                ],
            }],
        };
        let c = v.counts();
        assert_eq!((c.blocked, c.working, c.waiting), (1, 1, 1));
        assert_eq!(v.severity(), "blocked", "the tray must go red on blocked");
        assert!(
            v.summary().contains("1 blocked"),
            "summary must carry the blocked count: {}", v.summary()
        );
    }

    #[test]
    fn first_snapshot_is_baseline() {
        let mut tr = Tracker::new("local", "work");
        let (evs, changed) = tr.apply(&snap(vec![agent("idle", "w1:p1", "t")]), now());
        assert!(evs.is_empty(), "baseline emitted events: {:?}", kinds(&evs));
        assert!(changed, "baseline should mark state changed (view appears)");
    }

    #[test]
    fn attention_transitions() {
        let cases: &[(&str, &str, Option<Kind>)] = &[
            ("working", "idle", Some(Kind::AgentIdle)),
            ("working", "done", Some(Kind::AgentDone)),
            ("idle", "blocked", Some(Kind::AgentBlocked)),
            ("working", "blocked", Some(Kind::AgentBlocked)),
            ("unknown", "blocked", Some(Kind::AgentBlocked)),
            ("idle", "working", Some(Kind::AgentWorking)),
            // noise: no event
            ("idle", "done", None),
            ("unknown", "unknown", None),
            ("blocked", "unknown", None),
            ("idle", "idle", None),
        ];
        for (from, to, want) in cases {
            let mut tr = Tracker::new("local", "work");
            tr.apply(&snap(vec![agent(from, "w1:p1", "t")]), now());
            let (evs, _) = tr.apply(&snap(vec![agent(to, "w1:p1", "t")]), now());
            match want {
                None => {
                    assert!(evs.is_empty(), "{from}→{to}: unexpected events {:?}", kinds(&evs));
                }
                Some(want) => {
                    assert!(
                        evs.len() == 1 && evs[0].kind == *want,
                        "{from}→{to}: got {:?}, want [{want:?}]",
                        kinds(&evs)
                    );
                    let ev = &evs[0];
                    assert!(
                        ev.from == *from
                            && ev.to == *to
                            && ev.agent == "claude"
                            && ev.session == "work"
                            && ev.host == "local",
                        "{from}→{to}: event fields wrong: {ev:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn spawn_and_leave() {
        let mut tr = Tracker::new("local", "work");
        let base = snap(vec![agent("idle", "w1:p1", "one")]);
        tr.apply(&base, now());

        let (evs, _) = tr.apply(
            &snap(vec![agent("idle", "w1:p1", "one"), agent("working", "w2:p1", "two")]),
            now(),
        );
        assert!(
            evs.len() == 1 && evs[0].kind == Kind::AgentSpawned && evs[0].pane_id == "w2:p1",
            "spawn: {:?}",
            kinds(&evs)
        );

        let (evs, _) = tr.apply(&base, now());
        assert!(
            evs.len() == 1
                && evs[0].kind == Kind::AgentLeft
                && evs[0].pane_id == "w2:p1"
                && evs[0].agent == "claude",
            "leave: {:?} {:?}",
            kinds(&evs),
            evs
        );
    }

    #[test]
    fn session_up_down_edges() {
        let mut tr = Tracker::new("local", "work");
        tr.apply(&snap(vec![agent("idle", "w1:p1", "t")]), now());

        // the down edge arrives on the downAfter-th consecutive failure
        let mut evs: Vec<Event> = Vec::new();
        let mut changed = false;
        for _ in 0..DOWN_AFTER {
            let (e, c) = tr.fail(now());
            evs = e;
            changed = c;
        }
        assert!(
            evs.len() == 1 && evs[0].kind == Kind::SessionDown && changed,
            "down edge: {:?} changed={changed}",
            kinds(&evs)
        );
        // repeated failures stay quiet
        let (evs, changed) = tr.fail(now());
        assert!(
            evs.is_empty() && !changed,
            "repeat fail should be quiet: {:?}",
            kinds(&evs)
        );
        let (evs, _) = tr.apply(&snap(vec![agent("idle", "w1:p1", "t")]), now());
        assert!(
            evs.len() == 1 && evs[0].kind == Kind::SessionUp,
            "up edge: {:?}",
            kinds(&evs)
        );
    }

    #[test]
    fn fail_before_first_success_is_silent() {
        let mut tr = Tracker::new("local", "ghost");
        let (evs, changed) = tr.fail(now());
        assert!(
            evs.is_empty() && !changed,
            "never-seen session should not emit: {:?}",
            kinds(&evs)
        );
    }

    #[test]
    fn view_severity_and_counts() {
        let mut v = View::default();
        v.sessions = vec![
            SessionView {
                host: "local".to_string(),
                name: "a".to_string(),
                up: true,
                agents: vec![
                    AgentView { status: "working".to_string(), ..AgentView::default() },
                    AgentView { status: "idle".to_string(), wants: true, ..AgentView::default() },
                ],
            },
            SessionView {
                host: "local".to_string(),
                name: "b".to_string(),
                up: false,
                agents: Vec::new(),
            },
        ];
        let c = v.counts();
        assert!(
            c.working == 1 && c.waiting == 1 && c.down == 1 && c.blocked == 0,
            "counts: {c:?}"
        );
        assert_eq!(v.severity(), "down");
        v.sessions[1].up = true;
        // one working + one waiting: working wins (busy fleet reads as busy)
        assert_eq!(v.severity(), "working");
        assert_eq!(v.summary(), "1 working · 1 waiting");
        // green only when every agent stopped and someone latched a finish
        v.sessions[0].agents[0].status = "done".to_string();
        v.sessions[0].agents[0].wants = true;
        assert_eq!(v.severity(), "waiting");
        // an idle agent that never latched is quiet, not waiting
        v.sessions[0].agents[1].wants = false;
        v.sessions[0].agents[0] = AgentView { status: "idle".to_string(), ..AgentView::default() };
        assert_eq!(v.severity(), "idle", "want idle for unlatched fleet");
        assert_eq!(v.summary(), "all quiet");
    }

    /// Folds a snapshot into the latch test's tracker through the engine's
    /// session map (Go reaches the tracker via a &Engine{sessions: ...}).
    fn latch_apply(e: &Engine, agents: Vec<herdr::Agent>) {
        e.sessions
            .lock()
            .unwrap()
            .get_mut("local/work")
            .expect("tracker installed")
            .apply(&snap(agents), now());
    }

    #[test]
    fn tracker_attention_latch() {
        let e = Engine::from_cfg(Config::default());
        *e.sessions.lock().unwrap() =
            HashMap::from([("local/work".to_string(), Tracker::new("local", "work"))]);
        let pane = "w1:p1";
        // baseline idle: resting, not waiting; a fresh tray must read gray
        latch_apply(&e, vec![agent("idle", pane, "t")]);
        assert_eq!(e.view().severity(), "idle", "startup with idle agents must not be waiting");
        // observed finish latches green
        latch_apply(&e, vec![agent("working", pane, "t")]);
        latch_apply(&e, vec![agent("idle", pane, "t")]);
        let v = e.view();
        assert!(
            v.severity() == "waiting" && v.sessions[0].agents[0].wants,
            "finish should latch waiting: {v:?}"
        );
        // idle → done reshuffle keeps the latch
        latch_apply(&e, vec![agent("done", pane, "t")]);
        assert_eq!(e.view().severity(), "waiting", "idle→done must keep the latch");
        // resuming work spends the attention
        latch_apply(&e, vec![agent("working", pane, "t")]);
        let v = e.view();
        assert!(
            v.severity() == "working" && !v.sessions[0].agents[0].wants,
            "work must clear the latch: {v:?}"
        );
        // a second finish latches again
        latch_apply(&e, vec![agent("done", pane, "t")]);
        assert_eq!(e.view().severity(), "waiting", "second finish should latch again");
        // the pane disappearing drops the latch with it
        latch_apply(&e, vec![]);
        let v = e.view();
        assert!(
            v.severity() == "idle" && v.sessions[0].agents.is_empty(),
            "agent_left must drop the latch: {v:?}"
        );
    }

    #[test]
    fn tracker_sticky_unknown() {
        let mut tr = Tracker::new("local", "work");
        let pane = "w1:p1";
        tr.apply(&snap(vec![agent("working", pane, "t")]), now());
        // a detection flap must not drop the agent from counts nor gray its row
        let (evs, _) = tr.apply(&snap(vec![agent("unknown", pane, "t")]), now());
        assert!(evs.is_empty(), "unknown flap emitted events: {:?}", kinds(&evs));
        assert_eq!(
            tr.agents.get(pane).expect("agent tracked").status,
            "working",
            "flap should keep last status"
        );
        // an unknown reading at baseline stays unknown (a real reading wins later)
        let mut tr2 = Tracker::new("local", "other");
        tr2.apply(&snap(vec![agent("unknown", "w9:p1", "t")]), now());
        tr2.apply(&snap(vec![agent("working", "w9:p1", "t")]), now());
        assert_eq!(
            tr2.agents.get("w9:p1").expect("agent tracked").status,
            "working",
            "baseline unknown must not be sticky"
        );
    }

    #[test]
    fn row_severity_cases() {
        let cases: &[(&str, bool, &str)] = &[
            ("blocked", false, "blocked"),
            ("blocked", true, "blocked"),
            ("working", false, "working"),
            ("idle", true, "waiting"),
            ("done", true, "waiting"),
            ("idle", false, "idle"),
            ("done", false, "idle"),
            ("unknown", false, "idle"),
            ("unknown", true, "idle"),
        ];
        for (status, wants, want) in cases {
            assert_eq!(
                row_severity(status, *wants),
                *want,
                "row_severity({status:?}, {wants})"
            );
        }
    }

    #[test]
    fn apply_change_detection() {
        let mut tr = Tracker::new("local", "work");
        let base = snap(vec![agent("working", "w1:p1", "Refactor auth")]);

        // baseline appears
        let (_, changed) = tr.apply(&base, now());
        assert!(changed, "baseline should mark changed");
        // identical snapshot: no change
        let (_, changed) = tr.apply(&base, now());
        assert!(!changed, "identical snapshot flagged as changed");
        // status move
        let flipped = snap(vec![agent("idle", "w1:p1", "Refactor auth")]);
        let (_, changed) = tr.apply(&flipped, now());
        assert!(changed, "status change not detected");
        // title-only refresh (view shows titles)
        let retitled = snap(vec![agent("idle", "w1:p1", "Fix login flow")]);
        let (_, changed) = tr.apply(&retitled, now());
        assert!(changed, "title change not detected");
        // spawn
        let grown = snap(vec![
            agent("idle", "w1:p1", "Fix login flow"),
            agent("working", "w2:p1", "Write docs"),
        ]);
        let (_, changed) = tr.apply(&grown, now());
        assert!(changed, "spawn not detected");
        // leave
        let (_, changed) = tr.apply(&retitled, now());
        assert!(changed, "leave not detected");
        // and quiet again
        let (_, changed) = tr.apply(&retitled, now());
        assert!(!changed, "stable snapshot flagged as changed");
    }

    #[test]
    fn down_requires_consecutive_failures() {
        let mut tr = Tracker::new("local", "work");
        tr.apply(&snap(vec![agent("idle", "w1:p1", "t")]), now());

        // One or two failed polls are noise (slow wsl.exe, transient timeout).
        for i in 0..(DOWN_AFTER - 1) {
            let (evs, changed) = tr.fail(now_plus(i as u64));
            assert!(
                evs.is_empty() && !changed,
                "failure {} flapped the session: {:?}",
                i + 1,
                kinds(&evs)
            );
            assert!(tr.up, "failure {} took the session down", i + 1);
        }
        // The third consecutive failure marks it down.
        let (evs, changed) = tr.fail(now_plus(DOWN_AFTER as u64 - 1));
        assert!(
            evs.len() == 1 && evs[0].kind == Kind::SessionDown && changed,
            "expected session_down on failure {DOWN_AFTER}, got {:?}",
            kinds(&evs)
        );
        // Further failures stay silent.
        let (evs, _) = tr.fail(now_plus(DOWN_AFTER as u64));
        assert!(evs.is_empty(), "duplicate session_down: {:?}", kinds(&evs));
        // Recovery resets the counter and announces the comeback.
        let (evs, changed) = tr.apply(&snap(vec![agent("idle", "w1:p1", "t")]), now());
        assert!(
            evs.len() == 1 && evs[0].kind == Kind::SessionUp && changed,
            "expected session_up on recovery, got {:?}",
            kinds(&evs)
        );
        assert!(tr.up && tr.fails == 0, "recovery state wrong: up={} fails={}", tr.up, tr.fails);
    }

    // ---- engine_test.go helpers ----

    /// fakeBackend feeds canned snapshots per session (engine_test.go).
    /// Shared as Arc so the test can set() snapshots while the engine polls
    /// through the Box<dyn Backend> it owns.
    struct FakeBackend {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        sessions: Vec<herdr::Session>,
        snaps: HashMap<String, herdr::Snapshot>,
        errs: HashMap<String, String>,
    }

    impl FakeBackend {
        fn new() -> FakeBackend {
            FakeBackend { state: Mutex::new(FakeState::default()) }
        }

        /// fakeBackend.set.
        fn set(&self, name: &str, agents: Vec<herdr::Agent>) {
            let mut st = self.state.lock().unwrap();
            if !st.sessions.iter().any(|s| s.name == name) {
                st.sessions.push(herdr::Session {
                    host: "local".to_string(),
                    name: name.to_string(),
                    ref_: name.to_string(),
                    home: String::new(),
                });
            }
            st.snaps.insert(name.to_string(), herdr::Snapshot { agents });
        }
    }

    impl herdr::Backend for Arc<FakeBackend> {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn rescan(&self) -> Duration {
            Duration::ZERO
        }

        fn discover(&self, _timeout: Duration) -> anyhow::Result<Vec<herdr::Session>> {
            Ok(self.state.lock().unwrap().sessions.clone())
        }

        fn fetch(&self, session: &herdr::Session, _timeout: Duration) -> anyhow::Result<herdr::Snapshot> {
            let st = self.state.lock().unwrap();
            if let Some(msg) = st.errs.get(&session.name) {
                return Err(anyhow::anyhow!("{}", msg.clone()));
            }
            // PORT-NOTE: Go's fake returns (nil, nil) for an unset snapshot,
            // which would crash tracker.apply on a nil deref; tests always
            // set() a snapshot first, so default() stands in here.
            Ok(st.snaps.get(&session.name).cloned().unwrap_or_default())
        }
    }

    /// recorder: collects the engine's events safely (engine_test.go).
    type Recorder = Arc<Mutex<Vec<Event>>>;

    /// recorder.attach: drains the event stream into a shared vec.
    fn attach_recorder(e: &Arc<Engine>) -> Recorder {
        let evs: Recorder = Arc::new(Mutex::new(Vec::new()));
        let evs2 = Arc::clone(&evs);
        let e2 = Arc::clone(e);
        std::thread::spawn(move || {
            loop {
                match e2.events().lock().expect("events lock").recv() {
                    Ok(ev) => evs2.lock().unwrap().push(ev),
                    Err(_) => break, // senders gone: Go's range over the channel ends
                }
            }
        });
        evs
    }

    /// newTestEngine: kinds from parse_set (event.Attention default), one
    /// injected fake backend, and a pre-run config tweak (Go mutates e.cfg
    /// after New, before Run).
    fn new_test_engine(
        kinds_list: &[Kind],
        tweak: impl FnOnce(&mut Config),
    ) -> (Arc<Engine>, Arc<FakeBackend>) {
        let fb = Arc::new(FakeBackend::new());
        let names = event::names(kinds_list);
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ks = event::parse_set(&name_refs).expect("test kinds parse");
        let mut cfg = Config::default();
        cfg.kinds = ks.into_iter().collect();
        tweak(&mut cfg);
        let e = Arc::new(Engine::with_backends(cfg, vec![Box::new(fb.clone())]));
        (e, fb)
    }

    /// Spawns Engine::run on its own thread (Go: go e.Run(ctx)); returns
    /// the join handle and the stop flag standing in for the context.
    fn run_engine(e: &Arc<Engine>) -> (std::thread::JoinHandle<()>, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let e2 = Arc::clone(e);
        let handle = std::thread::spawn(move || e2.run(stop2));
        (handle, stop)
    }

    /// waitFor polls cond until true or the deadline passes.
    fn wait_for(d: Duration, cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn engine_emits_attention_transition() {
        let (e, fb) = new_test_engine(&Kind::ATTENTION, |_| {});
        let working = herdr::Agent {
            pane_id: "w1:p1".to_string(),
            name: "claude".to_string(),
            status: "working".to_string(),
            title: "Refactor auth".to_string(),
            project: "api".to_string(),
            ..herdr::Agent::default()
        };
        let idle = herdr::Agent {
            pane_id: "w1:p1".to_string(),
            name: "claude".to_string(),
            status: "idle".to_string(),
            title: "Refactor auth".to_string(),
            project: "api".to_string(),
            ..herdr::Agent::default()
        };
        fb.set("work", vec![working]);

        let (run_h, stop) = run_engine(&e);
        let events = attach_recorder(&e);

        assert!(
            wait_for(Duration::from_secs(2), || e.view().sessions.len() == 1),
            "baseline never appeared in view"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "baseline emitted events: {:?}",
            events.lock().unwrap()
        );

        fb.set("work", vec![idle]);
        assert!(
            wait_for(Duration::from_secs(2), || events.lock().unwrap().len() == 1),
            "expected 1 event, got {:?}",
            events.lock().unwrap()
        );
        let ev = &events.lock().unwrap()[0];
        assert!(
            ev.kind == Kind::AgentIdle
                && ev.agent == "claude"
                && ev.from == "working"
                && ev.to == "idle",
            "event = {ev:?}"
        );
        let v = e.view();
        assert!(
            v.severity() == "waiting"
                && v.sessions.len() == 1
                && v.sessions[0].agents[0].status == "idle",
            "view: {v:?}"
        );

        stop.store(true, Ordering::Relaxed);
        run_h.join().expect("engine run thread");
    }

    #[test]
    fn engine_kinds_filter() {
        let (e, fb) = new_test_engine(&Kind::ATTENTION, |_| {});
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "idle".to_string(),
                ..herdr::Agent::default()
            }],
        );

        let (run_h, stop) = run_engine(&e);
        let events = attach_recorder(&e);

        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "working".to_string(),
                ..herdr::Agent::default()
            }],
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            events.lock().unwrap().is_empty(),
            "non-attention event leaked: {:?}",
            events.lock().unwrap()
        );
        assert_eq!(e.view().severity(), "working", "view should still update");

        stop.store(true, Ordering::Relaxed);
        run_h.join().expect("engine run thread");
    }

    #[test]
    fn engine_cooldown() {
        let (e, fb) = new_test_engine(&Kind::ATTENTION, |cfg| {
            cfg.cooldown = Duration::from_secs(3600); // Go: e.cfg.Cooldown = time.Hour
        });
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "working".to_string(),
                ..herdr::Agent::default()
            }],
        );

        let (run_h, stop) = run_engine(&e);
        let events = attach_recorder(&e);

        assert!(
            wait_for(Duration::from_secs(2), || e.view().sessions.len() == 1),
            "baseline never appeared"
        );
        // Two rapid working→idle cycles: the second must be suppressed.
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "idle".to_string(),
                ..herdr::Agent::default()
            }],
        );
        assert!(
            wait_for(Duration::from_secs(2), || events.lock().unwrap().len() == 1),
            "first event missing"
        );
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "working".to_string(),
                ..herdr::Agent::default()
            }],
        );
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "idle".to_string(),
                ..herdr::Agent::default()
            }],
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "cooldown did not suppress duplicate: {} events",
            events.lock().unwrap().len()
        );

        stop.store(true, Ordering::Relaxed);
        run_h.join().expect("engine run thread");
    }

    #[test]
    fn engine_session_filter() {
        let (e, fb) = new_test_engine(&Kind::ATTENTION, |cfg| {
            cfg.include = vec!["work*".to_string()];
            cfg.exclude = vec!["work-scratch".to_string()];
        });
        fb.set("work", vec![]);
        fb.set("work-scratch", vec![]);
        fb.set("personal", vec![]);

        let (run_h, stop) = run_engine(&e);

        assert!(
            wait_for(Duration::from_secs(2), || {
                let v = e.view();
                v.sessions.len() == 1 && v.sessions[0].name == "work"
            }),
            "filter mismatch: {:?}",
            e.view().sessions
        );

        stop.store(true, Ordering::Relaxed);
        run_h.join().expect("engine run thread");
    }

    #[test]
    fn state_change_quiet_when_stable() {
        let (e, fb) = new_test_engine(&Kind::ATTENTION, |_| {});
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "idle".to_string(),
                title: "Refactor auth".to_string(),
                ..herdr::Agent::default()
            }],
        );

        let (run_h, stop) = run_engine(&e);

        assert!(
            wait_for(Duration::from_secs(2), || e.view().sessions.len() == 1),
            "baseline never appeared"
        );
        // drain any pending signal from the baseline (Go drains one slot)
        let _ = e.state_change().lock().unwrap().try_recv();
        // stable polls must not signal
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            assert!(
                e.state_change().lock().unwrap().try_recv().is_err(),
                "state signal while snapshot unchanged"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // a real transition must signal
        fb.set(
            "work",
            vec![herdr::Agent {
                pane_id: "w1:p1".to_string(),
                name: "claude".to_string(),
                status: "working".to_string(),
                title: "Refactor auth".to_string(),
                ..herdr::Agent::default()
            }],
        );
        assert!(
            wait_for(Duration::from_secs(2), || {
                e.state_change().lock().unwrap().try_recv().is_ok()
            }),
            "no state signal after a real transition"
        );

        stop.store(true, Ordering::Relaxed);
        run_h.join().expect("engine run thread");
    }
}
