//! Loads and validates agent-notify configuration. Port of
//! `internal/config/config.go` and `duration.go`.
//!
//! Precedence for the config file location: `--config` flag, then the
//! `AGENT_NOTIFY_CONFIG` environment variable, then
//! `<os.UserConfigDir>()/agent-notify/config.toml`. A missing file means "use
//! the defaults"; agent-notify works with zero configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::event::{self, Kind};

/// The fully annotated config written by `agent-notify init`
/// (`//go:embed example.toml` in the Go package).
pub const EXAMPLE: &str = include_str!("example.toml");

// Defaults for render templates.
pub const DEFAULT_TITLE_TEMPLATE: &str =
    "{{if .Agent}}{{.Agent}}{{else}}{{.Session}}{{end}} {{.Verb}}";
pub const DEFAULT_BODY_TEMPLATE: &str =
    "{{if .Title}}{{.Title}}{{else if .Project}}{{.Project}}{{else}}{{.Host}}/{{.Session}}{{end}}";

// ---------------------------------------------------------------------------
// Duration (duration.go)

/// Wraps `std::time::Duration` so TOML configs can write it as a string
/// ("1s", "250ms", "2h45m" — Go's time.ParseDuration grammar). An empty
/// value keeps the zero Duration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration(StdDuration);

impl Duration {
    pub const ZERO: Duration = Duration(StdDuration::ZERO);

    /// Terse accessor for the wrapped duration (`Duration.D`).
    pub fn d(&self) -> StdDuration {
        self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

impl From<StdDuration> for Duration {
    fn from(d: StdDuration) -> Self {
        Duration(d)
    }
}

/// Parses a Go `time.ParseDuration` string: an optional sign, one or more
/// decimal numbers each with a unit (ns, us/µs/μs, ms, s, m, h), e.g.
/// "300ms", "-1.5h", "2h45m". Port of the Go grammar; errors quote the
/// original input like Go's `time: invalid duration "..."`. Negative values
/// parse but saturate at zero: `std::time::Duration` cannot be negative, and
/// agent-notify's durations (cooldowns, polls) never use them meaningfully.
pub fn parse_duration(orig: &str) -> Result<StdDuration, String> {
    let s = orig.trim();
    let bad = || format!("time: invalid duration {orig:?}");
    let missing_unit = || format!("time: missing unit in duration {orig:?}");
    if s.is_empty() {
        return Err(bad());
    }
    let mut rest = s;
    let neg = rest.starts_with('-');
    rest = rest.trim_start_matches(['+', '-']);
    if rest == "0" {
        return Ok(StdDuration::ZERO);
    }
    if rest.is_empty() {
        return Err(bad());
    }

    // (unit text, nanoseconds for the unit). Longest match wins, so "ms"
    // beats "m" and "µs"/"μs" (U+00B5 / U+03BC) both mean microseconds.
    const UNITS: [(&str, u128); 8] = [
        ("ns", 1),
        ("us", 1_000),
        ("µs", 1_000),
        ("μs", 1_000),
        ("ms", 1_000_000),
        ("s", 1_000_000_000),
        ("m", 60_000_000_000),
        ("h", 3_600_000_000_000),
    ];

    let mut total: u128 = 0;
    while !rest.is_empty() {
        // integer part: at least one digit unless a fraction follows.
        let int_len = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if int_len == 0 && !rest.starts_with('.') {
            return Err(bad());
        }
        let (int_part, after_int) = rest.split_at(int_len);

        // optional fraction: '.' plus at least one digit.
        let mut frac_part = "";
        let mut after_int = after_int;
        if let Some(t) = after_int.strip_prefix('.') {
            let frac_len = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if frac_len == 0 {
                return Err(bad());
            }
            let (f, t2) = t.split_at(frac_len);
            frac_part = f;
            after_int = t2;
        }

        // unit: the longest grammar unit that prefixes what remains.
        let mut unit: Option<(&str, u128)> = None;
        for (u, ns) in UNITS {
            if after_int.starts_with(u)
                && unit.map(|(m, _)| u.len() > m.len()).unwrap_or(true)
            {
                unit = Some((u, ns));
            }
        }
        let (u, ns) = unit.ok_or_else(missing_unit)?;

        // piece = int × ns + frac × ns / 10^len(frac), exact.
        let int_v: u128 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().map_err(|_| bad())?
        };
        let mut piece = int_v.checked_mul(ns).ok_or_else(bad)?;
        if !frac_part.is_empty() {
            let frac_v: u128 = frac_part.parse().map_err(|_| bad())?;
            let scale = 10u128
                .checked_pow(frac_part.len() as u32)
                .ok_or_else(bad)?;
            // only the fraction's contribution is scaled down; the integer
            // part is already in nanoseconds
            let frac_ns = frac_v.checked_mul(ns).ok_or_else(bad)?.checked_div(scale).ok_or_else(bad)?;
            piece = piece.checked_add(frac_ns).ok_or_else(bad)?;
        }
        total = total.checked_add(piece).ok_or_else(bad)?;
        rest = &after_int[u.len()..];
    }
    if neg {
        return Ok(StdDuration::ZERO);
    }
    if total > u64::MAX as u128 {
        return Err(bad());
    }
    Ok(StdDuration::from_nanos(total as u64))
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(self.0))
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Ok(Duration::ZERO);
        }
        parse_duration(&s).map(Duration).map_err(serde::de::Error::custom)
    }
}

