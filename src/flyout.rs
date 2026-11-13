//! The flyout panel, redesigned (v2): a session grid, rune-kit-rendered
//! reinterpretation of the Go GDI panel — now a **regular window** (titled,
//! resizable, min/max/close, taskbar icon, centered) instead of the Go
//! borderless flyover. From the Go only the *interaction* contract stays
//! (`tray/flyout_windows.go`: windowless app, 1 s refresh, agent-click
//! focus, session collapse); the surface rebuilds from
//! [`rune_kit::Kit`] tokens:
//!
//! - one state code — the tray icon's five colors (`icon::color`) —
//!   carried by the header dot, the count chips, the proportional state
//!   bar, and every row dot; only *blocked* rows get a tinted background.
//! - sessions are custom collapsible triggers (kit glyphs, kit hover,
//!   a11y button roles), defaulted open when they hold attention agents.
//!   "N finished" / "N resting" groups collapse the settled agents.
//! - sessions sort attention-first (blocked > working > waiting >
//!   resting, offline last), agents the same way inside a session.
//!
//! RUNE MAPPING (verified against the vendored rune source at rev 003a5c7):
//! a titled, resizable, centered window opened by a windowless
//! [`rune::App`] (the `examples/launcher` pattern — closing hides, the app
//! keeps running), driven by `AppSpec::poll` for the refresh cadence,
//! `.scroll(ScrollSpec)` for wheel scrolling, `rune::set_app_icon` for the
//! title-bar/taskbar icon.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::el::ScrollSpec;
use rune::{
    col, el, icon, row, spacer, text, App, AppHandle, AppSpec, Color, Cx, Display, El, Tx,
};
use rune::{NativeDisplayId, TextStyle, WindowOptions};
use rune_kit::{Kit, PaletteSeed, SegmentedControl};

use crate::engine::{self, View};
use crate::icon;

// ---------------------------------------------------------------------------
// Panel model — the data the panel renders, unit tested
// ---------------------------------------------------------------------------

/// Panel padding.
pub const FLY_PAD: f32 = 20.0;

/// Grid geometry (`flyout_windows.go`'s colMinW/colMaxW/colGap/gridRowGap,
/// interpreted as points): sessions fill a columns × rows grid; the column
/// count whose panel rectangle best matches a widescreen 1.5 aspect ratio
/// wins, so a large fleet spreads sideways instead of growing a long scroll.
pub const COL_MIN_W: f32 = 240.0;
pub const COL_MAX_W: f32 = 400.0;
pub const COL_GAP: f32 = 24.0;
pub const GRID_ROW_GAP: f32 = 22.0;
/// Below this a column is unreadably narrow and the candidate is skipped.
const COL_MIN_READABLE: f32 = 200.0;

/// The panel is a fixed-size window now (`PANEL_W` × ~60 % of the work
/// area): tabs keep the fleet navigable, the grid fills the overview tab,
/// and the old content-fit shrink is gone — its height estimates drifted
/// from rune's real layout, which is where the surprise scrollbar came
/// from.
const TAB_BAR_H: f32 = 42.0;

/// One agent line in the panel: a two-line attention block (working /
/// blocked) or a compact single-line row inside an expanded group.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentItem {
    pub host: String,
    pub sess: String,
    pub pane: String,
    /// What the agent is doing: herdr's title, or the project when untitled.
    pub title: String,
    /// The herdr project/workspace name (the tab name's left side).
    pub project: String,
    /// The tab's display label ("#", "#test") — herdr's `tabs` array.
    pub tab: String,
    /// "runner · project" (or just the runner) in the dim caption line.
    pub meta: String,
    /// The herdr client currently has this pane focused.
    pub focused: bool,
    /// Panel state word: "blocked" | "working" | "waiting" | "idle".
    pub sev: &'static str,
}

/// A collapsed-by-default group of settled agents ("N finished" /
/// "N resting", `tray.appendGroup`).
#[derive(Clone, Debug, PartialEq)]
pub struct GroupItem {
    /// Rest-toggle key ("host/name/finished" | "host/name/resting").
    pub key: String,
    /// "finished" | "resting".
    pub label: &'static str,
    /// Icon-color key for the group dot.
    pub sev: &'static str,
    pub open: bool,
    pub agents: Vec<AgentItem>,
}

/// One session and its rows (`SessionView` reshaped for the panel).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSection {
    /// Collapse-toggle key ("host/name").
    pub key: String,
    pub name: String,
    pub host: String,
    pub up: bool,
    /// Effective open state (default + user override).
    pub open: bool,
    pub total: usize,
    /// blocked + working agents, sorted attention-first.
    pub active: Vec<AgentItem>,
    /// Latched done agents ("N finished").
    pub finished: Option<GroupItem>,
    /// Everything settled and unlatched ("N resting").
    pub resting: Option<GroupItem>,
}

/// One count chip in the header ("1 blocked"); the chip set doubles as the
/// legend for the row-dot state code.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub label: String,
    /// Icon-color key.
    pub sev: &'static str,
}

/// One proportional segment of the fleet state bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarSeg {
    /// Icon-color key ("blocked" | "down" | "working" | "waiting" | "idle").
    pub sev: &'static str,
    pub weight: f32,
}

/// The panel's data: header summary + session sections. Carries semantics
/// only — rune's layout engine sizes everything (`buildRows` rethought).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PanelModel {
    /// Fleet severity word (`View::severity`), colors the header dot.
    pub fleet_sev: &'static str,
    pub chips: Vec<Chip>,
    pub bar: Vec<BarSeg>,
    pub sessions: Vec<SessionSection>,
    /// Nothing known yet: render the invitation instead of sections.
    pub empty: bool,
}

/// Renders an icon color in Windows COLORREF (BGR) layout
/// (`tray.colorRef`).
pub fn colorref(c: icon::Rgb) -> u32 {
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "—".to_string()
    } else {
        s.to_string()
    }
}

/// Collapses runs of whitespace to single spaces (herdr titles carry
/// newlines and tabs that must not break the single-line rows).
///
/// PORT-NOTE: the Go painter's tofu-glyph stripping (`tray.gdiSafe`) is
/// gone — rune shapes text with Parley and renders emoji/pictographs fine,
/// so titles render as herdr sent them.
fn flat_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The agent's line-1 text: herdr's task title, or the project when the
/// agent has no title (`buildRows`' title fallback).
fn agent_title(a: &engine::AgentView) -> String {
    if a.title.is_empty() {
        a.project.clone()
    } else {
        a.title.clone()
    }
}

/// The agent's line-2 text: "runner · project", or just the runner
/// (buildRows' info line).
fn agent_meta(a: &engine::AgentView) -> String {
    if a.project.is_empty() {
        a.name.clone()
    } else {
        format!("{} · {}", a.name, a.project)
    }
}

/// The panel word for an agent state; the icon-color keys are the same
/// words except resting, which paints the tray's idle gray.
fn sev_word(status: &str, wants: bool) -> &'static str {
    match engine::row_severity(status, wants) {
        "blocked" => "blocked",
        "working" => "working",
        "waiting" => "waiting",
        _ => "idle",
    }
}

/// Attention rank for sorting: blocked first, working next, then latched
/// finished, then resting.
fn sev_rank(sev: &str) -> u8 {
    match sev {
        "blocked" => 0,
        "working" => 1,
        "waiting" => 2,
        _ => 3,
    }
}

