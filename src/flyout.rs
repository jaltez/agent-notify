//! The flyout panel: the platform-neutral row model built from a View
//! (`tray/flyout_rows.go`, shared with the Windows painter and the rune
//! port) plus the window itself (`tray/flyout_windows.go` Win32, replaced
//! by a borderless rune window here). Scaffold: signatures only, bodies
//! `todo!()`.
//!
//! RUNE MAPPING: the flyout becomes a rune window configured with
//! `WindowOptions { borderless: true, transparent: true, resizable: false,
//! always_on_top: true, visible: false, .. }` (fields verified in the
//! vendored `rune::WindowOptions`), opened by
//! `rune::App::windowless()` + `AppHandle::open`/`focus`, woken with
//! `Tx::wake_after` for the refresh cadence, and activated with
//! `AppHandle::activate(token)` when the tray hands over a Wayland token.

/// Panel geometry shared by the row model and the painters
/// (`flyout_rows.go` consts).
pub const FLY_WIDTH: i32 = 420;
pub const FLY_PAD: i32 = 14;

/// Classifies a flyout row; the painter picks layout, height, and
/// interaction from it. The model itself is platform-neutral so it can be
/// unit-tested anywhere (`tray.rowKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// fleet title line with severity dot.
    Summary,
    /// counts line with the fleet state bar.
    SummarySub,
    /// session header: chevron + counts, click toggles.
    Session,
    /// two-line attention block (dot + status wash).
    Agent,
    /// one collapsed-group agent on a single line.
    AgentCompact,
    /// "N finished"/"N resting" line, click toggles the group.
    Group,
    /// placeholder when nothing is known.
    Empty,
}

/// One segment of the fleet state bar (pixel width precomputed,
/// `tray.barSeg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarSeg {
    pub col: u32,
    pub px: i32,
}

/// One painted line (or block) of the flyout panel (`tray.flyRow`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlyRow {
    pub kind: Option<RowKind>,
    /// COLORREF; 0 = no dot (use [`Rgb::colorref`]).
    pub dot: u32,
    /// line 1 (agent blocks: the title).
    pub text: String,
    /// session rows: the host label; agent rows: host to open on.
    pub host: String,
    /// agent rows: session name the agent belongs to.
    pub sess: String,
    /// agent rows: herdr pane id, when the agent has one.
    pub pane: String,
    /// line 2, agent blocks only (runner · project).
    pub sub: String,
    /// paint in the dim color.
    pub dim: bool,
    /// group key this row toggles ("" = inert).
    pub toggle: String,
    /// effective open state, toggle rows only.
    pub open: bool,
    /// state bar segments, rowSummarySub only.
    pub bar: Vec<BarSeg>,
}

/// Strips glyphs Segoe UI renders as tofu boxes: emoji, pictographs,
/// nerd-font Private Use Area icons, variation selectors, and zero-width
/// joiners that herdr titles carry. Geometric shapes (chevrons) and normal
/// text pass through (`tray.gdiSafe`, flyout_rows.go:77).
pub fn gdi_safe(s: &str) -> String {
    let _ = s;
    todo!("port of tray.gdiSafe (flyout_rows.go:77)")
}

/// Builds the flyout row model from a View and the open/collapsed group
/// state (`tray.buildRows` in flyout_rows.go).
pub fn build_rows(v: &crate::engine::View, open_groups: &std::collections::HashSet<String>) -> Vec<FlyRow> {
    let _ = (v, open_groups);
    todo!("port of the flyout_rows.go row builder")
}

/// Shows the flyout panel as a borderless rune window, positioned at the
/// tray (Go: Win32 popup in `flyout_windows.go`). Focus moves to the agent
/// terminal on row click (`wt` + `herdr agent attach`, see the Go
/// flyout bridge); clicking an agent focuses it in an open client instead
/// of opening a tab.
pub fn show(v: &crate::engine::View) -> anyhow::Result<()> {
    let _ = v;
    todo!("port of tray/flyout_windows.go + cli/flyout_bridge.go on a rune window")
}