/// Renders a duration in the smallest exact unit ("500ms", "90s", "2h").
/// The Go side never marshals durations; this exists for the Rust side's
/// own serialization only.
pub fn format_duration(d: StdDuration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".to_string();
    }
    const UNITS: [(&str, u128); 6] = [
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
        ("ns", 1),
    ];
    for (u, div) in UNITS {
        if nanos % div == 0 {
            return format!("{}{u}", nanos / div);
        }
    }
    unreachable!("ns divisor is 1, so the modulo is always 0")
}

// ---------------------------------------------------------------------------
// Config structs

/// The root configuration (`config.Config`). TOML key names match the Go
/// `toml:` tags exactly.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// event kinds that trigger sinks; default "attention".
    #[serde(default)]
    pub events: Vec<String>,
    /// suppress identical (session,pane,kind) events within this window.
    #[serde(default)]
    pub cooldown: Duration,
    #[serde(default)]
    pub render: Render,
    #[serde(default)]
    pub herdr: Herdr,
    #[serde(default)]
    pub sink: Vec<Sink>,
}

/// Customizes how event titles and bodies are produced (`config.Render`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Render {
    /// Go text/template over event.Event.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// Configures the herdr source and its backends (`config.Herdr`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Herdr {
    /// session-name globs to keep; empty = all.
    #[serde(default)]
    pub include: Vec<String>,
    /// session-name globs to drop.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub local: Local,
    #[serde(default)]
    pub windows: Windows,
    #[serde(default)]
    pub wsl: Wsl,
}

/// The native-socket backend (Linux, WSL, macOS) (`config.Local`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Local {
    /// auto | on | off.
    #[serde(default)]
    pub enabled: String,
    /// seconds between snapshots.
    #[serde(default)]
    pub poll: Duration,
    /// per-snapshot command timeout.
    #[serde(default)]
    pub timeout: Duration,
    /// override ~/.config/herdr.
    #[serde(default)]
    pub socket_dir: String,
    /// herdr binary name/path.
    #[serde(default)]
    pub binary: String,
}

/// The herdr.exe backend: Windows-native sessions, or Windows sessions
/// polled from WSL via interop (`config.Windows`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Windows {
    /// auto | on | off.
    #[serde(default)]
    pub enabled: String,
    /// extra named sessions besides the default one.
    #[serde(default)]
    pub sessions: Vec<String>,
    #[serde(default)]
    pub poll: Duration,
    #[serde(default)]
    pub timeout: Duration,
    /// "auto" picks herdr/herdr.exe from PATH.
    #[serde(default)]
    pub binary: String,
}