/// Builds the panel model from a View and the user's collapse overrides
/// (`tray.buildRows` in flyout_rows.go, rethought): sessions sort
/// attention-first (min agent severity, offline last, name ties), agents
/// sort the same way inside a session, a session expands by default only
/// when it holds working or blocked agents, and finished/resting agents
/// collapse into per-session group lines. `open_state` and `rest_open`
/// hold the user's click overrides (session key "host/name"; group key
/// "host/name/finished" | "host/name/resting").
pub fn build_model(
    v: &View,
    open_state: &HashMap<String, bool>,
    rest_open: &HashMap<String, bool>,
) -> PanelModel {
    let mut m = PanelModel {
        fleet_sev: v.severity(),
        ..PanelModel::default()
    };
    if v.sessions.is_empty() {
        m.empty = true;
        return m;
    }

    // fleet counts -> header chips (nonzero, severity order) + state bar
    let (mut blocked, mut down, mut working, mut waiting, mut total) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for s in &v.sessions {
        if !s.up {
            down += 1;
            continue;
        }
        for a in &s.agents {
            total += 1;
            match engine::row_severity(&a.status, a.wants) {
                "blocked" => blocked += 1,
                "working" => working += 1,
                "waiting" => waiting += 1,
                _ => {}
            }
        }
    }
    let resting = total.saturating_sub(blocked + working + waiting);
    for (n, sev, label) in [
        (blocked, "blocked", "blocked"),
        (working, "working", "working"),
        (waiting, "waiting", "waiting"),
        (resting, "idle", "resting"),
    ] {
        if n > 0 {
            m.chips.push(Chip {
                label: format!("{n} {label}"),
                sev,
            });
        }
    }
    for (n, sev) in [
        (blocked, "blocked"),
        (down, "down"),
        (working, "working"),
        (waiting, "waiting"),
        (resting, "idle"),
    ] {
        if n > 0 {
            m.bar.push(BarSeg {
                sev,
                weight: n as f32,
            });
        }
    }

    // one section per session, grouped and sorted
    let mut secs: Vec<SessionSection> = Vec::with_capacity(v.sessions.len());
    for s in &v.sessions {
        let key = format!("{}/{}", s.host, s.name);
        let (mut active, mut finished, mut resting) = (Vec::new(), Vec::new(), Vec::new());
        for a in &s.agents {
            let item = AgentItem {
                host: s.host.clone(),
                sess: s.name.clone(),
                pane: a.pane_id.clone(),
                title: flat_ws(&or_dash(&agent_title(a))),
                project: a.project.clone(),
                tab: a.tab.clone(),
                meta: flat_ws(&agent_meta(a)),
                focused: a.focused,
                sev: sev_word(&a.status, a.wants),
            };
            match engine::row_severity(&a.status, a.wants) {
                "working" | "blocked" => active.push(item),
                "waiting" => finished.push(item),
                _ => resting.push(item),
            }
        }
        let by_attention = |x: &AgentItem, y: &AgentItem| {
            sev_rank(x.sev)
                .cmp(&sev_rank(y.sev))
                .then_with(|| x.title.cmp(&y.title))
        };
        active.sort_by(by_attention);
        finished.sort_by(by_attention);
        resting.sort_by(by_attention);

        let open = open_state.get(&key).copied().unwrap_or(!active.is_empty());
        let group =
            |label: &'static str, sev: &'static str, agents: Vec<AgentItem>, tag: &str| -> Option<GroupItem> {
                if agents.is_empty() {
                    return None;
                }
                let gkey = format!("{key}/{tag}");
                Some(GroupItem {
                    open: rest_open.get(&gkey).copied().unwrap_or(false),
                    key: gkey,
                    label,
                    sev,
                    agents,
                })
            };
        secs.push(SessionSection {
            finished: group("finished", "waiting", finished, "finished"),
            resting: group("resting", "idle", resting, "resting"),
            key: key.clone(),
            name: s.name.clone(),
            host: s.host.clone(),
            up: s.up,
            open,
            total: s.agents.len(),
            active,
        });
    }
    secs.sort_by(|x, y| {
        section_rank(x)
            .cmp(&section_rank(y))
            .then_with(|| x.name.cmp(&y.name))
            .then_with(|| x.host.cmp(&y.host))
    });
    m.sessions = secs;
    m
}

/// Session attention rank for sorting: the strongest agent state in the
/// session; offline sessions sink below everything.
fn section_rank(s: &SessionSection) -> u8 {
    if !s.up {
        return 9;
    }
    s.active
        .iter()
        .map(|a| sev_rank(a.sev))
        .min()
        .unwrap_or(4) // nothing in attention: below working, above offline
}

// ---------------------------------------------------------------------------
// Geometry (flyout_windows.go consts + layout)
// ---------------------------------------------------------------------------

/// View refresh cadence (`refreshEvery`).
pub const REFRESH_EVERY: Duration = Duration::from_secs(1);
// PORT-NOTE (2026-11-12): the panel is a regular window now — Go's
// activateGrace, the auto-close timer and the Escape/click-outside
// dismissal are gone with the flyover semantics. A window stays open until
// the user closes it (the native close hides it; the app keeps running).

/// Persisted UI state — one tiny TOML line next to the config file, so
/// the title mode survives restarts. Best effort: unreadable/missing
/// falls back to the task-titles default; write failures are silent.
fn ui_state_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("agent-notify").join("ui.toml"))
}

fn load_title_mode() -> TitleMode {
    let Some(path) = ui_state_path() else {
        return TitleMode::Task;
    };
    match std::fs::read_to_string(path) {
        Ok(s) if s.contains("\"tab\"") => TitleMode::Tab,
        _ => TitleMode::Task,
    }
}

fn save_title_mode(mode: TitleMode) {
    let Some(path) = ui_state_path() else {
        return;
    };
    let v = match mode {
        TitleMode::Task => "task",
        TitleMode::Tab => "tab",
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("title_mode = \"{v}\"\n"));
}

/// Estimated part heights, in points, that [`metrics`] sums to size the
/// window. The real layout is flex (rune measures text); these stay coarse
/// because their only job is picking the opening grid geometry.
const EST_TITLE_H: f32 = 26.0; // "Agent Notify" + version
const EST_CHIPS_H: f32 = 22.0; // count chips row
const EST_BAR_H: f32 = 6.0; // state bar
const EST_HEADER_AIR: f32 = 56.0; // header gaps + divider + panel rhythm
const EST_TRIGGER_H: f32 = 38.0; // session header
const EST_AGENT_H: f32 = 56.0; // two-line attention block
const EST_GROUP_H: f32 = 32.0; // "N finished"/"N resting" trigger
const EST_COMPACT_H: f32 = 26.0; // expanded group's agent line
/// Card padding around one session section, top+bottom (the estimates
/// above carry the rows; the card adds this wrapper).
const EST_CARD_PAD: f32 = 28.0;

/// Estimated height of one session section (`rowHeight` over the current
/// open/closed state).
fn section_est_h(s: &SessionSection) -> f32 {
    let mut h = EST_CARD_PAD + EST_TRIGGER_H;
    if s.open {
        h += s.active.len() as f32 * EST_AGENT_H;
        for g in [&s.finished, &s.resting].into_iter().flatten() {
            h += EST_GROUP_H;
            if g.open {
                h += g.agents.len() as f32 * EST_COMPACT_H;
            }
        }
    }
    h
}

const HEADER_EST_H: f32 = EST_TITLE_H + EST_CHIPS_H + EST_BAR_H + EST_HEADER_AIR;

/// Window sizing for the panel: a **fixed** window — `PANEL_W` wide,
/// ~60 % of the work area tall — with the session grid fitted to its
/// overview tab (`flyout.layout`'s scoring, now against the fixed inner
/// height instead of the whole work area). Content that still overflows
/// scrolls inside the window; the window itself never resizes to content.
#[derive(Debug)]
pub struct Metrics {
    /// Session columns on the overview tab (1 when the area is narrow;
    /// sessions wrap to further rows beyond `cols` per row).
    pub cols: usize,
    /// Inner width of one session column.
    pub col_w: f32,
    /// Outer panel width in points (fixed).
    pub width: f32,
    /// Fixed window height in points.
    pub window_h: f32,
    /// Inner height the overview grid may fill before it scrolls.
    pub grid_h: f32,
}

