//! The normalized event model produced by sources (herdr today) and consumed
//! by sinks (tray, popups, webhooks, ...). Exact port of
//! `internal/event/event.go`, including the JSON field names.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Kind

/// The event types agent-notify can emit (`event.Kind`).
///
/// Serialized as the exact Go strings: `"agent_idle"`, ..., and
/// `"update_available"` (note: `UpdateAvail` does not follow snake_case).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Attention events: an agent stopped working and probably wants a human.
    /// working → idle.
    AgentIdle,
    /// working → done.
    AgentDone,
    /// anything → blocked.
    AgentBlocked,

    /// Non-attention kinds, opt-in via the "events" config key.
    /// anything → working.
    AgentWorking,
    /// new agent pane appeared.
    AgentSpawned,
    /// agent pane disappeared.
    AgentLeft,
    /// session stopped responding.
    SessionDown,
    /// session responded again.
    SessionUp,

    /// Synthesized by `agent-notify test` and the tray menu.
    Test,
    /// Synthesized by the background update check.
    #[serde(rename = "update_available")]
    UpdateAvail,
    /// Synthesized by the tray's first-run setup nudge.
    Setup,
}

impl Kind {
    /// Every kind in `event.All` order.
    pub const ALL: [Kind; 11] = [
        Kind::AgentIdle,
        Kind::AgentDone,
        Kind::AgentBlocked,
        Kind::AgentWorking,
        Kind::AgentSpawned,
        Kind::AgentLeft,
        Kind::SessionDown,
        Kind::SessionUp,
        Kind::Test,
        Kind::UpdateAvail,
        Kind::Setup,
    ];

    /// The default event set (`event.Attention`): the transitions where an
    /// agent stopped and wants a human.
    pub const ATTENTION: [Kind; 3] = [Kind::AgentIdle, Kind::AgentDone, Kind::AgentBlocked];

    /// The Go constant string for the kind (`string(k)`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::AgentIdle => "agent_idle",
            Kind::AgentDone => "agent_done",
            Kind::AgentBlocked => "agent_blocked",
            Kind::AgentWorking => "agent_working",
            Kind::AgentSpawned => "agent_spawned",
            Kind::AgentLeft => "agent_left",
            Kind::SessionDown => "session_down",
            Kind::SessionUp => "session_up",
            Kind::Test => "test",
            Kind::UpdateAvail => "update_available",
            Kind::Setup => "setup",
        }
    }

    /// Short human phrase for the kind, usable in templates (`Kind.Verb`).
    /// Unknown kinds fall back to their name; with an exhaustive enum every
    /// kind is in the table, so this always hits a phrase.
    pub fn verb(&self) -> &'static str {
        match self {
            Kind::AgentIdle => "went idle",
            Kind::AgentDone => "finished",
            Kind::AgentBlocked => "is blocked",
            Kind::AgentWorking => "is working",
            Kind::AgentSpawned => "joined",
            Kind::AgentLeft => "left",
            Kind::SessionDown => "went offline",
            Kind::SessionUp => "is back online",
            Kind::Test => "test notification",
            Kind::UpdateAvail => "update available",
            Kind::Setup => "needs setup",
        }
    }
}

/// Maps kinds back to their string form (`event.Names`).
pub fn names(kinds: &[Kind]) -> Vec<String> {
    kinds.iter().map(|k| k.as_str().to_string()).collect()
}

// ---------------------------------------------------------------------------
// Event

/// UTC timestamp: seconds since the Unix epoch plus nanoseconds. Stands in
/// for Go's `time.Time`, which marshals to JSON as an RFC 3339 string
/// (`2026-11-11T20:00:00.123456789Z`); the serde impl here produces and
/// accepts exactly that shape, trimming trailing fraction zeros like Go's
/// RFC3339Nano.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    pub secs: u64,
    pub nanos: u32,
}

impl Time {
    pub const ZERO: Time = Time { secs: 0, nanos: 0 };

    /// The current wall clock, UTC.
    pub fn now() -> Time {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Time { secs: d.as_secs(), nanos: d.subsec_nanos() }
    }