/// Sessions inside a WSL distro, polled from a Windows-native process via
/// wsl.exe interop (`config.WSL`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Wsl {
    /// auto | on | off.
    #[serde(default)]
    pub enabled: String,
    /// WSL distro name; empty = default.
    #[serde(default)]
    pub distro: String,
    /// restrict auto-discovery to these session names.
    #[serde(default)]
    pub sessions: Vec<String>,
    /// extra PATH entries when invoking herdr inside WSL.
    #[serde(default)]
    pub extra_path: Vec<String>,
    #[serde(default)]
    pub poll: Duration,
    #[serde(default)]
    pub timeout: Duration,
    /// how often to re-discover WSL sessions.
    #[serde(default)]
    pub rescan: Duration,
}

/// One notifier (`config.Sink`). `sink_type` selects the flavor; the
/// remaining fields are shared across flavors and documented per type in
/// the example config. Multiple sinks of the same type are allowed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Sink {
    /// tray | popup | bell | command | webhook | log. Serialized as `type`.
    #[serde(rename = "type")]
    pub sink_type: String,
    #[serde(default)]
    pub disabled: bool,

    // tray & popup
    /// tray: fire attention popups; default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup: Option<bool>,
    /// popup: Windows toast AppUserModelID.
    #[serde(default)]
    pub app_id: String,
    /// popup: override powershell.exe / notify-send path.
    #[serde(default)]
    pub binary: String,
    /// popup: notify-send expiration.
    #[serde(default)]
    pub expire: Duration,
    /// popup: severity logo in toasts; default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,

    // command
    /// argv template per element.
    #[serde(default)]
    pub command: Vec<String>,
    /// run a single command string via the OS shell.
    #[serde(default)]
    pub shell: bool,

    // webhook
    #[serde(default)]
    pub url: String,
    /// default POST.
    #[serde(default)]
    pub method: String,
    /// extra HTTP headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// used with body_template.
    #[serde(default)]
    pub content_type: String,
    /// custom body; default is the event as JSON.
    #[serde(default)]
    pub body_template: String,
    /// command & webhook; default 10s.
    #[serde(default)]
    pub timeout: Duration,
}

/// Sink types accepted in [`Sink::sink_type`] (`config.SinkTypes`).
pub const SINK_TYPES: [&str; 6] = ["tray", "popup", "bell", "command", "webhook", "log"];

impl Config {
    /// The built-in configuration used when no file exists (`config.Default`).
    pub fn default_config() -> Config {
        Config {
            events: event::names(&Kind::ATTENTION),
            cooldown: Duration(StdDuration::from_millis(500)),
            render: Render::default(),
            herdr: Herdr {
                include: Vec::new(),
                exclude: Vec::new(),
                local: Local {
                    enabled: "auto".into(),
                    poll: Duration(StdDuration::from_secs(1)),
                    timeout: Duration(StdDuration::from_secs(3)),
                    socket_dir: String::new(),
                    binary: "herdr".into(),
                },
                windows: Windows {
                    enabled: "auto".into(),
                    sessions: Vec::new(),
                    poll: Duration(StdDuration::from_secs(2)),
                    timeout: Duration(StdDuration::from_secs(6)),
                    binary: "auto".into(),
                },
                wsl: Wsl {
                    enabled: "auto".into(),
                    distro: String::new(),
                    sessions: Vec::new(),
                    extra_path: Vec::new(),
                    poll: Duration(StdDuration::from_secs(2)),
                    timeout: Duration(StdDuration::from_secs(10)),
                    rescan: Duration(StdDuration::from_secs(10)),
                },
            },
            sink: vec![Sink {
                sink_type: "tray".into(),
                popup: Some(true),
                ..Sink::default()
            }],
        }
    }

