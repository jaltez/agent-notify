//! The flyout panel, redesigned (v2): a single-column, rune-kit-rendered
//! reinterpretation of the Go GDI panel. The first port painted the Go
//! layout's fixed pixel rows with raw `el` boxes and inherited its
//! cramped grid; this version keeps only the *interaction* contract
//! (`tray/flyout_windows.go`: windowless app, show/hide without focus,
//! 1 s refresh, 30 s auto-close with pointer re-arm, Escape/click-outside
//! dismissal, agent-click focus) and rebuilds the surface from
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
//! borderless, transparent, always-on-top rune window opened by a
//! windowless [`rune::App`] (the `examples/launcher` pattern), driven by
//! `AppSpec::poll` for the refresh cadence, `.scroll(ScrollSpec)` for wheel
//! scrolling, `Tx::close_window` for dismissal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::el::ScrollSpec;
use rune::{
    col, el, icon, row, spacer, text, App, AppHandle, AppSpec, Color, Cx, Display, El, Key, Tx,
};
use rune::{KeyEvent, NativeDisplayId, WindowOptions};
use rune_kit::{Kit, PaletteSeed};

use crate::engine::{self, View};
use crate::icon;

// ---------------------------------------------------------------------------
// Panel model — the data the panel renders, unit tested
// ---------------------------------------------------------------------------

/// Panel geometry: fixed-width single column (the Go grid scaled columns
/// against a 1.5 aspect ratio; collapsibles do that compaction now).
pub const FLY_WIDTH: f32 = 420.0;
pub const FLY_PAD: f32 = 12.0;

/// One agent line in the panel: a two-line attention block (working /
/// blocked) or a compact single-line row inside an expanded group.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentItem {
    pub host: String,
    pub sess: String,
    pub pane: String,
    /// What the agent is doing: herdr's title, or the project when untitled.
    pub title: String,
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

/// Auto-close delay (`autoCloseDelay`): the panel hides itself after this
/// long unless the pointer is over it (then it re-arms) or it is scrollable
/// (then it is never armed).
pub const AUTO_CLOSE_DELAY: Duration = Duration::from_secs(30);
/// View refresh cadence (`refreshEvery`).
pub const REFRESH_EVERY: Duration = Duration::from_secs(1);
// PORT-NOTE: Go's activateGrace (600ms) is not a separate timer here. In the
// Go wndproc, the grace branch only resets `activated` (no hide decision
// depends on it): a focus loss hides iff the panel held foreground, which
// flyout_focus below reproduces directly.

/// Estimated part heights, in points, that [`metrics`] sums to size the
/// window. The real layout is flex (rune measures text); these stay coarse
/// because their only job is fitting the window to its content.
const EST_TITLE_H: f32 = 26.0; // "Agent Notify" + version
const EST_CHIPS_H: f32 = 22.0; // count chips row
const EST_BAR_H: f32 = 5.0; // state bar
const EST_HEADER_AIR: f32 = 26.0; // header gaps + divider
const EST_TRIGGER_H: f32 = 30.0; // session header
const EST_AGENT_H: f32 = 44.0; // two-line attention block
const EST_GROUP_H: f32 = 26.0; // "N finished"/"N resting" trigger
const EST_COMPACT_H: f32 = 22.0; // expanded group's agent line
const EST_SECTION_GAP: f32 = 10.0; // between sessions

/// The panel's outer width, clamped to the work area (single column: the
/// Go grid's column scoring is gone — collapsibles compact instead).
fn panel_width(area_w: f32) -> f32 {
    FLY_WIDTH.min((area_w - 16.0).max(280.0))
}

/// Window sizing for the panel: full content height up to the work area,
/// then it scrolls (`flyout.layout`'s width/height outcome, simplified).
pub struct Metrics {
    /// Outer panel width in points.
    pub width: f32,
    /// Full content height in points (pre-clamp).
    pub content_h: f32,
    /// Window height in points: as tall as the content, up to
    /// `area_h - 16`; beyond that the panel scrolls.
    pub window_h: f32,
    pub scrollable: bool,
}