pub fn metrics(m: &PanelModel, area_w: f32, area_h: f32) -> Metrics {
    // Generous room: ~62 % of the work area wide (1080 cap), ~72 % tall
    // (780 cap) — full titles over cramped columns.
    let width = (area_w * 0.62).clamp(560.0, 1080.0).min((area_w - 48.0).max(320.0));
    let window_h = (area_h * 0.72)
        .clamp(480.0, 780.0)
        .min((area_h - 48.0).max(300.0));
    // overview tab: header + tab bar above the grid
    let grid_h = window_h - 2.0 * FLY_PAD - HEADER_EST_H - TAB_BAR_H - 12.0;

    let groups: Vec<f32> = m.sessions.iter().map(section_est_h).collect();
    let avail = width - 2.0 * FLY_PAD;

    let mut max_cols = 1usize;
    while max_cols < groups.len()
        && ((max_cols + 1) as f32 * (COL_MIN_W + COL_GAP) - COL_GAP) <= avail
    {
        max_cols += 1;
    }

    // Spread as wide as the fixed grid area allows: the tallest fitting
    // column count wins (Go's scoring minimized aspect-ratio distance,
    // which kept small fleets in one honest narrow column — the thing the
    // redesign is for). Nothing fitting ⇒ widest grid, the viewport scrolls.
    let fits = |c: usize| -> Option<(usize, f32)> {
        let mut col_w = (avail - (c - 1) as f32 * COL_GAP) / c as f32;
        if col_w > COL_MAX_W {
            col_w = COL_MAX_W;
        }
        if col_w < COL_MIN_READABLE {
            return None; // unreadably narrow
        }
        let mut hgt = 0.0;
        for row in groups.chunks(c) {
            hgt += row.iter().copied().fold(0.0_f32, f32::max) + GRID_ROW_GAP;
        }
        hgt -= GRID_ROW_GAP;
        if hgt > grid_h {
            return None; // overflows the fixed grid area: unusable
        }
        Some((c, col_w))
    };
    let mut best = None;
    for c in (1..=max_cols).rev() {
        if let Some(pick) = fits(c) {
            best = Some(pick);
            break;
        }
    }
    let (cols, col_w) = best.unwrap_or((max_cols, {
        let mut col_w = (avail - (max_cols - 1) as f32 * COL_GAP) / max_cols as f32;
        if col_w > COL_MAX_W {
            col_w = COL_MAX_W;
        }
        col_w
    }));

    Metrics {
        cols,
        col_w,
        width,
        window_h,
        grid_h,
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// A COLORREF (0x00BBGGRR) as an opaque rune [`Color`], sRGB-decoded into
/// rune's linear-light working space (how `icon::color`'s bytes land on
/// screen).
fn col_of(c: u32) -> Color {
    Color {
        r: rune::color::srgb_decode((c & 0xFF) as f32 / 255.0),
        g: rune::color::srgb_decode(((c >> 8) & 0xFF) as f32 / 255.0),
        b: rune::color::srgb_decode(((c >> 16) & 0xFF) as f32 / 255.0),
        a: 1.0,
    }
}

/// The five-state panel code — the tray icon's own palette
/// (`icon::color`): blocked red, down amber, working yellow, waiting green,
/// idle gray. Panel and tray speak one visual language.
fn sev_color(sev: &str) -> Color {
    col_of(colorref(icon::color(sev)))
}

/// The kit the panel renders with, from three seed colours: the deep
/// neutral panel background, near-white text, and a blue UI accent (the
/// state code lives in [`icon::color`] — working is yellow now, not blue).
/// Status colours stay `icon::color`'s (see [`sev_color`]) — kit
/// tokens carry everything else (text ramp, borders, hover).
fn panel_kit() -> Kit {
    Kit::from_palette(PaletteSeed {
        bg: rune::color::hex("#1c1e22").expect("panel bg hex"),
        fg: rune::color::hex("#e7eaee").expect("panel fg hex"),
        accent: rune::color::hex("#3b82f6").expect("panel accent hex"),
    })
}

// ---------------------------------------------------------------------------
// Work area (flyout show() bottom-right anchoring)
// ---------------------------------------------------------------------------

/// The usable desktop rectangle the panel anchors to (`flyout.areaW/...`
/// from SPI_GETWORKAREA). Points are rune's logical units; the `_px` fields
/// are physical desktop pixels for [`AppHandle::set_position`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorkArea {
    pub x_px: i32,
    pub y_px: i32,
    pub right_px: i32,
    pub bottom_px: i32,
    pub w_pts: f32,
    pub h_pts: f32,
    /// Physical pixels per point (1 where positions are already logical —
    /// Wayland — or unknown).
    pub scale: f32,
}

impl WorkArea {
    /// Centered anchor in physical pixels for a `w_pts × h_pts` panel —
    /// the redesign dropped the tray-icon-corner anchoring: the window is
    /// a full surface now, so it opens where the eyes are.
    pub fn center_px(&self, w_pts: f32, h_pts: f32) -> (i32, i32) {
        let s = if self.scale > 0.01 { self.scale } else { 1.0 };
        let area_w = (self.right_px - self.x_px) as f32;
        let area_h = (self.bottom_px - self.y_px) as f32;
        let x = self.x_px + ((area_w - w_pts * s) / 2.0).round() as i32;
        let y = self.y_px + ((area_h - h_pts * s) / 2.0).round() as i32;
        (x.max(self.x_px), y.max(self.y_px))
    }
}

/// Detects the work area from rune's displays. On Windows the query goes
/// through `SPI_GETWORKAREA` (taskbar-excluded, like the Go panel); on
/// Linux it is best-effort: the primary display's bounds, minus no taskbar
/// (panel may sit under a dock/bar; documented deviation).
pub fn work_area(displays: &[Display]) -> WorkArea {
    let d = displays.iter().find(|d| d.primary).or_else(|| displays.first());
    let (x, y, w, h, scale, wayland) = match d {
        Some(d) => (
            d.position.0,
            d.position.1,
            d.size.0 as i32,
            d.size.1 as i32,
            d.scale as f32,
            matches!(d.native_id(), Some(NativeDisplayId::Wayland(_))),
        ),
        None => (0, 0, 1920, 1080, 1.0, false),
    };
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    #[cfg(target_os = "windows")]
    {
        // The Go panel queries the work area at every show; we detect once
        // per run (displays are fixed) and re-anchor on every layout.
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETWORKAREA,
        };
        let mut area = RECT::default();
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut area as *mut RECT as *mut core::ffi::c_void),
                windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if ok.is_ok() {
            x = area.left;
            y = area.top;
            w = area.right - area.left;
            h = area.bottom - area.top;
        }
    }
    // Wayland reports compositor-logical bounds; X11/Windows report physical
    // pixels, which divide by the scale into points.
    let s = if wayland {
        1.0
    } else if scale > 0.01 {
        scale
    } else {
        1.0
    };
    let (w_pts, h_pts) = if wayland {
        (w as f32, h as f32)
    } else {
        (w as f32 / s, h as f32 / s)
    };
    WorkArea {
        x_px: x,
        y_px: y,
        right_px: x + w,
        bottom_px: y + h,
        w_pts,
        h_pts,
        scale: s,
    }
}

// ---------------------------------------------------------------------------
// The rune panel window (examples/launcher pattern)
// ---------------------------------------------------------------------------

/// The agent a click focuses: what [`AgentHit`] carries to the CLI's
/// focus callback (`flyRow` host/sess/pane, `flyout.focusAgent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHit {
    pub host: String,
    pub sess: String,
    pub pane: String,
}

type FocusAgentFn = Arc<dyn Fn(&AgentHit) + Send + Sync>;

/// Thread-safe control handle for the panel: what the tray thread (and the
/// refresh loop) use to show, hide, wake and quit it. Cheap to clone.
#[derive(Clone)]
pub struct FlyControl {
    me: AppHandle<FlyState>,
    focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
}

impl FlyControl {
    /// Wires the agent-focus callback (`flyout.focusAgent`'s herdr call;
    /// the CLI injects the actual invocation).
    pub fn set_focus_agent(&self, f: impl Fn(&AgentHit) + Send + Sync + 'static) {
        *self.focus_agent.lock().unwrap() = Some(Arc::new(f));
    }

    /// Shows or hides the panel (tray left click, `flyout.toggle`).
    pub fn toggle(&self) {
        if self.me.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Shows the panel (`flyout.toggle`'s show arm). The window keeps its
    /// size and position across shows: the user owns the geometry.
    pub fn show(&self) {
        self.me.update(|s: &mut FlyState, tx: &mut Tx| {
            s.last_fp.clear(); // force a fresh layout commit on the next poll
            tx.scroll_to_y(CONTENT_KEY, 0.0); // Go show(): scroll = 0
        });
        // open() = show + activate (Go's SetForegroundWindow); on Wayland,
        // where apps can't place or focus windows, this is a plain show.
        self.me.open();
        self.me.wake(); // make poll size and anchor the panel at once
    }

    /// Shows the panel with a compositor activation token the tray handed
    /// over (Wayland `TrayEvent::Activate`), so the panel may take focus.
    pub fn activate(&self, token: impl Into<String>) {
        self.me.update(|s: &mut FlyState, tx: &mut Tx| {
            s.last_fp.clear();
            tx.scroll_to_y(CONTENT_KEY, 0.0);
        });
        self.me.activate(token);
        self.me.wake();
    }

    /// Hides the panel, retaining its state (`flyout.hide` → SW_HIDE).
    pub fn hide(&self) {
        self.me.hide();
    }

    /// Asks for a poll+repaint from any thread (`flyout.notify`).
    pub fn notify(&self) {
        self.me.wake();
    }

    /// Quits the panel's rune app (the tray's Quit).
    pub fn quit(&self) {
        self.me.quit();
    }

    /// Whether the panel is on screen (`AppHandle::is_visible`).
    pub fn is_visible(&self) -> bool {
        self.me.is_visible()
    }

    /// Lands the work area the run factory detected, before the first
    /// frame (`Tray::run` → `flyout::work_area`).
    pub fn apply_work_area(&self, area: WorkArea) {
        self.me.update(move |s: &mut FlyState, _| {
            s.area = area;
            s.geometry_set = false; // re-anchor with the real geometry
        });
    }
}

/// The flyout panel: owns the windowless rune [`App`] (until
/// [`Tray::run`][crate::tray::Tray::run] hands it to the event loop) plus a
/// [`FlyControl`] the tray thread talks to. Left click toggles, state-change
/// wakes refresh it, and agent clicks call the injected focus callback.
pub struct Flyout {
    ctl: FlyControl,
    app: Option<App<FlyState>>,
}

/// The rune [`WindowOptions`] for the panel: an integrated-chrome window —
/// borderless (the header draws its own maximize/close and drags
/// the window), opaque content under rounded corners, resizable, not
/// topmost — starting hidden until the tray shows it.
pub fn window_options() -> WindowOptions {
    WindowOptions {
        transparent: true,
        borderless: true,
        click_through: false,
        always_on_top: false,
        visible: false,
        position: None,
        physical_size: None,
        min_size: Some((560.0, 420.0)),
        max_size: None,
        resizable: true,
    }
}

impl Flyout {
    /// Builds the windowless panel app. The OS window itself is created by
    /// the event loop on the first [`FlyControl::show`] — rune window
    /// creation has loop affinity, which is what the Go `InitFlyout`
    /// "before Run" rule guarded.
    pub fn new(eng: Arc<engine::Engine>) -> Flyout {
        let focus_agent: Arc<Mutex<Option<FocusAgentFn>>> = Arc::new(Mutex::new(None));
        let eng2 = Arc::clone(&eng);
        let fa = Arc::clone(&focus_agent);
        // Title bar, taskbar and Alt+Tab carry the agent disc (a no-op off
        // Windows/macOS). Must precede `run` — windows opened later pick it up.
        let _ = rune::set_app_icon(crate::icon::render_png(crate::icon::color("working"), 256));
        let app = App::new(AppSpec {
            poll: Some(flyout_poll),
            ..AppSpec::new("Agent Notify", (1080.0, 648.0), flyout_init, flyout_view)
        })
        .windowless(); // no window at launch; open() creates and shows it
        let me = app.handle();
        let me_for_state = me.clone();
        let app = app.init(move || FlyState::new(eng2, me_for_state, fa));
        Flyout {
            ctl: FlyControl {
                me,
                focus_agent,
            },
            app: Some(app),
        }
    }

    /// The thread-safe control handle (clone freely).
    pub fn control(&self) -> FlyControl {
        self.ctl.clone()
    }

    /// Takes the app out for the event loop (`Tray::run`). Panics if the
    /// app was already taken: [`Flyout::new`] and this are a pair.
    pub(crate) fn into_app(mut self) -> App<FlyState> {
        self.app.take().expect("flyout app taken twice")
    }
}

/// Which room the panel shows: the fleet grid or one session full-width.
/// Sticky across shows; a session key that stops existing renders as `All`.
#[derive(Clone, Debug, PartialEq)]
enum Tab {
    All,
    Session(String),
}

/// How agent rows label their item: the herdr task title, or the herdr
/// tab name ("<space> - #<n>", part-colored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleMode {
    Task,
    Tab,
}

/// The panel's per-window state (`flyout` struct's window-thread fields).
/// `pub(crate)` so the tray can park the opaque [`App`] between init and
/// run; every field stays private.
pub(crate) struct FlyState {
    eng: Arc<engine::Engine>,
    me: AppHandle<FlyState>,
    /// The kit the panel renders with (`FlyState` owns it so the view fn
    /// stays a pure render of model + tokens).
    kit: Kit,
    /// Selected room (overview grid vs one session's full-width view).
    tab: Tab,
    /// How agent rows label their items (task title vs tab name).
    title_mode: TitleMode,
    /// Whether the maximize button currently reads "restore" (local view;
    /// rune exposes no getter, so an external Win+Up may desync the glyph).
    maximized: bool,
    focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    /// collapse state: session key -> user override; defaults live in
    /// `build_rows` (`flyout.openState`).
    open_state: HashMap<String, bool>,
    /// group key ("session/finished" | "session/resting") -> override
    /// (`flyout.restOpen`).
    rest_open: HashMap<String, bool>,
    area: WorkArea,
    /// whether poll already applied the opening geometry (size + centered
    /// position). After it, the user owns the window: no forced resizes.
    geometry_set: bool,
    /// the engine view the view fn renders; refreshed by poll.
    snapshot: View,
    /// fingerprint of the last rendered row model: skips view re-runs when
    /// a refresh tick found nothing new.
    last_fp: String,
}

impl FlyState {
    /// Collapses or expands every session and settled group in the fleet
    /// (the tab bar's two bulk buttons). Explicit overrides for everything,
    /// so the attention-open defaults stop mattering until toggled back.
    fn set_all(&mut self, open: bool) {
        let m = build_model(&self.snapshot, &self.open_state, &self.rest_open);
        for sec in &m.sessions {
            self.open_state.insert(sec.key.clone(), open);
            if let Some(g) = &sec.finished {
                self.rest_open.insert(g.key.clone(), open);
            }
            if let Some(g) = &sec.resting {
                self.rest_open.insert(g.key.clone(), open);
            }
        }
    }