    /// RFC 3339 / RFC3339Nano formatting: `YYYY-MM-DDTHH:MM:SS[.frac]Z`,
    /// the fraction's trailing zeros trimmed, no fraction when zero.
    pub fn to_rfc3339(&self) -> String {
        let (y, mo, d) = civil_from_days((self.secs / 86_400) as i64);
        let s = self.secs % 86_400;
        let (h, mi, se) = (s / 3600, (s % 3600) / 60, s % 60);
        let mut out = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}");
        if self.nanos > 0 {
            let mut frac = format!("{:09}", self.nanos);
            while frac.ends_with('0') {
                frac.pop();
            }
            let _ = write!(out, ".{frac}");
        }
        out.push('Z');
        out
    }

    /// Parses the RFC 3339 shapes Go's time.Time unmarshals: `Z` or a
    /// numeric offset, with an optional fraction. This port accepts a `T`
    /// or space separator and offsets at minute granularity.
    pub fn parse_rfc3339(s: &str) -> Option<Time> {
        let b = s.as_bytes();
        if b.len() < 20 {
            return None;
        }
        let num = |r: std::ops::Range<usize>| -> Option<i64> {
            let t = s.get(r)?;
            if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            t.parse().ok()
        };
        let year = num(0..4)?;
        if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
            return None;
        }
        let month = num(5..7)? as u64;
        let day = num(8..10)? as u64;
        if b[13] != b':' || b[16] != b':' {
            return None;
        }
        let hour = num(11..13)? as u64;
        let min = num(14..16)? as u64;
        let sec = num(17..19)? as u64;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
            return None;
        }
        let mut i = 19;
        let mut nanos: u32 = 0;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let start = i;
            let mut scale: u32 = 100_000_000;
            while i < b.len() && b[i].is_ascii_digit() {
                if scale > 0 {
                    nanos += (b[i] - b'0') as u32 * scale;
                    scale /= 10;
                }
                i += 1;
            }
            if i == start {
                return None;
            }
        }
        // Zone: Z, or ±hh[:mm]
        let zone_secs: i64 = match b.get(i) {
            Some(b'Z') | Some(b'z') if i + 1 == b.len() => 0,
            Some(sign @ (b'+' | b'-')) => {
                let rest = s.get(i + 1..)?;
                let (zh, zm) = match rest.len() {
                    2 => (rest, "00"),
                    5 if rest.as_bytes()[2] == b':' => (&rest[0..2], &rest[3..5]),
                    _ => return None,
                };
                if !zh.bytes().all(|c| c.is_ascii_digit()) || !zm.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let off: i64 = zh.parse::<i64>().ok()? * 3600 + zm.parse::<i64>().ok()? * 60;
                if *sign == b'-' {
                    -off
                } else {
                    off
                }
            }
            _ => return None,
        };
        let days = days_from_civil(year, month as u32, day as u32);
        let secs = days * 86_400 + (hour * 3600 + min * 60 + sec) as i64 - zone_secs;
        if secs < 0 {
            return None;
        }
        Some(Time { secs: secs as u64, nanos })
    }
}

impl std::fmt::Display for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl Default for Time {
    fn default() -> Self {
        Time::ZERO
    }
}

impl serde::Serialize for Time {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> serde::Deserialize<'de> for Time {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Time::parse_rfc3339(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid RFC3339 time {s:?}")))
    }
}

/// Days-since-epoch → (year, month, day). Howard Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since epoch. Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m as u64 - 3 } else { m as u64 + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// One observed state transition on a monitored session (`event.Event`).
/// The serde attributes reproduce Go's `omitempty` shapes: empty strings and
/// `focused: false` are left out of the JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub kind: Kind,
    pub time: Time,
    /// emitting source, e.g. "herdr".
    pub source: String,
    /// backend: local | windows | wsl.
    pub host: String,
    /// herdr session name.
    pub session: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl Event {
    /// The human phrase for the event kind (`Event.Verb`).
    pub fn verb(&self) -> &'static str {
        self.kind.verb()
    }
}

// ---------------------------------------------------------------------------
// ParseSet

/// Reports an event name that is not a known kind (`event.UnknownKindError`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub struct UnknownKindError {
    pub kind: String,
    pub all: Vec<String>,
}

impl std::fmt::Display for UnknownKindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown event kind {} (known: {}, attention, all, none)",
            self.kind,
            self.all.join(", ")
        )
    }
}

/// Expands a configured event list into concrete kinds (`event.ParseSet`).
/// Accepts the pseudo-sets "attention" (default), "all", and "none" plus any
/// kind names. A nil or empty list means the default attention set.
pub fn parse_set(entry: &[&str]) -> Result<Vec<Kind>, UnknownKindError> {
    if entry.is_empty() {
        return Ok(Kind::ATTENTION.to_vec());
    }
    let mut out: Vec<Kind> = Vec::new();
    let add = |ks: &[Kind], out: &mut Vec<Kind>| {
        for k in ks {
            if !out.contains(k) {
                out.push(*k);
            }
        }
    };
    for n in entry {
        match *n {
            "attention" => add(&Kind::ATTENTION, &mut out),
            "all" => add(&Kind::ALL, &mut out),
            "none" => {} // no kinds
            _ => {
                let known = Kind::ALL.iter().any(|k| k.as_str() == *n);
                if !known {
                    return Err(UnknownKindError { kind: (*n).to_string(), all: names(&Kind::ALL) });
                }
                let k = Kind::ALL.iter().find(|k| k.as_str() == *n).copied().unwrap();
                add(&[k], &mut out);
            }
        }
    }
    Ok(out)
}