/// Estimates the panel's content height from the model (`flyout.layout`'s
/// bookkeeping, minus the grid: rune lays the rows out for real).
pub fn metrics(m: &PanelModel, area_w: f32, area_h: f32) -> Metrics {
    let width = panel_width(area_w);
    let mut h = FLY_PAD * 2.0
        + EST_TITLE_H
        + EST_CHIPS_H
        + EST_BAR_H
        + EST_HEADER_AIR
        + EST_SECTION_GAP;
    if m.empty {
        h += EST_TRIGGER_H;
    } else {
        for s in &m.sessions {
            h += EST_TRIGGER_H;
            if s.open {
                h += s.active.len() as f32 * EST_AGENT_H;
                for g in [&s.finished, &s.resting].into_iter().flatten() {
                    h += EST_GROUP_H;
                    if g.open {
                        h += g.agents.len() as f32 * EST_COMPACT_H;
                    }
                }
            }
            h += EST_SECTION_GAP;
        }
    }
    let max_h = (area_h - 16.0).max(120.0);
    let scrollable = h > max_h;
    Metrics {
        width,
        content_h: h,
        window_h: if scrollable { max_h } else { h },
        scrollable,
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
/// (`icon::color`): blocked red, down amber, working blue, waiting green,
/// idle gray. Panel and tray speak one visual language.
fn sev_color(sev: &str) -> Color {
    col_of(colorref(icon::color(sev)))
}

/// The kit the panel renders with, from three seed colours: the deep
/// neutral panel background, near-white text, and the working blue as
/// accent. Status colours stay `icon::color`'s (see [`sev_color`]) — kit
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
    /// Bottom-right anchor in physical pixels, the Go margins (12 right, 8
    /// bottom) scaled to points (`flyout.show`: x = right - w - 12,
    /// y = bottom - h - 8).
    pub fn bottom_right_px(&self, w_pts: f32, h_pts: f32) -> (i32, i32) {
        let s = if self.scale > 0.01 { self.scale } else { 1.0 };
        let x = self.right_px - (w_pts * s + 12.0 * s).round() as i32;
        let y = self.bottom_px - (h_pts * s + 8.0 * s).round() as i32;
        (x, y)
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

    /// Shows and activates the panel, resetting the scroll, the hover
    /// state and the auto-close clock (`flyout.show`).
    pub fn show(&self) {
        self.me.update(|s: &mut FlyState, tx: &mut Tx| {
            s.shown_at = Some(Instant::now());
            s.armed_evaluated = false; // re-evaluate the auto-close arming
            s.pointer_inside = false;
            s.activated = false;
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
            s.shown_at = Some(Instant::now());
            s.armed_evaluated = false;
            s.pointer_inside = false;
            s.activated = false;
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
            s.applied_size = None; // re-anchor with the real geometry
            s.applied_pos = None;
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

/// The rune [`WindowOptions`] for the panel: borderless, transparent (the
/// rounded corners clip the fill), topmost, non-resizable, no min-size
/// floor (the poll sizes it to its content).
pub fn window_options() -> WindowOptions {
    WindowOptions {
        transparent: true,
        borderless: true,
        click_through: false,
        always_on_top: true,
        visible: false,
        position: None,
        physical_size: None,
        min_size: Some((0.0, 0.0)),
        max_size: None,
        resizable: false,
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
        let app = App::new(AppSpec {
            poll: Some(flyout_poll),
            on_key: Some(flyout_on_key),
            ..AppSpec::new("agent-notify", (420.0, 300.0), flyout_init, flyout_view)
        })
        .windowless() // no window at launch; open() creates and shows it
        .on_focus(flyout_focus);
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

/// The panel's per-window state (`flyout` struct's window-thread fields).
/// `pub(crate)` so the tray can park the opaque [`App`] between init and
/// run; every field stays private.
pub(crate) struct FlyState {
    eng: Arc<engine::Engine>,
    me: AppHandle<FlyState>,
    /// The kit the panel renders with (`FlyState` owns it so the view fn
    /// stays a pure render of model + tokens).
    kit: Kit,
    focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    /// collapse state: session key -> user override; defaults live in
    /// `build_rows` (`flyout.openState`).
    open_state: HashMap<String, bool>,
    /// group key ("session/finished" | "session/resting") -> override
    /// (`flyout.restOpen`).
    rest_open: HashMap<String, bool>,
    area: WorkArea,
    /// when the panel was shown: the auto-close clock and the focus grace.
    shown_at: Option<Instant>,
    /// the auto-close arming is evaluated once per show: only a
    /// non-scrollable panel gets one (`flyout.show`).
    armed_evaluated: bool,
    autoclose_armed: bool,
    /// whether we actually hold foreground: the deactivate-dismiss (click
    /// outside) may only fire when we had it, otherwise a lost foreground
    /// race hides the panel instantly (`flyout.activated`).
    activated: bool,
    /// pointer over the panel: the auto-close re-arms while it is.
    pointer_inside: bool,
    /// last applied window size/position, so poll only requests changes.
    applied_size: Option<(f32, f32)>,
    applied_pos: Option<(i32, i32)>,
    /// the engine view the view fn renders; refreshed by poll.
    snapshot: View,
    /// fingerprint of the last rendered row model: skips view re-runs when
    /// a refresh tick found nothing new.
    last_fp: String,
}

impl FlyState {
    fn new(
        eng: Arc<engine::Engine>,
        me: AppHandle<FlyState>,
        focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    ) -> FlyState {
        FlyState {
            eng,
            me,
            kit: panel_kit(),
            focus_agent,
            open_state: HashMap::new(),
            rest_open: HashMap::new(),
            // Sane default until the run factory lands the real display
            // geometry (`Tray::run` sends it as the loop's first update).
            area: WorkArea::default(),
            shown_at: None,
            armed_evaluated: true,
            autoclose_armed: false,
            activated: false,
            pointer_inside: false,
            applied_size: None,
            applied_pos: None,
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

/// View refresh + window geometry + auto-close (`WM_TIMER` refresh,
/// `flyout.paint`'s re-size and the auto-close timer). `now` is rune's app
/// clock; the wall-clock timers use `Instant` like the Go timers did.
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
    let fp = model_fingerprint(&model);
    let changed = fp != s.last_fp;
    s.last_fp = fp;

    let mt = metrics(&model, s.area.w_pts, s.area.h_pts);
    // refresh and in-panel collapse/expand change the content: keep the
    // window inside the work area and resize on the spot
    if s.applied_size != Some((mt.width, mt.window_h)) {
        tx.resize_window_instant((mt.width, mt.window_h));
        let pos = s.area.bottom_right_px(mt.width, mt.window_h);
        if s.applied_pos != Some(pos) {
            s.me.set_position(pos);
            s.applied_pos = Some(pos);
        }
        s.applied_size = Some((mt.width, mt.window_h));
    }
    if !s.armed_evaluated {
        s.armed_evaluated = true;
        // A scrollable panel stays open until dismissed (flyout.show).
        s.autoclose_armed = !mt.scrollable;
    }
    if s.autoclose_armed
        && s.shown_at.is_some_and(|t| t.elapsed() >= AUTO_CLOSE_DELAY)
    {
        if s.pointer_inside {
            // the user is reading the panel: re-arm instead of hiding
            s.shown_at = Some(Instant::now());
        } else {
            tx.close_window();
        }
    }
    tx.wake_after(REFRESH_EVERY);
    changed
}

/// Escape closes the panel (`WM_KEYDOWN` VK_ESCAPE).
fn flyout_on_key(_s: &mut FlyState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key == Key::Escape {
        tx.close_window();
        return true;
    }
    false
}

/// Focus transitions (`WM_ACTIVATE`): a focus loss dismisses the panel —
/// but only if we actually held foreground. A panel whose activation was
/// yanked during startup (another window held foreground; `activateGrace`)
/// never had `activated` set, so it stays up until a real dismissal.
fn flyout_focus(s: &mut FlyState, focused: bool, tx: &mut Tx) {
    if focused {
        s.activated = true;
        return;
    }
    if s.activated {
        s.activated = false;
        tx.close_window();
    }
    // No activation held (the startup grace case): keep the panel up, as a
    // real tray click grants foreground and click-outside still dismisses
    // afterwards.
}

/// The scroll viewport's key (`Tx::scroll_to_y` reset on show).
const CONTENT_KEY: &str = "flyout-content";

/// Builds the panel (`flyout.paint`): a rounded, bordered, dark panel; the
/// two full-width summary rows; then the session grid, wheel-scrollable
/// with rune's own scrollbar thumb when the content overflows.
fn flyout_view(s: &FlyState, _cx: &mut Cx<FlyState>) -> El {
    let model = build_model(&s.snapshot, &s.open_state, &s.rest_open);
    let k = &s.kit;

    let mut body = col().w_full().gap(2.0);
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
    } else {
        for (i, sec) in model.sessions.iter().enumerate() {
            body = body.child(session_section(sec, i, k));
        }
    }

    let inner_w = panel_width(s.area.w_pts) - 2.0 * FLY_PAD;
    col()
        .key("panel")
        .w_full()
        .h_full()
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_muted)
        .radius(14.0)
        .clip_rounded(14.0)
        .padding(FLY_PAD)
        .on_hover_change(|s: &mut FlyState, hovering: bool, _| {
            s.pointer_inside = hovering; // the auto-close re-arms while over the panel
        })
        .child(
            col()
                .key(CONTENT_KEY)
                .w_full()
                .h_full()
                .gap(10.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(header_el(&model, inner_w, k))
                .child(el().w_full().h(1.0).fill(k.c.border_muted))
                .child(body),
        )
}

/// The header: fleet dot + name (+ version stamp), the count chips — which
/// double as the legend for the row dots — and the proportional state bar.
fn header_el(m: &PanelModel, inner_w: f32, k: &Kit) -> El {
    let mut c = col().w_full().gap(8.0);

    let mut title = row()
        .key("header-title")
        .w_full()
        .h(EST_TITLE_H)
        .center_y()
        .gap(8.0)
        .child(el().size(10.0, 10.0).radius(5.0).fill(sev_color(m.fleet_sev)))
        .child(text("Agent Notify").font(k.ty.ui_medium).color(k.c.text_base));
    if crate::buildinfo::is_release() {
        // stamp the build so a stale binary is visible at a glance
        title = title.child(spacer()).child(
            text(crate::buildinfo::VERSION)
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
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
        // integer pixel spans — the last segment absorbs the rounding (the
        // Go bar's trick) so adjacent segments leave no seams
        let mut widths: Vec<f32> = Vec::with_capacity(m.bar.len());
        let mut used = 0.0f32;
        for (i, seg) in m.bar.iter().enumerate() {
            let w = if i + 1 == m.bar.len() {
                (inner_w - used).max(0.0)
            } else {
                (seg.weight / total * inner_w).floor().max(1.0)
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
                .radius(2.5)
                .clip_rounded(2.5)
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

/// One session: the collapsible trigger plus, when open, its attention
/// blocks and settled groups (`flyout.paint`'s session group, restacked).
fn session_section(sec: &SessionSection, idx: usize, k: &Kit) -> El {
    let mut c = col().key(format!("section:{idx}")).w_full().gap(2.0);
    c = c.child(session_trigger(sec, k));
    if !sec.open {
        return c;
    }
    for (j, a) in sec.active.iter().enumerate() {
        c = c.child(agent_row(a, format!("agent:{idx}:{j}"), k));
    }
    for g in [sec.finished.as_ref(), sec.resting.as_ref()]
        .into_iter()
        .flatten()
    {
        c = c.child(group_trigger(g, k));
        if g.open {
            for (j, a) in g.agents.iter().enumerate() {
                c = c.child(compact_row(a, format!("compact:{idx}:{j}"), k));
            }
        }
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
        .pl(2.0)
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
                .font(k.ty.ui_medium)
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
fn agent_row(a: &AgentItem, key: String, k: &Kit) -> El {
    let hit = (a.host.clone(), a.sess.clone(), a.pane.clone());
    let color = sev_color(a.sev);
    let mut blk = col().key(key).w_full().radius(k.radius.sm);
    if a.sev == "blocked" {
        blk = blk.fill(k.alpha(color, 0.10));
    }
    let mut meta = row()
        .w_full()
        .h(18.0)
        .center_y()
        .gap(6.0)
        .pl(26.0) // dot column (8px dot + 10px gap) + the 8px row inset
        .child(
            text(a.meta.clone())
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .truncate(),
        );
    if a.focused {
        meta = meta.child(text("✱").font(k.ty.caption()).color(sev_color("down")));
    }
    blk = blk
        .child(
            row()
                .w_full()
                .h(26.0)
                .center_y()
                .gap(10.0)
                .pl(8.0)
                .child(el().size(8.0, 8.0).radius(4.0).fill(color))
                .child(
                    text(a.title.clone())
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(el().w(2.0)),
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
        .pl(20.0) // nested rows sit inside their session header
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
fn compact_row(a: &AgentItem, key: String, k: &Kit) -> El {
    let hit = (a.host.clone(), a.sess.clone(), a.pane.clone());
    let t = row()
        .key(key)
        .w_full()
        .h(EST_COMPACT_H)
        .center_y()
        .gap(10.0)
        .pl(30.0) // group indent + dot column
        .radius(k.radius.sm)
        .child(el().size(7.0, 7.0).radius(3.5).fill(sev_color(a.sev)))
        .child(
            text(a.title.clone())
                .font(k.ty.ui_compact)
                .color(k.c.text_muted)
                .truncate(),
        );
    k.interactive(t).on_press_tx(move |s: &mut FlyState, tx: &mut Tx| {
        focus_agent(s, tx, &hit.0, &hit.1, &hit.2);
    })
}

/// Agent click: close the panel, then run the injected focus callback off
/// the UI thread (`flyout.focusAgent`: hide, then go func).
fn focus_agent(s: &mut FlyState, tx: &mut Tx, host: &str, sess: &str, pane: &str) {
    if pane.is_empty() {
        return;
    }
    tx.close_window();
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
            ],
        }
    }

    /// Pixels, not just the model: renders the redesigned panel to a PNG.
    /// Ignored by default (needs a wgpu adapter — vulkan/lavapipe under
    /// WSLg); run it with `cargo test -- --ignored smoke`.
    #[test]
    #[ignore = "needs a wgpu adapter; run explicitly: cargo test -- --ignored smoke"]
    fn smoke_renders_the_panel_to_png() {
        // A standalone app whose state is the panel's, pre-loaded with the
        // synthetic view (poll is None here: the snapshot must survive).
        let app0 = App::new(AppSpec::new(
            "agent-notify",
            (420.0, 560.0),
            flyout_init,
            flyout_view,
        ));
        let me = app0.handle();
        let state = FlyState {
            snapshot: smoke_view(),
            area: WorkArea {
                x_px: 0,
                y_px: 0,
                right_px: 420,
                bottom_px: 560,
                w_pts: 420.0,
                h_pts: 560.0,
                scale: 1.0,
            },
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
}