    fn new(
        eng: Arc<engine::Engine>,
        me: AppHandle<FlyState>,
        focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    ) -> FlyState {
        FlyState {
            eng,
            me,
            kit: panel_kit(),
            tab: Tab::All,
            title_mode: load_title_mode(),
            maximized: false,
            focus_agent,
            open_state: HashMap::new(),
            rest_open: HashMap::new(),
            // Sane default until the run factory lands the real display
            // geometry (`Tray::run` sends it as the loop's first update).
            area: WorkArea::default(),
            geometry_set: false,
            snapshot: View::default(),
            last_fp: String::new(),
        }
    }
}

/// The app starts windowless: state is built by [`Flyout::new`]'s
/// `App::init` closure (the launcher pattern).
fn flyout_init() -> FlyState {
    unreachable!("FlyState is built by App::init")
}

/// View refresh + opening geometry (`WM_TIMER` refresh, `flyout.paint`'s
/// re-size, once). `now` is rune's app clock.
fn flyout_poll(s: &mut FlyState, _now: f64, tx: &mut Tx) -> bool {
    if !s.me.is_visible() {
        // Hidden: the Go panel kills its timers on hide. Stop the cadence
        // and idle until the tray wakes us for the next show.
        return false;
    }
    // The engine view is read on the UI thread; Engine::view takes a
    // short-held mutex, so this is safe (flyout_windows.go header note).
    s.snapshot = s.eng.view();
    let model = build_model(&s.snapshot, &s.open_state, &s.rest_open);
    let fp = format!("|mode:{:?}{}", s.title_mode, model_fingerprint(&model));
    let changed = fp != s.last_fp;
    s.last_fp = fp;

    let mt = metrics(&model, s.area.w_pts, s.area.h_pts);
    // Opening geometry, exactly once: size to the standard panel and
    // center it. Afterwards the user owns the window — a regular window
    // is not yanked around by its own app.
    if !s.geometry_set {
        tx.resize_window_instant((mt.width, mt.window_h));
        let pos = s.area.center_px(mt.width, mt.window_h);
        s.me.set_position(pos);
        s.geometry_set = true;
    }
    tx.wake_after(REFRESH_EVERY);
    changed
}

/// The scroll viewport's key (`Tx::scroll_to_y` reset on show).
const CONTENT_KEY: &str = "flyout-content";

/// Builds the panel (`flyout.paint`): a rounded, bordered, dark window;
/// the fleet header, the tab bar (overview + one room per session), then
/// the room body — session grid on "All", that session full-width
/// otherwise — wheel-scrollable inside the fixed window when needed.
fn flyout_view(s: &FlyState, _cx: &mut Cx<FlyState>) -> El {
    let model = build_model(&s.snapshot, &s.open_state, &s.rest_open);
    let k = &s.kit;

    let mt = metrics(&model, s.area.w_pts, s.area.h_pts);
    let inner_w = mt.width - 2.0 * FLY_PAD;

    // Tab bar: the overview grid ("All") plus one full-width room per
    // session. Labels repeat the host only when a name is ambiguous
    // across hosts ("default" vs "windows/default").
    let mut labels = vec!["All".to_string()];
    labels.extend(tab_labels(&model));
    let selected = match &s.tab {
        Tab::All => Some(0),
        Tab::Session(key) => model
            .sessions
            .iter()
            .position(|sec| &sec.key == key)
            .map(|i| i + 1),
    };
    let tab_bar = if model.sessions.is_empty() {
        None
    } else {
        Some(
            SegmentedControl::new(&labels, selected)
                .fit()
                .label("Room")
                .on_pick::<FlyState>(move |st, i, _| {
                    st.tab = if i == 0 {
                        Tab::All
                    } else {
                        // Resolve through the CURRENT model, so a stale
                        // index cannot pin a vanished session.
                        let m = build_model(&st.snapshot, &st.open_state, &st.rest_open);
                        m.sessions
                            .get(i - 1)
                            .map(|sec| Tab::Session(sec.key.clone()))
                            .unwrap_or(Tab::All)
                    };
                })
                .build(k),
        )
    };

    // Which room renders: the picked session, or the overview when the
    // tab is All / the session went away / the fleet is empty.
    let room = model.sessions.iter().position(|sec| {
        matches!(&s.tab, Tab::Session(key) if &sec.key == key)
    });

    let mut body = col().key("panel-body").w_full();
    if model.empty {
        body = body.child(
            row()
                .key("empty")
                .w_full()
                .h(EST_TRIGGER_H)
                .center_y()
                .child(
                    text("No herdr sessions yet — they show up here when herdr starts.")
                        .font(k.ty.ui_compact)
                        .color(k.c.text_muted)
                        .truncate(),
                ),
        );
    } else if let Some(idx) = room {
        // One session, full width: the same section tree, room to breathe
        // (`session_section` fills whatever width its parent gives it).
        body = body.child(session_section(&model.sessions[idx], idx, s.title_mode, k));
    } else {
        // Sessions fill a columns × rows grid (`flyout.layout`'s grid);
        // rune lays the cells out for real (row-major, each row as tall
        // as its tallest session). The wheel viewport below matters only
        // when the fleet outgrows the fixed grid area.
        let mut grid = col()
            .key("session-grid")
            .w_full()
            .equal_columns(mt.cols)
            .gaps(GRID_ROW_GAP, COL_GAP);
        for (i, sec) in model.sessions.iter().enumerate() {
            grid = grid.child(session_section(sec, i, s.title_mode, k));
        }
        body = body.child(grid);
    }

    col()
        .key("panel")
        .w_full()
        .h_full()
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_muted)
        .radius(14.0)
        .clip_rounded(14.0)
        .padding(FLY_PAD)
        .gap(16.0) // header / divider / tabs / body need air between them
        .child(header_el(&model, inner_w, s.maximized, k))
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(
            row()
                .key("tab-bar")
                .w_full()
                .h(TAB_BAR_H)
                .center_y()
                .gap(8.0)
                .child(tab_bar.unwrap_or_else(|| el()))
                .child(spacer())
                // bulk fold controls: a visible pill grouping the pair
                .child(
                    row()
                        .key("fold-group")
                        .gap(2.0)
                        .padding(2.0)
                        .fill(k.alpha(k.c.text_base, 0.05))
                        .radius(k.radius.sm)
                        .child(fold_btn(
                            "fold-collapse",
                            "collapse",
                            k.glyph.chevron_up,
                            k,
                            |s: &mut FlyState, _| s.set_all(false),
                        ))
                        .child(fold_btn(
                            "fold-expand",
                            "expand",
                            k.glyph.chevron_down,
                            k,
                            |s: &mut FlyState, _| s.set_all(true),
                        )),
                )
                .child(titles_btn(s.title_mode, k)),
        )
        .child(
            col()
                .key(CONTENT_KEY)
                .w_full()
                .h_full()
                .gap(14.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(body),
        )
}

/// Tab labels: the session name, host-prefixed only when the name is
/// ambiguous across hosts. Index 0 is the overview ("All"); callers
/// prepend it.
fn tab_labels(m: &PanelModel) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in &m.sessions {
        *counts.entry(s.name.as_str()).or_default() += 1;
    }
    m.sessions
        .iter()
        .map(|s| {
            if counts[s.name.as_str()] > 1 {
                format!("{}/{}", s.host, s.name)
            } else {
                s.name.clone()
            }
        })
        .collect()
}

