//! The herdr source: session discovery and snapshot polling across three
//! backends (`internal/herdr/herdr.go` + `local.go` / `windows.go` /
//! `wsl.go`):
//!
//!   - [`Local`]:   native Unix sockets at ~/.config/herdr (Linux, WSL, macOS)
//!   - [`Windows`]: herdr.exe sessions (native Windows, or the Windows side
//!     polled from WSL via interop)
//!   - [`Wsl`]:     sessions inside a WSL distro, polled from a
//!     Windows-native process via wsl.exe
//!
//! The types here ([`Agent`], [`Snapshot`], [`Session`]) and the
//! [`Backend`] trait are the shared contract with the engine, tray and
//! flyout ports.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use crate::proc;

// Agent statuses reported by herdr's snapshot API (`herdr.Status*`).
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_BLOCKED: &str = "blocked";
pub const STATUS_DONE: &str = "done";
pub const STATUS_UNKNOWN: &str = "unknown";

/// One monitored agent pane, normalized across sources (`herdr.Agent`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub pane_id: String,
    pub name: String,
    pub status: String,
    pub title: String,
    pub project: String,
    /// The agent's tab's display label ("#", "#test"; `tabs.label`, joined
    /// from the agent's `tab_id` at parse time).
    pub tab: String,
    pub focused: bool,
}

/// A tab's display label, keyed by tab id (`herdr.Tab`; the snapshot's
/// `tabs` array is what names tabs — "#" and "#test" are real labels).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabLabel {
    pub tab_id: String,
    pub label: String,
}

/// A point-in-time view of one session (`herdr.Snapshot`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub agents: Vec<Agent>,
    pub tabs: Vec<TabLabel>,
}

/// One herdr session on one backend (`herdr.Session`). `ref_` is the
/// backend-specific handle (socket path or session name; Go field `Ref`);
/// `home` is the WSL $HOME used to build PATH entries inside the distro.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub host: String,
    pub name: String,
    pub ref_: String,
    pub home: String,
}

/// Discovers sessions and fetches their snapshots (`herdr.Backend`). Go's
/// `context.Context` parameters map to explicit `timeout` arguments — every
/// caller bounds its calls with the backend's configured timeout.
///
/// Object safety: implementors are used as `Box<dyn Backend>` /
/// `Arc<dyn Backend>` collections in the engine, so the trait keeps
/// method receivers object-safe.
pub trait Backend: Send + Sync {
    /// Backend name: "local" | "windows" | "wsl".
    fn name(&self) -> &'static str;
    /// Session discovery, bounded by `timeout`.
    fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<Session>>;
    /// Fetches one session's snapshot, bounded by `timeout`.
    fn fetch(&self, session: &Session, timeout: Duration) -> anyhow::Result<Snapshot>;
    /// How often Discover should be re-run; zero means every poll
    /// (`Backend.Rescan`).
    fn rescan(&self) -> Duration;
}

// ---------------------------------------------------------------------------
// Snapshot parsing