    /// Reads `path` over the defaults and validates the result
    /// (`config.Load`). Warnings cover keys that decoded into nothing
    /// (usually typos).
    pub fn load(path: &Path) -> anyhow::Result<(Config, Vec<String>)> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read config: {e}"))?;
        let table: toml::Table = toml::from_str(&data)
            .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
        let warnings = undecoded_keys(&table);
        // All config fields carry #[serde(default)], so decoding straight
        // into Config layers the file over [`Config::default_config`]'s
        // values exactly like Go's decode over `Default()`.
        let cfg: Config = table.try_into().map_err(|e| {
            anyhow::anyhow!("parse config {}: {e}", path.display())
        })?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("invalid config {}: {e}", path.display()))?;
        Ok((cfg, warnings))
    }

    /// Checks the merged configuration for structural errors
    /// (`Config.Validate`).
    pub fn validate(&self) -> Result<(), String> {
        let names: Vec<&str> = self.events.iter().map(|s| s.as_str()).collect();
        event::parse_set(&names).map_err(|e| e.to_string())?;
        for (name, val) in [
            ("herdr.local.enabled", &self.herdr.local.enabled),
            ("herdr.windows.enabled", &self.herdr.windows.enabled),
            ("herdr.wsl.enabled", &self.herdr.wsl.enabled),
        ] {
            if !valid_enabled(val) {
                return Err(format!("{name}: must be auto, on or off (got {val:?})"));
            }
        }
        for (i, s) in self.sink.iter().enumerate() {
            if !SINK_TYPES.contains(&s.sink_type.as_str()) {
                return Err(format!(
                    "sink[{i}]: unknown type {:?} (known: {})",
                    s.sink_type,
                    SINK_TYPES.join(", ")
                ));
            }
            match s.sink_type.as_str() {
                "webhook" if s.url.is_empty() => {
                    return Err(format!("sink[{i}] webhook: url is required"));
                }
                "command" => {
                    if s.command.is_empty() {
                        return Err(format!("sink[{i}] command: command is required"));
                    }
                    if s.shell && s.command.len() != 1 {
                        return Err(format!(
                            "sink[{i}] command: shell = true needs exactly one command string"
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Resolves where `init` should write: explicit flag, environment, then the
/// OS user config dir (`config.Path`). Returns `None` when nothing can be
/// resolved.
pub fn path(flag_value: &str) -> Option<PathBuf> {
    if !flag_value.is_empty() {
        return Some(PathBuf::from(flag_value));
    }
    if let Ok(env) = std::env::var("AGENT_NOTIFY_CONFIG") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    dirs::config_dir().map(|d| d.join("agent-notify").join("config.toml"))
}

/// The WSL mount point of the Windows drive; a parameter below so tests can
/// point it elsewhere (Go: package var `mntC`).
const MNT_C: &str = "/mnt/c";

/// Picks the config to LOAD: explicit flag, environment, the OS default, and
/// on WSL the Windows-side %APPDATA% config when it exists, so one file can
/// drive both the Windows tray and a WSL daemon (`config.Resolve`). The
/// second return says which rule matched ("flag", "env", "default",
/// "windows-shared", "").
pub fn resolve(flag_value: &str) -> (Option<PathBuf>, &'static str) {
    if !flag_value.is_empty() {
        return (Some(PathBuf::from(flag_value)), "flag");
    }
    if let Ok(env) = std::env::var("AGENT_NOTIFY_CONFIG") {
        if !env.is_empty() {
            return (Some(PathBuf::from(env)), "env");
        }
    }
    let def = path("");
    if let Some(def) = &def {
        if file_exists(def) {
            return (Some(def.clone()), "default");
        }
    }
    if cfg!(target_os = "linux") {
        if let Some(p) = windows_shared_config() {
            return (Some(p), "windows-shared");
        }
    }
    (def, "default") // exists or not: the normal location
}

/// Finds a Windows-side config under the WSL mount point, if any
/// (`config.windowsSharedConfig`). Only meaningful on Linux/WSL.
pub fn windows_shared_config() -> Option<PathBuf> {
    windows_shared_config_at(MNT_C)
}

pub(crate) fn windows_shared_config_at(mnt: &str) -> Option<PathBuf> {
    let users = Path::new(mnt).join("Users");
    let entries = std::fs::read_dir(&users).ok()?;
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        let p = dir
            .join("AppData")
            .join("Roaming")
            .join("agent-notify")
            .join("config.toml");
        if file_exists(&p) {
            return Some(p);
        }
    }
    None
}

pub(crate) fn file_exists(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(md) => !md.is_dir(),
        Err(_) => false,
    }
}

fn valid_enabled(v: &str) -> bool {
    matches!(v, "" | "auto" | "on" | "off")
}

// ---------------------------------------------------------------------------
// Unknown-key warnings (Go's md.Undecoded())

/// Walks a decoded TOML table and reports keys that do not map onto any
/// config field, at any depth — the port of `md.Undecoded()` warnings. The
/// schema below is the hand-written mirror of the config structs; keep both
/// in sync.
fn undecoded_keys(table: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    walk(table, "", CONFIG_SCHEMA, &mut out);
    out
}

/// Copies cheaply (only static references), so `walk` passes sub-schemas
/// by value.
#[derive(Clone, Copy)]
enum Schema {
    Leaf,
    Map(&'static [(&'static str, Schema)]),
    ArrayOf(&'static Schema),
}

const CONFIG_SCHEMA: Schema = Schema::Map(&[
    ("events", Schema::Leaf),
    ("cooldown", Schema::Leaf),
    (
        "render",
        Schema::Map(&[("title", Schema::Leaf), ("body", Schema::Leaf)]),
    ),
    (
        "herdr",
        Schema::Map(&[
            ("include", Schema::Leaf),
            ("exclude", Schema::Leaf),
            (
                "local",
                Schema::Map(&[
                    ("enabled", Schema::Leaf),
                    ("poll", Schema::Leaf),
                    ("timeout", Schema::Leaf),
                    ("socket_dir", Schema::Leaf),
                    ("binary", Schema::Leaf),
                ]),
            ),
            (
                "windows",
                Schema::Map(&[
                    ("enabled", Schema::Leaf),
                    ("sessions", Schema::Leaf),
                    ("poll", Schema::Leaf),
                    ("timeout", Schema::Leaf),
                    ("binary", Schema::Leaf),
                ]),
            ),
            (
                "wsl",
                Schema::Map(&[
                    ("enabled", Schema::Leaf),
                    ("distro", Schema::Leaf),
                    ("sessions", Schema::Leaf),
                    ("extra_path", Schema::Leaf),
                    ("poll", Schema::Leaf),
                    ("timeout", Schema::Leaf),
                    ("rescan", Schema::Leaf),
                ]),
            ),
        ]),
    ),
    ("sink", Schema::ArrayOf(&Schema::Map(&SINK_SCHEMA))),
]);

const SINK_SCHEMA: [(&str, Schema); 15] = [
    ("type", Schema::Leaf),
    ("disabled", Schema::Leaf),
    ("popup", Schema::Leaf),
    ("app_id", Schema::Leaf),
    ("binary", Schema::Leaf),
    ("expire", Schema::Leaf),
    ("image", Schema::Leaf),
    ("command", Schema::Leaf),
    ("shell", Schema::Leaf),
    ("url", Schema::Leaf),
    ("method", Schema::Leaf),
    ("headers", Schema::Leaf),
    ("content_type", Schema::Leaf),
    ("body_template", Schema::Leaf),
    ("timeout", Schema::Leaf),
];

fn walk(table: &toml::Table, prefix: &str, schema: Schema, out: &mut Vec<String>) {
    let Schema::Map(children) = schema else {
        return;
    };
    for (key, val) in table {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let Some((_, child)) = children.iter().find(|(k, _)| k == key) else {
            out.push(full);
            continue;
        };
        match *child {
            Schema::Leaf => {}
            Schema::Map(inner) => {
                if let toml::Value::Table(t) = val {
                    walk(t, &full, Schema::Map(inner), out);
                }
            }
            Schema::ArrayOf(elem) => {
                if let toml::Value::Array(items) = val {
                    for (i, item) in items.iter().enumerate() {
                        if let toml::Value::Table(t) = item {
                            walk(t, &format!("{full}[{i}]"), *elem, out);
                        }
                    }
                }
            }
        }
    }
}