/// A fold-group push button: glyph + short label ("collapse" / "expand"),
/// kit hover, muted colors.
fn fold_btn(
    key: &'static str,
    label: &str,
    g: &'static rune::Icon,
    k: &Kit,
    press: impl Fn(&mut FlyState, &mut Tx) + 'static,
) -> El {
    let t = row()
        .key(key)
        .h(24.0)
        .px(8.0)
        .center_y()
        .gap(6.0)
        .radius(6.0)
        .child(icon(g, 13.0).color(k.c.icon_muted))
        .child(
            text(label.to_owned())
                .font(k.ty.ui_compact)
                .color(k.c.text_muted),
        );
    k.interactive(t).on_press_tx(move |s: &mut FlyState, tx: &mut Tx| press(s, tx))
}

/// The title-mode toggle: "Aa" as its character, the active mode spelled
/// out beside it in accent when tab names are on.
fn titles_btn(mode: TitleMode, k: &Kit) -> El {
    let state = match mode {
        TitleMode::Task => "task",
        TitleMode::Tab => "tab",
    };
    let state_color = match mode {
        TitleMode::Task => k.c.text_muted,
        TitleMode::Tab => k.c.text_accent,
    };
    let t = row()
        .key("btn-titles")
        .h(26.0)
        .px(10.0)
        .center_y()
        .gap(6.0)
        .fill(k.alpha(k.c.text_base, 0.05))
        .radius(k.radius.sm)
        .child(text("Aa").font(k.ty.ui_compact).color(k.c.text_muted))
        .child(text(state).font(k.ty.ui_compact).color(state_color));
    k.interactive(t).on_press_tx(|s: &mut FlyState, _tx: &mut Tx| {
        s.title_mode = match s.title_mode {
            TitleMode::Task => TitleMode::Tab,
            TitleMode::Tab => TitleMode::Task,
        };
        save_title_mode(s.title_mode);
    })
}