/// Decodes herdr CLI JSON, peeling the response envelopes
/// ({"id": ..., "result": {"snapshot": {...}}}) and normalizing agents
/// (`herdr.ParseSnapshot`). Accepts UTF-16LE output (wsl.exe transcodes
/// some outputs); plain UTF-8 passes through untouched.
pub fn parse_snapshot(data: &[u8]) -> anyhow::Result<Snapshot> {
    let data = proc::decode_utf16(data);
    let doc: Value = serde_json::from_slice(&data)
        .map_err(|e| anyhow::anyhow!("parse snapshot JSON: {e}"))?;
    let doc = unwrap_value(doc);
    let mut snap = Snapshot::default();
    // Tab labels first: agents join their tab_id against them (the pane
    // object itself carries no tab name).
    let labels: std::collections::HashMap<String, String> = doc
        .get("tabs")
        .and_then(Value::as_array)
        .map(|raw| {
            raw.iter()
                .filter_map(|t| match t {
                    Value::Object(m) => {
                        let id = str_field(m, &["tab_id"]);
                        if id.is_empty() {
                            None
                        } else {
                            Some((id, str_field(m, &["label"])))
                        }
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(raw) = doc.get("tabs").and_then(Value::as_array) {
        for t in raw {
            if let Value::Object(m) = t {
                snap.tabs.push(TabLabel {
                    tab_id: str_field(m, &["tab_id"]),
                    label: str_field(m, &["label"]),
                });
            }
        }
    }
    if let Some(raw) = doc.get("agents").and_then(Value::as_array) {
        for ra in raw {
            let m = match ra {
                Value::Object(m) => m,
                _ => continue,
            };
            let tab_id = str_field(m, &["tab_id"]);
            snap.agents.push(Agent {
                name: str_field(m, &["agent", "display_agent"]),
                status: normalize_status(&str_field(m, &["agent_status"])).to_string(),
                title: str_field(m, &["terminal_title_stripped", "terminal_title"]),
                project: project_of(&[
                    str_field(m, &["foreground_cwd"]),
                    str_field(m, &["cwd"]),
                ]),
                pane_id: str_field(m, &["pane_id"]),
                tab: labels.get(&tab_id).cloned().unwrap_or_default(),
                focused: matches!(m.get("focused"), Some(Value::Bool(true))),
            });
        }
    }
    Ok(snap)
}

/// Walks the CLI envelope keys result/snapshot until the payload
/// (`herdr.unwrap`).
fn unwrap_value(mut doc: Value) -> Value {
    loop {
        let mut found = false;
        for key in ["snapshot", "result"] {
            if doc.get(key).map(Value::is_object).unwrap_or(false) {
                doc = doc[key].take();
                found = true;
                break;
            }
        }
        if !found {
            return doc;
        }
    }
}

/// Maps a raw status string onto the known set (`herdr.NormalizeStatus`).
pub fn normalize_status(s: &str) -> &'static str {
    // strings.ToLower is Unicode-aware in Go; match that rather than
    // ASCII-only lowering.
    match s.trim().to_lowercase().as_str() {
        STATUS_IDLE => STATUS_IDLE,
        STATUS_WORKING => STATUS_WORKING,
        STATUS_BLOCKED => STATUS_BLOCKED,
        STATUS_DONE => STATUS_DONE,
        _ => STATUS_UNKNOWN,
    }
}

/// First non-empty string among the JSON fields, in order (`herdr.str`).
fn str_field(m: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
    for k in keys.iter().copied() {
        if let Some(v) = m.get(k) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    String::new()
}

/// Basename of the first non-empty path, tolerating Windows separators
/// (`herdr.projectOf`).
fn project_of(paths: &[String]) -> String {
    for p in paths {
        let p = p.replace('\\', "/");
        // strings.TrimRight: trailing slashes only (Go herdr.go:137).
        let p = p.trim_end_matches('/');
        if p.is_empty() {
            continue;
        }
        return match p.rfind('/') {
            Some(i) => p[i + 1..].to_string(),
            None => p.to_string(),
        };
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Local backend (local.go)

/// Polls herdr sessions through their native Unix sockets:
/// `<dir>/herdr.sock` (session "default") and `<dir>/sessions/<name>/herdr.sock`
/// (`herdr.Local`).
pub struct Local {
    dir: Option<PathBuf>,
    binary: String,
    once: OnceLock<Option<PathBuf>>, // resolved binary (LookPath), cached
}

impl Local {
    /// Builds the backend; socket_dir and binary fall back to defaults
    /// (`herdr.NewLocal`).
    pub fn new(socket_dir: &str, binary: &str) -> Local {
        let dir = if socket_dir.is_empty() {
            dirs::home_dir().map(|h| h.join(".config").join("herdr"))
        } else {
            Some(PathBuf::from(socket_dir))
        };
        let binary = if binary.is_empty() { "herdr".to_string() } else { binary.to_string() };
        Local { dir, binary, once: OnceLock::new() }
    }

    /// Reports whether the socket directory and binary exist, so the engine
    /// can skip the backend under "auto" (`Local.Available`).
    pub fn available(&self) -> bool {
        match &self.dir {
            Some(d) if std::fs::metadata(d).map(|m| m.is_dir()).unwrap_or(false) => {}
            _ => return false,
        }
        self.lookup_binary().is_some()
    }

    fn lookup_binary(&self) -> Option<PathBuf> {
        self.once.get_or_init(|| proc::look_path(&self.binary)).clone()
    }

    fn is_socket(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(path)
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }
}

impl Backend for Local {
    fn name(&self) -> &'static str {
        "local"
    }

    fn rescan(&self) -> Duration {
        Duration::ZERO // discovery is a cheap glob
    }

    fn discover(&self, _timeout: Duration) -> anyhow::Result<Vec<Session>> {
        let dir = self.dir.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no herdr socket directory (home unavailable)")
        })?;
        if self.lookup_binary().is_none() {
            anyhow::bail!("herdr binary {:?} not found in PATH", self.binary);
        }
        let mut out = Vec::new();
        let root = dir.join("herdr.sock");
        if self.is_socket(&root) {
            out.push(Session {
                host: "local".into(),
                name: "default".into(),
                ref_: root.display().to_string(),
                home: String::new(),
            });
        }
        let sdir = dir.join("sessions");
        if let Ok(entries) = std::fs::read_dir(&sdir) {
            // os.ReadDir returns entries sorted by filename; read_dir makes
            // no order guarantee, so sort to keep session order deterministic.
            let mut dirs: Vec<std::fs::DirEntry> = entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .collect();
            dirs.sort_by_key(|e| e.file_name());
            for ent in dirs {
                let sock = ent.path().join("herdr.sock");
                if self.is_socket(&sock) {
                    out.push(Session {
                        host: "local".into(),
                        name: ent.file_name().display().to_string(),
                        ref_: sock.display().to_string(),
                        home: String::new(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn fetch(&self, session: &Session, timeout: Duration) -> anyhow::Result<Snapshot> {
        let mut reference = session.ref_.clone();
        if reference.is_empty() {
            // the "default" session; Go's filepath.Join("", "herdr.sock")
            // yields the relative path "herdr.sock" when home was
            // unavailable, so replicate that rather than an empty ref.
            reference = match &self.dir {
                Some(d) => d.join("herdr.sock").display().to_string(),
                None => "herdr.sock".to_string(),
            };
        }
        let argv = vec![self.binary.clone(), "api".into(), "snapshot".into()];
        let env = vec![format!("HERDR_SOCKET_PATH={reference}")];
        let out = proc::run(&argv, &env, "", timeout)?;
        parse_snapshot(&out)
    }
}

// ---------------------------------------------------------------------------
// Windows backend (windows.go)

/// Polls herdr.exe sessions: natively on Windows, or the Windows side from
/// WSL via interop (herdr.exe is reachable from PATH there) (`herdr.Windows`).
pub struct Windows {
    binary: PathBuf,
    sessions: Vec<String>,
    dir: Option<PathBuf>,
}

impl Windows {
    /// Resolves the herdr.exe binary. binary_spec may be "auto" (default),
    /// a name looked up in PATH, or an explicit path. The extra sessions
    /// are polled in addition to the default one (`herdr.NewWindows`).
    pub fn new(binary_spec: &str, sessions: &[String]) -> anyhow::Result<Windows> {
        let mut candidates: Vec<&str> = vec!["herdr.exe"];
        if cfg!(target_os = "windows") {
            candidates = vec!["herdr", "herdr.exe"];
        }
        let explicit: Option<&str> = if !binary_spec.is_empty() && binary_spec != "auto" {
            Some(binary_spec)
        } else {
            None
        };
        let list: Vec<&str> = explicit
            .map(|b| vec![b])
            .unwrap_or_else(|| candidates.clone());
        for c in &list {
            if let Some(p) = proc::look_path(c) {
                return Ok(Windows {
                    binary: p,
                    sessions: sessions.to_vec(),
                    dir: proc::interop_dir(),
                });
            }
        }
        // Go's fmt %v on []string: "[herdr herdr.exe]" (windows.go:40).
        anyhow::bail!("herdr.exe not found in PATH (tried [{}])", list.join(" "))
    }
}

impl Backend for Windows {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn rescan(&self) -> Duration {
        Duration::ZERO // session list is static config
    }

    fn discover(&self, _timeout: Duration) -> anyhow::Result<Vec<Session>> {
        let mut names = vec!["default".to_string()];
        for s in &self.sessions {
            if s.is_empty() || s == "default" {
                continue;
            }
            names.push(s.clone());
        }
        Ok(names
            .into_iter()
            .map(|n| Session { host: "windows".into(), name: n.clone(), ref_: n, home: String::new() })
            .collect())
    }

    fn fetch(&self, session: &Session, timeout: Duration) -> anyhow::Result<Snapshot> {
        let mut argv = vec![self.binary.display().to_string()];
        if session.name != "default" {
            argv.push("--session".into());
            argv.push(session.name.clone());
        }
        argv.push("api".into());
        argv.push("snapshot".into());
        let dir = self.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        let out = proc::run(&argv, &[], &dir, timeout)?;
        parse_snapshot(&out)
    }
}

// ---------------------------------------------------------------------------
// WSL backend (wsl.go)

/// The base PATH used for herdr calls inside the distro. wsl.exe -e runs
/// without a login shell, so ~/.local/bin and ~/.cargo/bin (the usual herdr
/// install locations) must be re-added explicitly (`herdr.defaultWSLPath`).
pub const DEFAULT_WSL_PATH: [&str; 6] =
    ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"];

/// Lists herdr sockets with absolute paths, one per line
/// (`herdr.discoveryScript`).
const DISCOVERY_SCRIPT: &str = "for s in \"$HOME\"/.config/herdr/herdr.sock \"$HOME\"/.config/herdr/sessions/*/herdr.sock; do [ -S \"$s\" ] && printf '%s\\n' \"$s\"; done";

/// Polls sessions inside a WSL distro from a Windows-native process,
/// through wsl.exe interop (`herdr.WSL`).
pub struct Wsl {
    base: Vec<String>, // wsl.exe [-d distro]
    only: Vec<String>, // restrict discovery to these session names
    extra_path: Vec<String>,
    rescan: Duration,
}

impl Wsl {
    /// Builds the backend. distro empty means the default distro; sessions
    /// empty means auto-discover everything (`herdr.NewWSL`).
    pub fn new(distro: &str, sessions: &[String], extra_path: &[String], rescan: Duration) -> Wsl {
        let mut base = vec!["wsl.exe".to_string()];
        if !distro.is_empty() {
            base.push("-d".into());
            base.push(distro.to_string());
        }
        Wsl { base, only: sessions.to_vec(), extra_path: extra_path.to_vec(), rescan }
    }
}

/// Maps a socket path to its session name and the distro $HOME it lives
/// under (`herdr.sessionFromSocket`):
/// `<home>/.config/herdr/herdr.sock` is the default session,
/// `<home>/.config/herdr/sessions/<name>/herdr.sock` a named one.
fn session_from_socket(sock: &str) -> (String, String) {
    const MARKER: &str = "/.config/herdr/";
    let mut home = String::new();
    if let Some(i) = sock.find(MARKER) {
        if i > 0 {
            home = sock[..i].to_string();
        }
    }
    let path = Path::new(sock);
    if path.file_name().and_then(|s| s.to_str()) != Some("herdr.sock") {
        return (String::new(), home);
    }
    let Some(parent) = path.parent() else {
        return (String::new(), home);
    };
    let name_of = |p: Option<&Path>| -> String {
        p.and_then(|p| p.file_name().and_then(|s| s.to_str())).unwrap_or("").to_string()
    };
    // <home>/.config/herdr/sessions/<name>/herdr.sock
    if name_of(parent.parent()) == "sessions" {
        return (name_of(Some(parent)), home);
    }
    // <home>/.config/herdr/herdr.sock
    if name_of(Some(parent)) == "herdr" {
        return ("default".to_string(), home);
    }
    (String::new(), home)
}

impl Backend for Wsl {
    fn name(&self) -> &'static str {
        "wsl"
    }

    fn rescan(&self) -> Duration {
        self.rescan
    }

    fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<Session>> {
        let mut argv = self.base.clone();
        argv.extend(["-e".into(), "sh".into(), "-c".into(), DISCOVERY_SCRIPT.into()]);
        let out = proc::run(&argv, &[], "", timeout)?;
        let out = proc::decode_utf16(&out);
        let mut sessions = Vec::new();
        for line in String::from_utf8_lossy(&out).split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (name, home) = session_from_socket(line);
            if name.is_empty() {
                continue;
            }
            if !self.only.is_empty() && !self.only.iter().any(|o| *o == name) {
                continue;
            }
            sessions.push(Session { host: "wsl".into(), name, ref_: line.to_string(), home });
        }
        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sessions)
    }

    fn fetch(&self, session: &Session, timeout: Duration) -> anyhow::Result<Snapshot> {
        let mut argv = self.base.clone();
        argv.push("-e".into());
        if !session.home.is_empty() {
            let mut path = format!(
                "{}/.local/bin:{}/.cargo/bin:{}",
                session.home,
                session.home,
                DEFAULT_WSL_PATH.join(":")
            );
            for p in &self.extra_path {
                path.push(':');
                path.push_str(p);
            }
            argv.extend([
                "env".into(),
                format!("PATH={path}"),
                format!("HERDR_SOCKET_PATH={}", session.ref_),
                "herdr".into(),
                "api".into(),
                "snapshot".into(),
            ]);
        } else {
            argv.extend(["herdr".into(), "api".into(), "snapshot".into()]);
        }
        let out = proc::run(&argv, &[], "", timeout)?;
        parse_snapshot(&out)
    }
}

/// Returns the wsl.exe argument tail that runs a one-shot herdr command
/// against a discovered session socket, the same way the wsl backend
/// itself invokes herdr: no login shell, explicit PATH (wsl.exe -e would
/// drop ~/.local/bin), and HERDR_SOCKET_PATH to pick the session. Without
/// a socket ref it falls back to --session (`herdr.WSLHerdrArgs`).
pub fn wsl_herdr_args(
    reference: &str,
    home: &str,
    extra_path: &[String],
    args: &[&str],
) -> Vec<String> {
    if reference.is_empty() || home.is_empty() {
        let mut out: Vec<String> = vec!["-e".into(), "herdr".into(), "--session".into()];
        out.extend(args.iter().map(|s| (*s).to_string()));
        return out;
    }
    let mut path = format!("{home}/.local/bin:{home}/.cargo/bin:{}", DEFAULT_WSL_PATH.join(":"));
    for p in extra_path {
        path.push(':');
        path.push_str(p);
    }
    let mut out: Vec<String> = vec![
        "-e".into(),
        "env".into(),
        format!("PATH={path}"),
        format!("HERDR_SOCKET_PATH={reference}"),
        "herdr".into(),
    ];
    out.extend(args.iter().map(|s| (*s).to_string()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // herdr_test.go fixture: response envelope wrapping the snapshot.
    const FIXTURE: &str = r#"{"id":"cli:api:snapshot","result":{"snapshot":{
  "version":"0.8.2","protocol":20,
  "agents":[
    {"agent":"claude","agent_status":"working","cwd":"/home/dev/projects/api-server",
     "foreground_cwd":"/home/dev/projects/api-server","focused":true,"pane_id":"w1:p1",
     "terminal_title":"π : Refactor auth module","terminal_title_stripped":"Refactor auth module"},
    {"agent":"codex","agent_status":"idle","cwd":"C:\\work\\website",
     "foreground_cwd":"","focused":false,"pane_id":"w2:p3",
     "terminal_title":"","terminal_title_stripped":""},
    {"agent":"aide","agent_status":"Blocked","cwd":"","focused":false,"pane_id":"w3:p1"}
  ]
}}}"#;

    #[test]
    fn test_parse_snapshot() {
        let snap = parse_snapshot(FIXTURE.as_bytes()).expect("parse");
        assert_eq!(snap.agents.len(), 3, "agents = {}, want 3", snap.agents.len());
        let a = &snap.agents[0];
        assert!(
            a.name == "claude"
                && a.status == STATUS_WORKING
                && a.title == "Refactor auth module"
                && a.project == "api-server"
                && a.pane_id == "w1:p1"
                && a.focused,
            "agent[0] mis-normalized: {a:?}"
        );
        let b = &snap.agents[1];
        assert_eq!(b.project, "website"); // windows separator, foreground empty
        let c = &snap.agents[2];
        assert_eq!(c.status, STATUS_BLOCKED); // case-insensitive
        assert!(
            c.project.is_empty() && c.title.is_empty(),
            "agent[2] should be empty: {c:?}"
        );
    }

    #[test]
    fn test_parse_snapshot_bare() {
        // no envelope wrapping
        parse_snapshot(br#"{"agents":[]}"#).expect("bare snapshot");
    }

    #[test]
    fn test_parse_snapshot_joins_tab_labels() {
        // agents carry only tab_id; the label lives in the tabs array and
        // parse_snapshot joins them ("#", "#test" are real tab names)
        let doc = br##"{"id":"x","result":{"snapshot":{
            "agents":[
                {"agent":"omp","agent_status":"done","pane_id":"w2S:p1","tab_id":"w2S:t1","workspace_id":"w2S"},
                {"agent":"pi","agent_status":"working","pane_id":"w2S:p3","tab_id":"w2S:t2","workspace_id":"w2S"},
                {"agent":"zed","agent_status":"idle","pane_id":"w9:p1","tab_id":"w9:t9","workspace_id":"w9"}
            ],
            "tabs":[
                {"tab_id":"w2S:t1","label":"#","number":1,"workspace_id":"w2S"},
                {"tab_id":"w2S:t2","label":"#test","number":2,"workspace_id":"w2S"}
            ]}}}"##;
        let snap = parse_snapshot(doc).expect("snapshot parses");
        assert_eq!(snap.tabs.len(), 2);
        assert_eq!(snap.agents[0].tab, "#");
        assert_eq!(snap.agents[1].tab, "#test");
        assert_eq!(snap.agents[2].tab, "", "unknown tab id stays empty");
    }

    #[test]
    fn test_session_from_socket() {
        let cases: &[(&str, &str, &str)] = &[
            ("/home/dev/.config/herdr/herdr.sock", "default", "/home/dev"),
            ("/home/dev/.config/herdr/sessions/work/herdr.sock", "work", "/home/dev"),
            ("/tmp/other/sock", "", ""), // unrecognized layout
        ];
        for (sock, want_name, want_home) in cases {
            let (name, home) = session_from_socket(sock);
            assert_eq!(
                (name.as_str(), home.as_str()),
                (*want_name, *want_home),
                "sessionFromSocket({sock:?}) = {name:?},{home:?} want {want_name:?},{want_home:?}"
            );
        }
    }

    #[test]
    fn test_decode_utf16() {
        let ascii = b"{\"agents\":[]}";
        assert_eq!(proc::decode_utf16(ascii).as_ref(), ascii, "ascii mangled");
        // "OK" as UTF-16LE with BOM
        let le = [0xFF, 0xFE, 0x4F, 0x00, 0x4B, 0x00];
        assert_eq!(proc::decode_utf16(&le).as_ref(), b"OK", "BOM decode");
        // UTF-16LE without BOM (wsl.exe style)
        let nb = b"{\x00\"\x00a\x00\"\x00";
        assert_eq!(proc::decode_utf16(nb).as_ref(), b"{\"a\"", "no-BOM decode");
    }
}