/// The header: fleet dot + name (+ version stamp), the count chips — which
/// double as the legend for the row dots — and the proportional state bar.
/// `maximized` switches the middle button's glyph to restore.
fn header_el(m: &PanelModel, inner_w: f32, maximized: bool, k: &Kit) -> El {
    let mut c = col().w_full().gap(8.0);

    // Integrated chrome: the header drags the window; the two buttons
    // pinned at the far right are its maximize / close
    // (`El::no_window_drag` keeps presses off the drag region). Close
    // hides — the app keeps running and the tray reopens the window with
    // its geometry. The maximize glyph tracks our own toggles (rune has
    // no getter; an external Win+Up may desync it).
    let controls = {
        // no minimize button: this is a status surface, not an app to
        // park — maximize and close cover it
        let max_btn = {
            // maximize: one square; restore: square-in-square
            let glyph = if maximized {
                el().size(10.0, 10.0)
                    .border(1.4, k.c.icon_muted)
                    .child(el().size(5.0, 5.0).border(1.2, k.c.icon_muted))
            } else {
                el().size(9.0, 9.0).radius(2.0).border(1.4, k.c.icon_muted)
            };
            let t = row()
                .key("win-max")
                .w(30.0)
                .h(24.0)
                .center()
                .radius(6.0)
                .child(glyph);
            k.interactive(t).on_press_tx(|s: &mut FlyState, tx: &mut Tx| {
                s.maximized = !s.maximized;
                tx.toggle_maximized();
            })
        };
        let close_btn = {
            let t = row()
                .key("win-close")
                .w(30.0)
                .h(24.0)
                .center()
                .radius(6.0)
                .child(icon(k.glyph.close, 12.0).color(k.c.icon_muted));
            k.interactive(t).on_press_tx(|s: &mut FlyState, _tx: &mut Tx| s.me.hide())
        };
        row().gap(2.0).no_window_drag().child(max_btn).child(close_btn)
    };

    let mut title = row()
        .key("header-title")
        .window_drag()
        .w_full()
        .h(EST_TITLE_H)
        .center_y()
        .gap(8.0)
        .child(el().size(11.0, 11.0).radius(5.5).fill(sev_color(m.fleet_sev)))
        .child(
            text("Agent Notify")
                .font(TextStyle {
                    size: k.ty.ui_medium.size + 1.0,
                    ..k.ty.ui_medium
                })
                .color(k.c.text_base),
        )
        .child(spacer());
    if crate::buildinfo::is_release() {
        // stamp the build so a stale binary is visible at a glance
        title = title.child(
            text(crate::buildinfo::VERSION)
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    title = title.child(controls);
    c = c.child(title);

    if !m.chips.is_empty() {
        let mut chips = row()
            .key("header-chips")
            .w_full()
            .h(EST_CHIPS_H)
            .center_y()
            .gap(6.0);
        for ch in &m.chips {
            chips = chips.child(chip_el(ch, k));
        }
        c = c.child(chips);
    }

    if !m.bar.is_empty() {
        let total: f32 = m.bar.iter().map(|s| s.weight).sum();
        // short of the row edges: the bar reads as its own element, not a
        // full-bleed rule
        let bar_w = inner_w - 16.0;
        // integer pixel spans — the last segment absorbs the rounding (the
        // Go bar's trick) so adjacent segments leave no seams
        let mut widths: Vec<f32> = Vec::with_capacity(m.bar.len());
        let mut used = 0.0f32;
        for (i, seg) in m.bar.iter().enumerate() {
            let w = if i + 1 == m.bar.len() {
                (bar_w - used).max(0.0)
            } else {
                (seg.weight / total * bar_w).floor().max(1.0)
            };
            widths.push(w);
            used += w;
        }
        let mut bar = row().w_full();
        for (seg, w) in m.bar.iter().zip(&widths) {
            bar = bar.child(el().w(*w).h(EST_BAR_H).fill(sev_color(seg.sev)));
        }
        c = c.child(
            el()
                .key("state-bar")
                .w_full()
                .h(EST_BAR_H)
                .mx(8.0)
                .radius(3.0)
                .clip_rounded(3.0)
                .child(bar),
        );
    }
    c
}

/// One count chip: state-tinted pill with a state-colored label.
fn chip_el(c: &Chip, k: &Kit) -> El {
    let color = sev_color(c.sev);
    row()
        .key(format!("chip:{}", c.label))
        .h(20.0)
        .radius(10.0)
        .fill(k.alpha(color, 0.14))
        .pl(8.0)
        .center_y()
        .child(text(c.label.clone()).font(k.ty.small_medium).color(color))
        .child(el().w(8.0))
}

/// One session: a raised card holding the collapsible trigger plus, when
/// open, its attention blocks and settled groups (`flyout.paint`'s session
/// group, restacked). The card fill alone separates blocks and groups —
/// no border, per the 2026-11-12 pass; `min_w(0)` floors the grid cell: a
/// grid column sizes to its widest child's min-content, and without the
/// floor a long unbreakable title pushes the column past the panel edge.
fn session_section(sec: &SessionSection, idx: usize, mode: TitleMode, k: &Kit) -> El {
    let mut c = col()
        .key(format!("section:{idx}"))
        .min_w(0.0)
        .w_full()
        .gap(6.0)
        .fill(k.alpha(k.c.text_base, 0.03))
        .radius(12.0)
        .padding(14.0);
    c = c.child(session_trigger(sec, k));
    if !sec.open {
        return c;
    }
    // The attention rows run flat on the card; every settled group drops
    // into a recessed sub-tile with a severity stripe on its left edge —
    // finished (green) and resting (gray) read as zones apart from what is
    // still running.
    for (j, a) in sec.active.iter().enumerate() {
        c = c.child(agent_row(a, format!("agent:{idx}:{j}"), mode, k));
    }
    for g in [sec.finished.as_ref(), sec.resting.as_ref()]
        .into_iter()
        .flatten()
    {
        let mut body = col().w_full().gap(2.0).py(4.0);
        body = body.child(group_trigger(g, k));
        if g.open {
            for (j, a) in g.agents.iter().enumerate() {
                body = body.child(compact_row(a, format!("compact:{idx}:{j}"), mode, k));
            }
        }
        let tile = row()
            .key(format!("settled:{idx}:{}", g.key))
            .min_w(0.0)
            .w_full()
            .radius(6.0)
            .clip_rounded(6.0)
            .fill(Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.16,
            })
            .child(el().w(3.0).fill(sev_color(g.sev)))
            .child(body);
        c = c.child(tile);
    }
    c
}

/// The session trigger: kit disclosure glyph, session name, host, offline
/// flag, agent count; kit hover and a11y button role (`rowSession`'s band).
fn session_trigger(sec: &SessionSection, k: &Kit) -> El {
    let glyph = if sec.open {
        k.glyph.triangle_down
    } else {
        k.glyph.triangle_right
    };
    let key = sec.key.clone();
    let open = sec.open;
    let settled = sec.active.is_empty();
    let mut t = row()
        .key(format!("trigger:{}", sec.key))
        .role("button")
        .label(format!("session {} on {}", sec.name, sec.host))
        .expanded(open)
        .focusable(true)
        .focus_ring(2.0, 0.0, k.c.border_focus)
        .w_full()
        .h(EST_TRIGGER_H)
        .center_y()
        .gap(8.0)
        .pl(6.0)
        .pr(8.0)
        .fill(k.alpha(k.c.text_base, 0.04))
        .radius(k.radius.sm)
        .child(
            icon(glyph, 12.0).color(if sec.open {
                k.c.icon_base
            } else {
                k.c.icon_weaker
            }),
        )
        .child(
            text(sec.name.clone())
                .font(TextStyle {
                    size: k.ty.ui_medium.size + 1.0,
                    ..k.ty.ui_medium
                })
                .color(if settled && !sec.open {
                    k.c.text_muted
                } else {
                    k.c.text_base
                })
                .truncate(),
        );
    if !sec.host.is_empty() {
        t = t.child(
            text(sec.host.clone())
                .font(k.ty.small)
                .color(k.c.icon_weaker)
                .truncate(),
        );
    }
    if !sec.up {
        t = t.child(text("offline").font(k.ty.small_medium).color(sev_color("down")));
    }
    t = t.child(spacer()).child(
        text(format!(
            "{} agent{}",
            sec.total,
            if sec.total == 1 { "" } else { "s" }
        ))
        .font(k.ty.small)
        .color(k.c.text_faint),
    );
    k.interactive(t).on_press_tx(move |s: &mut FlyState, _| {
        let v = !open;
        s.open_state.insert(key.clone(), v);
    })
}

/// The two-line attention block: state dot, task title over the
/// "runner · project" caption. Only blocked rows get a tinted background —
/// the alarm shouts, the rest whispers (`rowAgent` drawing, redesigned).
/// `mode` picks the label: herdr's task title, or the colored
/// "<space> - #<n>" tab name.
fn agent_row(a: &AgentItem, key: String, mode: TitleMode, k: &Kit) -> El {
    let hit = (a.host.clone(), a.sess.clone(), a.pane.clone());
    let color = sev_color(a.sev);
    let mut blk = col().key(key).w_full().radius(k.radius.sm);
    if a.sev == "blocked" {
        blk = blk.fill(k.alpha(color, 0.10));
    }
    let mut meta = row()
        .w_full()
        .h(20.0)
        .center_y()
        .gap(6.0)
        .pl(30.0) // dot column (9px dot + 12px gap) + the 10px row inset
        .child(
            text(a.meta.clone())
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .truncate(),
        );
    if a.focused {
        meta = meta.child(text("✱").font(k.ty.caption()).color(sev_color("down")));
    }
    let title_font = TextStyle {
        size: k.ty.ui.size + 1.0,
        ..k.ty.ui
    };
    let title = match mode {
        TitleMode::Task => row()
            .w_full()
            .gap(4.0)
            .child(
                text(a.title.clone())
                    .font(title_font)
                    .color(k.c.text_base)
                    .truncate(),
            ),
        TitleMode::Tab if !a.project.is_empty() => {
            let (space, tab) = title_parts(a);
            row()
                .w_full()
                .gap(4.0)
                .child(
                    text(space)
                        .font(title_font)
                        .color(k.c.text_muted)
                        .truncate(),
                )
                .child(
                    text(format!("- {tab}"))
                        .font(title_font)
                        .color(k.c.text_accent)
                        .truncate(),
                )
        }
        TitleMode::Tab => row()
            .w_full()
            .gap(4.0)
            .child(
                text(a.title.clone())
                    .font(title_font)
                    .color(k.c.text_base)
                    .truncate(),
            ),
    };
    blk = blk
        .child(
            row()
                .w_full()
                .h(30.0)
                .center_y()
                .gap(12.0)
                .pl(10.0)
                .child(el().size(9.0, 9.0).radius(4.5).fill(color))
                .child(title)
                .child(el().w(6.0)),
        )
        .child(meta);
    k.interactive(blk).on_press_tx(move |s: &mut FlyState, tx: &mut Tx| {
        focus_agent(s, tx, &hit.0, &hit.1, &hit.2);
    })
}

/// The settled-agents group trigger ("N finished" / "N resting"): a ghost
/// collapsible line (`appendGroup`'s group row).
fn group_trigger(g: &GroupItem, k: &Kit) -> El {
    let glyph = if g.open {
        k.glyph.triangle_down
    } else {
        k.glyph.triangle_right
    };
    let key = g.key.clone();
    let open = g.open;
    let t = row()
        .key(format!("group:{}", g.key))
        .role("button")
        .label(format!("{} {}", g.agents.len(), g.label))
        .expanded(open)
        .focusable(true)
        .focus_ring(2.0, 0.0, k.c.border_focus)
        .w_full()
        .h(EST_GROUP_H)
        .center_y()
        .gap(8.0)
        .pl(10.0) // nested rows sit inside their group sub-tile
        .pr(8.0)
        .radius(k.radius.sm)
        .child(icon(glyph, 10.0).color(k.c.icon_weaker))
        .child(el().size(7.0, 7.0).radius(3.5).fill(sev_color(g.sev)))
        .child(
            text(format!("{} {}", g.agents.len(), g.label))
                .font(k.ty.ui_compact)
                .color(k.c.text_muted),
        );
    k.interactive(t).on_press_tx(move |s: &mut FlyState, _| {
        let v = !open;
        s.rest_open.insert(key.clone(), v);
    })
}

/// One settled agent on a single line, inside an expanded group
/// (the `AgentCompact` rows).
fn compact_row(a: &AgentItem, key: String, mode: TitleMode, k: &Kit) -> El {
    let hit = (a.host.clone(), a.sess.clone(), a.pane.clone());
    let tab_font = TextStyle {
        size: k.ty.ui_compact.size + 1.0,
        ..k.ty.ui_compact
    };
    let label: El = match mode {
        TitleMode::Task => text(a.title.clone())
            .font(tab_font)
            .color(k.c.text_muted)
            .truncate(),
        TitleMode::Tab if !a.project.is_empty() => {
            let (space, tab) = title_parts(a);
            row()
                .gap(4.0)
                .child(text(space).font(tab_font).color(k.c.text_faint).truncate())
                .child(
                    text(format!("- {tab}"))
                        .font(tab_font)
                        .color(k.c.text_accent)
                        .truncate(),
                )
        }
        TitleMode::Tab => text(a.title.clone())
            .font(tab_font)
            .color(k.c.text_muted)
            .truncate(),
    };
    let t = row()
        .key(key)
        .w_full()
        .h(EST_COMPACT_H)
        .center_y()
        .gap(12.0)
        .pl(26.0) // stripe + group indent + dot column
        .pr(8.0)
        .radius(k.radius.sm)
        .child(el().size(8.0, 8.0).radius(4.0).fill(sev_color(a.sev)))
        .child(label);
    k.interactive(t).on_press_tx(move |s: &mut FlyState, tx: &mut Tx| {
        focus_agent(s, tx, &hit.0, &hit.1, &hit.2);
    })
}

/// The tab-name title's two parts ("<space>", "<tab label>"): the project
/// and the tab's display label ("#", "#test" — herdr's `tabs` array). When
/// no label was joined (older herdr), the pane id degrades to "#<n>".
fn title_parts(a: &AgentItem) -> (String, String) {
    let tab = if a.tab.is_empty() {
        let n = a
            .pane
            .rsplit(':')
            .next()
            .unwrap_or("")
            .trim_start_matches('p');
        if n.is_empty() {
            a.pane.clone()
        } else {
            format!("#{n}")
        }
    } else {
        a.tab.clone()
    };
    (or_dash(&a.project), tab)
}

/// Agent click: run the injected focus callback off the UI thread
/// (`flyout.focusAgent`'s `go func`). The panel **stays open** — the user
/// keeps the fleet overview while the client jumps to the agent; only the
/// ✕ button hides the window.
fn focus_agent(s: &mut FlyState, _tx: &mut Tx, host: &str, sess: &str, pane: &str) {
    if pane.is_empty() {
        return;
    }
    let hit = AgentHit {
        host: host.to_string(),
        sess: sess.to_string(),
        pane: pane.to_string(),
    };
    let f = s.focus_agent.lock().unwrap().clone();
    if let Some(f) = f {
        std::thread::spawn(move || f(&hit));
    }
}

/// Content fingerprint of a panel model: identical output ⇒ identical
/// pixels, skip the view re-run (the flyout counterpart of the tray's
/// `fingerprint`). Includes the effective open states, so a 1 s refresh
/// after a collapse still lands.
fn model_fingerprint(m: &PanelModel) -> String {
    let mut b = String::new();
    b.push_str(&format!("|fleet:{}|empty:{}", m.fleet_sev, m.empty));
    for c in &m.chips {
        b.push_str(&format!("|chip:{}:{}", c.sev, c.label));
    }
    for s in &m.bar {
        b.push_str(&format!("|bar:{}:{:.3}", s.sev, s.weight));
    }
    for sec in &m.sessions {
        b.push_str(&format!(
            "|sec:{key}:{total}:up={up}:open={open}",
            key = sec.key,
            total = sec.total,
            up = sec.up,
            open = sec.open,
        ));
        for a in &sec.active {
            b.push_str(&format!(
                "|a:{pane}:{sev}:f={focused}:{title}",
                pane = a.pane,
                sev = a.sev,
                focused = a.focused,
                title = a.title,
            ));
        }
        for g in [&sec.finished, &sec.resting].into_iter().flatten() {
            b.push_str(&format!(
                "|g:{key}:open={open}",
                key = g.key,
                open = g.open,
            ));
            for a in &g.agents {
                b.push_str(&format!("|c:{}:{}", a.pane, a.title));
            }
        }
    }
    b
}

// ---------------------------------------------------------------------------
// Tests — the panel model (redesign; replaces the flyout_rows_test.go port)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, status: &str, title: &str) -> engine::AgentView {
        engine::AgentView {
            name: name.into(),
            status: status.into(),
            title: title.into(),
            pane_id: format!("p:{name}:{title}"),
            ..engine::AgentView::default()
        }
    }

    /// Four sessions covering every bucket: attention (working + latched
    /// done + resting), all-resting, finished-only, and offline.
    fn view() -> View {
        View {
            sessions: vec![
                engine::SessionView {
                    host: "wsl".into(),
                    name: "default".into(),
                    up: true,
                    agents: vec![
                        agent("pi", "working", "Refine shapes"),
                        agent("omp", "idle", "old run"),
                        agent("omp", "done", "finished task"),
                        engine::AgentView {
                            name: "pi".into(),
                            status: "done".into(),
                            wants: true,
                            title: "needs you".into(),
                            pane_id: "p:needsyou".into(),
                            ..engine::AgentView::default()
                        },
                    ],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "quiet".into(),
                    up: true,
                    agents: vec![
                        agent("pi", "idle", "resting one"),
                        agent("omp", "idle", "resting two"),
                    ],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "justdone".into(),
                    up: true,
                    agents: vec![engine::AgentView {
                        name: "pi".into(),
                        status: "done".into(),
                        wants: true,
                        title: "freshly finished".into(),
                        pane_id: "p:fresh".into(),
                        ..engine::AgentView::default()
                    }],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "down".into(),
                    up: false,
                    agents: vec![agent("pi", "idle", "gone")],
                },
            ],
        }
    }

    fn model() -> PanelModel {
        build_model(&view(), &HashMap::new(), &HashMap::new())
    }

    fn sec<'a>(m: &'a PanelModel, key: &str) -> &'a SessionSection {
        m.sessions
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("no section {key:?} in {m:?}"))
    }

    #[test]
    fn sessions_sort_by_attention_then_name() {
        let m = model();
        let keys: Vec<&str> = m.sessions.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            ["wsl/default", "wsl/justdone", "wsl/quiet", "wsl/down"],
            "attention first, then settled sessions alphabetical, offline last"
        );
    }

    #[test]
    fn agents_sort_by_attention_within_session() {
        let m = model();
        let d = sec(&m, "wsl/default");
        assert_eq!(d.active.len(), 1, "only the working agent is active");
        assert_eq!(d.active[0].title, "Refine shapes");
        let fin = d.finished.as_ref().expect("finished group exists");
        assert_eq!(fin.label, "finished");
        assert_eq!(fin.agents.len(), 1);
        let rest = d.resting.as_ref().expect("resting group exists");
        let titles: Vec<&str> = rest.agents.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(
            titles,
            ["finished task", "old run"],
            "resting agents sorted by title"
        );
    }

    #[test]
    fn defaults_open_with_attention_closed_without() {
        let m = model();
        assert!(sec(&m, "wsl/default").open, "attention session defaults open");
        assert!(
            !sec(&m, "wsl/quiet").open,
            "all-resting session defaults closed"
        );
        assert!(
            !sec(&m, "wsl/justdone").open,
            "finished-only session defaults closed"
        );
        assert!(!sec(&m, "wsl/down").open, "offline session defaults closed");
    }

    #[test]
    fn open_state_overrides_flip_defaults() {
        let open_state = HashMap::from([
            ("wsl/quiet".to_string(), true),
            ("wsl/default".to_string(), false),
        ]);
        let m = build_model(&view(), &open_state, &HashMap::new());
        assert!(sec(&m, "wsl/quiet").open, "override expands the quiet session");
        assert!(
            !sec(&m, "wsl/default").open,
            "override collapses the active session"
        );
    }

    #[test]
    fn group_overrides_flip_defaults() {
        let rest_open = HashMap::from([
            ("wsl/default/finished".to_string(), true),
            ("wsl/default/resting".to_string(), true),
        ]);
        let m = build_model(&view(), &HashMap::new(), &rest_open);
        let d = sec(&m, "wsl/default");
        let fin = d.finished.as_ref().expect("finished group exists");
        let rest = d.resting.as_ref().expect("resting group exists");
        assert!(
            fin.open && fin.agents.len() == 1,
            "finished group expanded with 1 agent"
        );
        assert!(
            rest.open && rest.agents.len() == 2,
            "resting group expanded with 2 agents"
        );
        assert!(
            !sec(&m, "wsl/quiet")
                .resting
                .as_ref()
                .expect("quiet resting group")
                .open,
            "other sessions' groups default closed"
        );
    }

    #[test]
    fn chips_are_nonzero_and_severity_ordered() {
        let m = model();
        let labels: Vec<&str> = m.chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["1 working", "2 waiting", "4 resting"],
            "nonzero counts in severity order (no blocked in the fixture)"
        );
    }

    #[test]
    fn state_bar_weights_cover_all_states() {
        let m = model();
        let total: f32 = m.bar.iter().map(|s| s.weight).sum();
        assert_eq!(total, 8.0, "7 agents in up sessions + 1 down session");
        let sevs: Vec<&str> = m.bar.iter().map(|s| s.sev).collect();
        assert_eq!(
            sevs,
            ["down", "working", "waiting", "idle"],
            "bar order blocked, down, working, waiting, idle — nonzero only"
        );
    }

    #[test]
    fn offline_sessions_flag_and_sink() {
        let m = model();
        let d = sec(&m, "wsl/down");
        assert!(!d.up, "the session keeps its offline flag");
        assert_eq!(
            m.sessions.last().map(|s| s.key.as_str()),
            Some("wsl/down"),
            "offline sessions sink to the bottom"
        );
    }

    #[test]
    fn empty_view_is_the_invitation() {
        let m = build_model(&View::default(), &HashMap::new(), &HashMap::new());
        assert!(m.empty);
        assert!(m.sessions.is_empty());
        assert!(m.chips.is_empty());
    }

    #[test]
    fn titles_collapse_whitespace_and_keep_glyphs() {
        assert_eq!(flat_ws("a\n\tb  c"), "a b c");
        assert_eq!(flat_ws("π > README: basics"), "π > README: basics");
        assert_eq!(
            flat_ws("README ⚙\u{FE0F}"),
            "README ⚙\u{FE0F}",
            "rune renders emoji; no stripping"
        );
        assert_eq!(flat_ws("日本語 タスク"), "日本語 タスク");
    }

    /// The smoke render's data: three sessions with every state, long
    /// titles, a focused agent and an offline session — pixels over words.
    fn smoke_view() -> View {
        View {
            sessions: vec![
                engine::SessionView {
                    host: "wsl".into(),
                    name: "default".into(),
                    up: true,
                    agents: vec![
                        engine::AgentView {
                            name: "pi".into(),
                            status: "working".into(),
                            title: "ART: Fix snapshot corruption and policy drift in the watcher"
                                .into(),
                            project: "policy".into(),
                            pane_id: "w1:p1".into(),
                            focused: true,
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "igno".into(),
                            status: "blocked".into(),
                            title: "Waiting on user: confirm the .amp gitignore addition".into(),
                            project: "repo".into(),
                            pane_id: "w1:p2".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "dpb".into(),
                            status: "working".into(),
                            title: "Revive webpuy and comunit".into(),
                            project: "web".into(),
                            pane_id: "w1:p3".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "omp".into(),
                            status: "done".into(),
                            wants: true,
                            title: "Old run".into(),
                            pane_id: "w1:p4".into(),
                            ..engine::AgentView::default()
                        },
                        agent("ka", "idle", "Resting between runs"),
                        agent("zed", "idle", "Idle"),
                        agent("mia", "idle", "Idle"),
                    ],
                },
                engine::SessionView {
                    host: "windows".into(),
                    name: "win".into(),
                    up: true,
                    agents: vec![
                        engine::AgentView {
                            name: "nt".into(),
                            status: "working".into(),
                            title: "Ship the release notes".into(),
                            project: "docs".into(),
                            pane_id: "w2:p1".into(),
                            ..engine::AgentView::default()
                        },
                        agent("svc", "blocked", "Blocked on credentials"),
                    ],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "remote".into(),
                    up: false,
                    agents: vec![agent("gone", "idle", "was here once")],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "pods".into(),
                    up: true,
                    agents: vec![
                        engine::AgentView {
                            name: "cg".into(),
                            status: "working".into(),
                            title: "AUDIT: Auditar portafolio de proyectos".into(),
                            project: "audit".into(),
                            pane_id: "w5:p1".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "viv".into(),
                            status: "blocked".into(),
                            title: "VIVI: Explorar repo vivienda".into(),
                            project: "vivi".into(),
                            pane_id: "w6:p1".into(),
                            ..engine::AgentView::default()
                        },
                    ],
                },
                engine::SessionView {
                    host: "windows".into(),
                    name: "lab".into(),
                    up: true,
                    agents: vec![agent("px", "working", "PROV: Check provider compat")],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "quiet".into(),
                    up: true,
                    agents: vec![agent("rest", "idle", "Resting between runs")],
                },
                engine::SessionView {
                    host: "windows".into(),
                    name: "bench".into(),
                    up: true,
                    agents: vec![agent("mk", "idle", "Idle"), agent("jo", "idle", "Idle")],
                },
            ],
        }
    }

    /// Grid sizing on the overview tab: the fixed 720 pt window, the
    /// tallest column count that fits the fixed grid area.
    #[test]
    fn grid_spreads_sessions_into_columns_on_wide_areas() {
        // Six busy sessions: one column cannot fit the fixed grid area,
        // so the sizing must spread them sideways.
        let sessions = (0..6)
            .map(|i| engine::SessionView {
                host: "wsl".into(),
                name: format!("s{i}"),
                up: true,
                agents: (0..3)
                    .map(|j| agent(&format!("a{j}"), "working", "Do things"))
                    .collect(),
            })
            .collect();
        let m = build_model(&View { sessions }, &HashMap::new(), &HashMap::new());
        let mt = metrics(&m, 1920.0, 900.0);
        assert_eq!(mt.width, 1080.0, "fixed window width: {mt:?}");
        assert_eq!(mt.window_h, 648.0, "72 % of the work area: {mt:?}");
        assert!(mt.cols >= 2, "busy fleet spreads into columns: {mt:?}");
        assert!((COL_MIN_READABLE..=COL_MAX_W).contains(&mt.col_w));
    }

    #[test]
    fn grid_collapses_to_one_column_on_narrow_areas() {
        let m = model();
        let mt = metrics(&m, 360.0, 800.0);
        assert_eq!(mt.cols, 1, "a narrow area holds one column: {mt:?}");
        assert_eq!(mt.width, 320.0, "clamped to the 360 pt work area: {mt:?}");
        assert_eq!(mt.window_h, 576.0, "72 % of 800: {mt:?}");
    }

    #[test]
    fn grid_falls_back_to_widest_grid_when_nothing_fits() {
        // Twelve open sessions of eight working agents each: no column
        // count fits the fixed grid area, so the widest grid wins and the
        // viewport scrolls (`flyout.layout`'s `bestW == 0` branch).
        let sessions = (0..12)
            .map(|i| engine::SessionView {
                host: "wsl".into(),
                name: format!("s{i}"),
                up: true,
                agents: (0..8)
                    .map(|j| agent(&format!("a{j}"), "working", "Do things"))
                    .collect(),
            })
            .collect();
        let m = build_model(&View { sessions }, &HashMap::new(), &HashMap::new());
        let mt = metrics(&m, 1920.0, 900.0);
        // widest grid: the same while-loop `metrics` uses to cap candidates
        let avail = mt.width - 2.0 * FLY_PAD;
        let mut want = 1usize;
        while want < m.sessions.len()
            && ((want + 1) as f32 * (COL_MIN_W + COL_GAP) - COL_GAP) <= avail
        {
            want += 1;
        }
        assert_eq!(mt.cols, want, "fallback picks the widest grid: {mt:?}");
    }

    #[test]
    fn metrics_window_is_fixed_and_clamped() {
        let m = model();
        // Wide desktop: 1080 × 72 % height.
        let mt = metrics(&m, 1920.0, 900.0);
        assert_eq!(mt.width, 1080.0);
        assert_eq!(mt.window_h, 648.0);
        // Short desktop: the height clamps to 480 and stays inside the area.
        let mt = metrics(&m, 1920.0, 600.0);
        assert_eq!(mt.window_h, 480.0);
        let mt = metrics(&m, 1920.0, 500.0);
        assert_eq!(mt.window_h, 452.0, "never taller than area-48: {mt:?}");
    }

    /// Tab labels: the session name alone, host-prefixed only when two
    /// hosts run same-named sessions.
    #[test]
    fn tab_labels_prefix_host_only_when_ambiguous() {
        let labels = tab_labels(&model());
        assert_eq!(
            labels,
            ["default", "justdone", "quiet", "down"],
            "unique names stay bare"
        );
        let v = View {
            sessions: vec![
                engine::SessionView {
                    host: "wsl".into(),
                    name: "default".into(),
                    up: true,
                    agents: vec![agent("a", "idle", "one")],
                },
                engine::SessionView {
                    host: "windows".into(),
                    name: "default".into(),
                    up: true,
                    agents: vec![agent("b", "idle", "two")],
                },
            ],
        };
        let m = build_model(&v, &HashMap::new(), &HashMap::new());
        assert_eq!(
            tab_labels(&m),
            ["windows/default", "wsl/default"],
            "ambiguous names gain the host prefix"
        );
    }

    /// Pixels, not just the model: renders the redesigned panel to a PNG.
    /// Ignored by default (needs a wgpu adapter — vulkan/lavapipe under
    /// WSLg); run it with `cargo test -- --ignored smoke`. One PNG per
    /// room: the overview grid and a session's full-width tab.
    #[test]
    #[ignore = "needs a wgpu adapter; run explicitly: cargo test -- --ignored smoke"]
    fn smoke_renders_the_panel_to_png() {
        let area = WorkArea {
            x_px: 0,
            y_px: 0,
            right_px: 1920,
            bottom_px: 900,
            w_pts: 1920.0,
            h_pts: 900.0,
            scale: 1.0,
        };
        let model = build_model(&smoke_view(), &HashMap::new(), &HashMap::new());
        let mt = metrics(&model, area.w_pts, area.h_pts);
        let app0 = App::new(AppSpec::new(
            "agent-notify",
            (mt.width, mt.window_h),
            flyout_init,
            flyout_view,
        ));
        let me = app0.handle();
        let state = FlyState {
            snapshot: smoke_view(),
            area,
            ..FlyState::new(
                Arc::new(engine::Engine::with_backends(
                    engine::Config::default(),
                    Vec::new(),
                )),
                me.clone(),
                Arc::new(Mutex::new(None)),
            )
        };
        let app = app0.init(move || state);
        let snap = std::env::temp_dir().join("agent-notify-flyout-smoke.png");
        rune::tooling::headless(
            app,
            rune::tooling::Args::parse_from(vec![
                "--snap".to_string(),
                snap.to_string_lossy().into_owned(),
                "--frames".to_string(),
                "8".to_string(),
            ]),
        );
        let meta = std::fs::metadata(&snap).expect("smoke PNG must exist");
        assert!(meta.len() > 0, "smoke PNG must not be empty");
        eprintln!("flyout smoke render written to {}", snap.display());
    }

    /// The session room: one session full-width behind its tab.
    #[test]
    #[ignore = "needs a wgpu adapter; run explicitly: cargo test -- --ignored smoke"]
    fn smoke_renders_a_session_room_to_png() {
        let area = WorkArea {
            x_px: 0,
            y_px: 0,
            right_px: 1920,
            bottom_px: 900,
            w_pts: 1920.0,
            h_pts: 900.0,
            scale: 1.0,
        };
        let model = build_model(&smoke_view(), &HashMap::new(), &HashMap::new());
        let mt = metrics(&model, area.w_pts, area.h_pts);
        let app0 = App::new(AppSpec::new(
            "agent-notify",
            (mt.width, mt.window_h),
            flyout_init,
            flyout_view,
        ));
        let me = app0.handle();
        let state = FlyState {
            snapshot: smoke_view(),
            area,
            tab: Tab::Session("wsl/default".to_string()),
            ..FlyState::new(
                Arc::new(engine::Engine::with_backends(
                    engine::Config::default(),
                    Vec::new(),
                )),
                me.clone(),
                Arc::new(Mutex::new(None)),
            )
        };
        let app = app0.init(move || state);
        let snap = std::env::temp_dir().join("agent-notify-flyout-room.png");
        rune::tooling::headless(
            app,
            rune::tooling::Args::parse_from(vec![
                "--snap".to_string(),
                snap.to_string_lossy().into_owned(),
                "--frames".to_string(),
                "8".to_string(),
            ]),
        );
        let meta = std::fs::metadata(&snap).expect("room PNG must exist");
        assert!(meta.len() > 0, "room PNG must not be empty");
        eprintln!("flyout room render written to {}", snap.display());
    }
}
