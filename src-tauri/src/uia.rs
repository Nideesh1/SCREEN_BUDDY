//! UIA grounding PROTOTYPE — read-only probe of the Windows UI Automation tree.
//!
//! # Why this exists
//!
//! The worker drives the GUI with PIXEL COORDINATES that a vision model
//! estimates from a ~1024px-wide downscaled screenshot. That estimate is our
//! dominant production failure: the model knows exactly which control it wants
//! and still lands 30px off, three turns in a row, on the same taskbar icon.
//!
//! Windows already publishes the answer. Every control in the UI Automation
//! tree (the same tree screen readers consume) reports a name, a control type,
//! and an EXACT screen rectangle. The intended end state is "screenshot for
//! understanding, element list for aiming": the model says `click element 7`
//! and Rust looks up rect 7 and clicks its centre. No estimation anywhere.
//!
//! **This file is not that.** This is the prototype that answers the only
//! question worth answering first: *what does the tree actually report on the
//! apps our workers touch?* Nothing here is wired into the agent loop, the
//! system prompt, or the tool schema. It is two Tauri commands that dump JSON
//! for a human to read. Deliberately so — we want data before we want a
//! feature.
//!
//! # Honest limits (read these before believing a dump)
//!
//! These are real, they are not fixable from inside this file, and they are why
//! any eventual integration must be **UIA-first, pixels-fallback** and never a
//! replacement for the vision path:
//!
//! - **Elevated windows are invisible.** A non-elevated process cannot read the
//!   UIA tree of an elevated one (UIPI). A dump taken while an admin Task
//!   Manager or a UAC consent dialog is foreground will come back empty or
//!   report only the desktop. This is not new breakage: our SendInput path
//!   already cannot type into those windows either. Same boundary, same
//!   mitigation (run the worker elevated, or accept the gap).
//! - **Electron / Chromium / custom-drawn UIs report useless trees.** Slack,
//!   VS Code, Discord, Teams and anything drawing its own widgets on a canvas
//!   typically surface one giant `Pane` or `Document` and nothing clickable
//!   inside it. Chromium gates its accessibility tree behind lazy activation —
//!   it materialises only once a client asks for it, and even then browsers may
//!   need `--force-renderer-accessibility` (or a screen reader present) to
//!   expose page content. A thin dump on a browser is EXPECTED, not a bug in
//!   this code; the filtered/unfiltered pair below exists so we can tell the
//!   two apart.
//! - **Java/Swing, Qt (without the a11y bridge), games, and remote-desktop
//!   sessions** report nothing usable at all.
//! - **Rects are physical desktop pixels**, in the virtual-screen coordinate
//!   space (so negative x/y on a left-hand secondary monitor is normal and
//!   correct). Our click path takes MODEL-space coordinates scaled against
//!   `sent_w`/`sent_h`. Bridging the two is a later task, and it also requires
//!   the process to be per-monitor DPI aware — if it is not, Windows lies to us
//!   about rects on scaled displays. Nothing here clicks anything, so this
//!   prototype is unaffected; the integration is not.
//! - **The tree is a snapshot of a moving target.** Between the walk and any
//!   later click the UI may have re-laid-out. Grounding buys precision, not
//!   freshness.
//!
//! # Scope
//!
//! Foreground window only, not the whole desktop. The desktop root walk is
//! enormous (every tray icon, every background window, every hidden shell
//! surface) and slow enough to be its own denial of service. Start focused.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// Maximum elements returned in a filtered dump. A model reading an element
/// list pays attention roughly like a human reading a menu: past a few dozen
/// entries the list stops being an aid. 60 also keeps the JSON well under the
/// token budget a screenshot already eats.
const FILTERED_CAP: usize = 60;

/// The unfiltered dump exists for diagnosis ("what are we throwing away?"), so
/// it gets a much larger cap — but still a cap, because a Win32 tree can be
/// tens of thousands of nodes and we are not going to serialize all of it.
const UNFILTERED_CAP: usize = 600;

/// Reading-order row banding, in physical pixels. Two controls whose tops are
/// within this many pixels are treated as being on the same visual row and
/// ordered left-to-right. 16px is about one line of UI text at 100% scale;
/// wider bands start merging genuinely distinct rows in dense toolbars.
const ROW_BAND: i32 = 16;

/// Anything occupying this share of the window is a backdrop, not a target.
/// Even after control-type filtering these show up (a full-window `ListItem`
/// in a single-item list, a `Document` the size of the client area).
const MAX_AREA_FRACTION: f64 = 0.95;

/// Depth limit for the tree walk. Real UI nests ~10-15 deep; past 24 we are
/// almost certainly in a pathological/recursive provider.
#[cfg(windows)]
const MAX_DEPTH: usize = 24;

/// Hard ceiling on nodes visited, regardless of depth. A cross-process UIA
/// property read is a COM round trip (~0.1-1ms), so this bounds the walk at
/// roughly a couple of seconds even in the worst case.
#[cfg(windows)]
const MAX_VISIT: usize = 4000;

/// Wall-clock ceiling on the walk. Belt to `MAX_VISIT`'s braces: a single
/// hung/unresponsive provider can block one COM call for far longer than its
/// node count suggests.
#[cfg(windows)]
const WALK_BUDGET_MS: u64 = 2500;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One element as the model would eventually see it.
///
/// `index` is the handle the model would cite (`click element 7`). It is
/// assigned AFTER filtering and ordering, so it is dense (0..n) and matches the
/// order the list is printed in — which is the whole point. It is stable only
/// within one dump; a fresh dump renumbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiaElement {
    pub index: usize,
    /// UIA control type, e.g. `Button`, `Edit`, `ListItem`. `Unknown` when the
    /// provider reports an id outside the documented enum.
    pub control_type: String,
    /// The accessible name — the label a screen reader would read. Empty is
    /// legal and common for icon-only controls that carry an automation id.
    pub name: String,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    /// Screen rect in physical, virtual-desktop pixels. `x`/`y` may be negative
    /// on a secondary monitor placed left of / above the primary.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Rect centre — the point a future `click element N` would target.
    /// Precomputed so the dump is directly comparable against whatever the
    /// model guessed from the screenshot.
    pub cx: i32,
    pub cy: i32,
    pub enabled: bool,
    pub offscreen: bool,
    /// Depth in the UIA tree below the window root. Diagnostic only.
    pub depth: usize,
}

/// Result of one dump. Always this shape, on every platform and on failure, so
/// the operator's console paste is uniform and `ok`/`note` carry the story.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiaDump {
    /// False when the tree could not be read at all (wrong OS, no foreground
    /// window, UIA refused). `note` says why. An `ok: true` dump with zero
    /// elements is a DIFFERENT and very interesting result: the tree was read
    /// and the app genuinely exposes nothing (Electron, elevated, browser with
    /// accessibility asleep).
    pub ok: bool,
    /// Foreground `HWND` as an integer, for correlating with other tooling.
    pub hwnd: i64,
    /// The window root element's accessible name — usually the title bar text.
    pub window_title: String,
    /// Window rect, physical pixels, `[x, y, w, h]`. Used for the backdrop
    /// filter and to sanity-check that rects land where the screenshot shows.
    pub window_rect: [i32; 4],
    /// True for `uia_dump` (the model-facing view), false for `uia_dump_all`.
    pub filtered: bool,
    /// Nodes actually visited by the walk, before any filtering.
    pub visited: usize,
    /// Nodes that survived filtering, before the cap.
    pub matched: usize,
    /// True when `matched` exceeded the cap and the tail was dropped.
    pub truncated: bool,
    /// True when the walk hit `MAX_VISIT`, `MAX_DEPTH` or the time budget and
    /// stopped early. A truncated WALK means the dump is not a faithful
    /// picture of the window — worth knowing before drawing conclusions.
    pub walk_capped: bool,
    pub elapsed_ms: u64,
    pub note: String,
    pub elements: Vec<UiaElement>,
}

/// A node as harvested from the tree, before filtering/ordering. Split out from
/// `UiaElement` so that every decision this file makes about what to keep and
/// how to order it is pure, platform-independent, and unit-testable on the Mac
/// none of us can run UIA on.
#[derive(Debug, Clone, PartialEq)]
pub struct RawElement {
    pub control_type: String,
    pub name: String,
    pub automation_id: String,
    pub class_name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub enabled: bool,
    pub offscreen: bool,
    pub depth: usize,
    /// Position in the depth-first tree walk. Preserved as the final tiebreak
    /// so ordering is total and deterministic.
    pub seq: usize,
}

// ---------------------------------------------------------------------------
// Pure logic: filtering, ordering, rect math
// ---------------------------------------------------------------------------

/// Control types worth offering a model as click targets.
///
/// Chosen for "clicking this does something". Deliberately EXCLUDED, with
/// reasons, because each one costs list slots that a real target could use:
///
/// - `Text` — labels. Hugely numerous, almost never the target. The label next
///   to a checkbox is not the checkbox.
/// - `Pane` / `Group` / `Window` / `Custom` / `Document` — containers. They
///   have big rects whose centre is usually empty space.
/// - `Image` — decorative far more often than actionable; an actionable icon is
///   nearly always wrapped in a `Button`.
/// - `ToolBar` / `MenuBar` / `Tab` / `List` / `Tree` / `Table` — the container,
///   not the item. We keep their children (`MenuItem`, `TabItem`, `ListItem`,
///   `TreeItem`, `DataItem`) instead.
/// - `ScrollBar` / `Thumb` — scrolling is a keyboard/wheel action for us.
///
/// `uia_dump_all` exists precisely so this list can be audited against reality
/// instead of defended from an armchair.
const INTERACTABLE: &[&str] = &[
    "Button",
    "CheckBox",
    "ComboBox",
    "DataItem",
    "Edit",
    "HeaderItem",
    "Hyperlink",
    "ListItem",
    "MenuItem",
    "RadioButton",
    "Slider",
    "SplitButton",
    "Spinner",
    "TabItem",
    "TreeItem",
];

/// Is this control type one a model should be offered as a click target?
pub fn is_interactable(control_type: &str) -> bool {
    INTERACTABLE.contains(&control_type)
}

/// Rect centre. Integer division truncates toward zero, which for negative
/// coordinates (a left-hand secondary monitor) would bias the point right/down
/// by one pixel — irrelevant for clicking, but the arithmetic is written as
/// `x + w/2` on the ORIGIN rather than averaging the edges so the bias is at
/// least consistent and never escapes the rect.
pub fn center(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    (x + w / 2, y + h / 2)
}

/// Does the element carry enough identity for a model to pick it by name?
///
/// An unnamed control with no automation id is un-citable — the model has
/// nothing to match against what it sees in the screenshot. The one exception
/// is `Edit`: text fields are routinely unnamed, and "the text box" is usually
/// unambiguous from the screenshot anyway, so an unnamed `Edit` still earns a
/// slot.
fn has_identity(e: &RawElement) -> bool {
    !e.name.trim().is_empty() || !e.automation_id.trim().is_empty() || e.control_type == "Edit"
}

/// Should this node appear in the filtered (model-facing) list?
///
/// Order of cheap-to-expensive rejections: geometry, then visibility, then
/// control type, then identity, then backdrop size.
pub fn keep_filtered(e: &RawElement, window_area: i64) -> bool {
    // Zero-area nodes are real and numerous: collapsed containers, virtualized
    // list items that have not been realised, providers that report a rect
    // before layout. There is nothing to click.
    if e.w <= 0 || e.h <= 0 {
        return false;
    }
    // Scrolled out of view, or on a hidden tab. Clicking the reported rect
    // would hit whatever is actually drawn there instead.
    if e.offscreen {
        return false;
    }
    // Disabled controls are visible and legitimately confusing to a model —
    // it will click a greyed-out OK button and report success. Dropping them
    // from the aiming list is the cheapest possible fix for that.
    if !e.enabled {
        return false;
    }
    if !is_interactable(&e.control_type) {
        return false;
    }
    if !has_identity(e) {
        return false;
    }
    if window_area > 0 {
        let area = (e.w as i64) * (e.h as i64);
        if (area as f64) > (window_area as f64) * MAX_AREA_FRACTION {
            return false;
        }
    }
    true
}

/// Should this node appear in the unfiltered (diagnostic) list?
///
/// Only genuinely empty noise is dropped, so the operator can see what the
/// filter above is discarding. Zero-area nodes go because they are pure volume
/// — often the majority of a Win32 tree — and carry no information.
pub fn keep_unfiltered(e: &RawElement) -> bool {
    e.w > 0 && e.h > 0
}

/// Sort key: reading order, not tree order.
///
/// Tree order is provider-authored and frequently has nothing to do with what
/// the model is looking at — Win32 z-order, XAML visual-tree order, and "the
/// order the developer added controls" all diverge from the screenshot. A model
/// reading `element 7` off a list and matching it to a screenshot does far
/// better when list position tracks visual position, so we sort top-to-bottom
/// by row band and then left-to-right within the band.
///
/// Banding rather than raw `y` matters: controls on one visual row rarely share
/// an exact top (a 24px button next to a 22px combo box), and sorting on raw
/// `y` would interleave two adjacent rows into a zigzag.
pub fn reading_order_key(e: &RawElement) -> (i32, i32, usize) {
    // `div_euclid` rather than `/` so negative y (secondary monitor) bands
    // downward consistently instead of folding toward zero at the origin.
    (e.y.div_euclid(ROW_BAND), e.x, e.seq)
}

/// Turn a raw walk into the final ordered, capped, indexed list.
///
/// Steps, in order:
/// 1. filter (per `filtered`),
/// 2. dedupe by exact rect,
/// 3. sort into reading order,
/// 4. cap,
/// 5. assign dense indices.
///
/// Indices are assigned LAST so that `index` always equals list position.
///
/// The dedupe matters more than it looks. UIA routinely reports a `Button` and
/// a `Text` child sharing one rect, or a `ListItem` wrapping a `DataItem` of
/// identical bounds. Two entries pointing at the same pixels is worse than
/// useless to a model: it invites a coin flip between synonyms. We keep the
/// FIRST occurrence in depth-first order, which is the shallower node — the
/// wrapper that actually handles the click, not its inner label.
///
/// Returns `(elements, matched_before_cap, truncated)`.
pub fn refine(raw: &[RawElement], filtered: bool, cap: usize, window_area: i64)
    -> (Vec<UiaElement>, usize, bool)
{
    let mut kept: Vec<&RawElement> = raw
        .iter()
        .filter(|e| if filtered { keep_filtered(e, window_area) } else { keep_unfiltered(e) })
        .collect();

    // Dedupe by rect, keeping the depth-first-first (shallowest) occurrence.
    // `seq` is assigned in walk order, so sorting by it restores that order
    // before the scan; a plain `retain` over a HashSet would depend on the
    // input already being in walk order, which is true today but is exactly the
    // kind of invisible coupling that breaks later.
    kept.sort_by_key(|e| e.seq);
    let mut seen: std::collections::HashSet<(i32, i32, i32, i32)> = std::collections::HashSet::new();
    kept.retain(|e| seen.insert((e.x, e.y, e.w, e.h)));

    let matched = kept.len();
    kept.sort_by_key(|e| reading_order_key(e));

    let truncated = matched > cap;
    kept.truncate(cap);

    let elements = kept
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let (cx, cy) = center(e.x, e.y, e.w, e.h);
            UiaElement {
                index: i,
                control_type: e.control_type.clone(),
                name: e.name.clone(),
                automation_id: opt(&e.automation_id),
                class_name: opt(&e.class_name),
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
                cx,
                cy,
                enabled: e.enabled,
                offscreen: e.offscreen,
                depth: e.depth,
            }
        })
        .collect();

    (elements, matched, truncated)
}

/// `""` -> `None`, so the JSON does not carry a screenful of empty strings.
fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// Assemble a `UiaDump` from a completed walk. Shared by the Windows path and
/// the off-platform stub so the two cannot drift in shape.
pub fn build_dump(
    ok: bool,
    hwnd: i64,
    window_title: String,
    window_rect: [i32; 4],
    raw: &[RawElement],
    filtered: bool,
    visited: usize,
    walk_capped: bool,
    elapsed_ms: u64,
    note: impl Into<String>,
) -> UiaDump {
    let cap = if filtered { FILTERED_CAP } else { UNFILTERED_CAP };
    let window_area = (window_rect[2] as i64) * (window_rect[3] as i64);
    let (elements, matched, truncated) = refine(raw, filtered, cap, window_area);
    UiaDump {
        ok,
        hwnd,
        window_title,
        window_rect,
        filtered,
        visited,
        matched,
        truncated,
        walk_capped,
        elapsed_ms,
        note: note.into(),
        elements,
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Instant;
    use uiautomation::types::{ControlType, Handle};
    use uiautomation::{UIAutomation, UIElement};
    use uiautomation::core::UITreeWalker;

    /// Map a `ControlType` to the canonical string `INTERACTABLE` is written
    /// against.
    ///
    /// Every clickable type is spelled out here rather than derived from
    /// `{:?}`/`{}` formatting. That looks like duplication and is not: the
    /// filter's entire behaviour hinges on these strings matching, and a
    /// formatting impl in someone else's crate is free to change its output
    /// (`Debug` gaining a wrapper, `Display` returning prose or a localized
    /// name) in a patch release. If that happened while we depended on it, the
    /// filtered dump would silently return ZERO elements on every app — which
    /// looks exactly like "UIA doesn't work here", the wrong conclusion, and
    /// the whole reason this prototype exists. Pinning them makes the contract
    /// ours.
    ///
    /// Non-clickable types keep the `{:?}` fallback: they only ever appear in
    /// the diagnostic `uia_dump_all` output, where an imprecise label costs a
    /// moment of squinting rather than a wrong conclusion.
    fn control_type_name(ct: ControlType) -> String {
        match ct {
            ControlType::Button => "Button",
            ControlType::CheckBox => "CheckBox",
            ControlType::ComboBox => "ComboBox",
            ControlType::DataItem => "DataItem",
            ControlType::Edit => "Edit",
            ControlType::HeaderItem => "HeaderItem",
            ControlType::Hyperlink => "Hyperlink",
            ControlType::ListItem => "ListItem",
            ControlType::MenuItem => "MenuItem",
            ControlType::RadioButton => "RadioButton",
            ControlType::Slider => "Slider",
            ControlType::SplitButton => "SplitButton",
            ControlType::Spinner => "Spinner",
            ControlType::TabItem => "TabItem",
            ControlType::TreeItem => "TreeItem",
            other => return format!("{other:?}"),
        }
        .to_string()
    }

    // The foreground `HWND`. Declared directly rather than pulling in the
    // `windows` crate: `uiautomation` already depends on a specific `windows`
    // version, and adding our own would either duplicate it or pin us to
    // whatever `uiautomation` bumps to next. `GetForegroundWindow` is a
    // zero-argument, no-error, stable-since-Win95 call returning an `HWND`,
    // which is a pointer-sized handle — `isize` is its exact ABI shape, and
    // `uiautomation::types::Handle` has `From<isize>`, so we never need the
    // `HWND` newtype at all.
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
    }

    /// Mutable state threaded through the recursive walk.
    struct Ctx {
        raw: Vec<RawElement>,
        visited: usize,
        capped: bool,
        deadline: Instant,
    }

    impl Ctx {
        fn should_stop(&mut self) -> bool {
            if self.capped {
                return true;
            }
            if self.visited >= MAX_VISIT || Instant::now() >= self.deadline {
                self.capped = true;
                return true;
            }
            false
        }
    }

    /// Read every property we care about off one element.
    ///
    /// Each getter is an independent cross-process COM call that can fail on
    /// its own — most commonly with `UIA_E_ELEMENTNOTAVAILABLE` when the
    /// control is destroyed mid-walk (a menu closing under us is the classic
    /// case). Every one is therefore defaulted rather than propagated: a node
    /// with a name and no class name is still useful, and one bad property
    /// must not abort the walk.
    fn snapshot(el: &UIElement, depth: usize, seq: usize) -> RawElement {
        let control_type = match el.get_control_type() {
            Ok(ct) => control_type_name(ct),
            // A provider reporting a control-type id outside the documented
            // enum is rare but legal. The localized name ("button", in the
            // user's UI language) is still better than nothing for diagnosis,
            // though it will not match INTERACTABLE — which is correct, we do
            // not want to aim at things we cannot classify.
            Err(_) => el
                .get_localized_control_type()
                .unwrap_or_else(|_| "Unknown".to_string()),
        };
        let rect = el.get_bounding_rectangle().ok();
        let (x, y, w, h) = match rect {
            Some(r) => (r.get_left(), r.get_top(), r.get_width(), r.get_height()),
            None => (0, 0, 0, 0),
        };
        RawElement {
            control_type,
            name: el.get_name().unwrap_or_default(),
            automation_id: el.get_automation_id().unwrap_or_default(),
            class_name: el.get_classname().unwrap_or_default(),
            x,
            y,
            w,
            h,
            // Default to "usable" when the provider will not say. A false
            // negative here silently deletes a real target from the list,
            // which is the failure we are trying to fix; a false positive at
            // worst wastes one list slot.
            enabled: el.is_enabled().unwrap_or(true),
            offscreen: el.is_offscreen().unwrap_or(false),
            depth,
            seq,
        }
    }

    /// Depth-first walk of the control view.
    ///
    /// The CONTROL view (not the raw view) is deliberate: the raw view includes
    /// every non-interactive structural node a provider felt like publishing
    /// and is several times larger for no gain. The control view is what
    /// assistive tech consumes, which is exactly the population we want.
    ///
    /// Recursion is bounded by `MAX_DEPTH` (24), so worst-case stack use is
    /// trivial and an explicit stack would only obscure the sibling walk.
    fn walk(walker: &UITreeWalker, el: &UIElement, depth: usize, ctx: &mut Ctx) {
        if ctx.should_stop() {
            return;
        }
        ctx.visited += 1;
        let seq = ctx.raw.len();
        ctx.raw.push(snapshot(el, depth, seq));

        if depth >= MAX_DEPTH {
            ctx.capped = true;
            return;
        }

        // `get_first_child` returns Err for a leaf — that is the normal exit,
        // not an error condition, and UIA gives us no separate "has children"
        // predicate to check first.
        let mut child = match walker.get_first_child(el) {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            walk(walker, &child, depth + 1, ctx);
            if ctx.should_stop() {
                return;
            }
            child = match walker.get_next_sibling(&child) {
                Ok(n) => n,
                Err(_) => return,
            };
        }
    }

    /// The whole COM-touching body of a dump. Blocking by construction; the
    /// caller runs it on a blocking thread.
    ///
    /// # COM threading
    ///
    /// `UIAutomation::new()` calls `CoInitializeEx(None, COINIT_MULTITHREADED)`
    /// on the CURRENT thread and then `CoCreateInstance(CUIAutomation)`. Two
    /// consequences drive the design here:
    ///
    /// 1. It must not run on the UI thread. Tauri's main thread is an STA that
    ///    also pumps the window message loop; initialising it MTA would fail
    ///    with `RPC_E_CHANGED_MODE`, and even if it succeeded, a deep
    ///    cross-process tree walk is thousands of blocking COM round trips —
    ///    seconds of frozen UI. Hence `spawn_blocking`, matching the pattern
    ///    already used by `artifacts.rs`, `runs_local.rs` and `video.rs`.
    /// 2. MTA (rather than STA) is the right apartment for a worker thread that
    ///    does not pump messages. UIA marshals calls into the target process
    ///    either way; an STA here would need a message pump we do not have and
    ///    would deadlock against providers that call back.
    ///
    /// We deliberately do NOT call `CoUninitialize`. `spawn_blocking` hands out
    /// POOLED threads, so a thread that ran a dump may be reused for the next
    /// one — on reuse `CoInitializeEx` returns `S_FALSE` (already initialised,
    /// refcount bumped) which the crate treats as success, so repeat dumps work
    /// unchanged. Uninitialising would instead mean tearing down and rebuilding
    /// the apartment on every call, and — worse — a mismatched
    /// init/uninit pair on a pooled thread is a classic way to invalidate a
    /// COM pointer another task still holds. The cost of leaving it is one
    /// initialised apartment per blocking thread tokio keeps alive: bounded,
    /// idempotent, and released when the thread dies. Every COM object created
    /// here (`UIAutomation`, `UIElement`, `UITreeWalker`) is dropped before the
    /// closure returns, so nothing COM-owned ever crosses the thread boundary —
    /// only the plain `UiaDump`, which is why this compiles at all given that
    /// `UIElement` is `!Send`.
    pub fn dump_blocking(filtered: bool) -> UiaDump {
        let started = Instant::now();
        let empty: Vec<RawElement> = Vec::new();
        let fail = |note: &str, hwnd: i64| {
            build_dump(false, hwnd, String::new(), [0, 0, 0, 0], &empty, filtered, 0, false,
                       started.elapsed().as_millis() as u64, note)
        };

        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd == 0 {
            // Genuinely happens: during a desktop-switch, while the lock screen
            // or a secure-desktop UAC prompt is up, or if nothing is focused.
            return fail("no foreground window (locked screen, secure desktop, or nothing focused)", 0);
        }

        let automation = match UIAutomation::new() {
            Ok(a) => a,
            Err(e) => return fail(&format!("UIAutomation init failed: {e}"), hwnd as i64),
        };

        let root: UIElement = match automation.element_from_handle(Handle::from(hwnd)) {
            Ok(el) => el,
            // The most likely cause by far is UIPI: the foreground window
            // belongs to an elevated process and we are not elevated.
            Err(e) => {
                return fail(
                    &format!("could not read the foreground window's UIA element ({e}) — \
                              most likely an elevated window, which a non-elevated process \
                              cannot see (our input path cannot reach it either)"),
                    hwnd as i64,
                )
            }
        };

        let window_title = root.get_name().unwrap_or_default();
        let window_rect = match root.get_bounding_rectangle() {
            Ok(r) => [r.get_left(), r.get_top(), r.get_width(), r.get_height()],
            Err(_) => [0, 0, 0, 0],
        };

        let walker = match automation.get_control_view_walker() {
            Ok(w) => w,
            Err(e) => return fail(&format!("control view walker unavailable: {e}"), hwnd as i64),
        };

        let mut ctx = Ctx {
            raw: Vec::new(),
            visited: 0,
            capped: false,
            deadline: started + std::time::Duration::from_millis(WALK_BUDGET_MS),
        };
        walk(&walker, &root, 0, &mut ctx);

        let note = if ctx.capped {
            "walk stopped early (depth, node or time budget) — this dump is PARTIAL"
        } else if ctx.raw.len() <= 2 {
            "tree is effectively empty: the provider exposes nothing below the window root. \
             Expected for Electron/custom-drawn apps, and for browsers whose accessibility \
             tree has not been activated"
        } else {
            "ok"
        };

        build_dump(
            true,
            hwnd as i64,
            window_title,
            window_rect,
            &ctx.raw,
            filtered,
            ctx.visited,
            ctx.capped,
            started.elapsed().as_millis() as u64,
            note,
        )
    }
}

// ---------------------------------------------------------------------------
// Off-platform stub
// ---------------------------------------------------------------------------

/// macOS/Linux stub. There is no UI Automation outside Windows (macOS's
/// equivalent is AX/`AXUIElement`, a different API with a different permission
/// model — a separate job, not a port of this one).
///
/// It returns the same `UiaDump` shape rather than an error so that any caller,
/// including a devtools paste, sees one contract everywhere. It also runs the
/// real `build_dump` over an empty walk, which is not ceremony: it keeps the
/// pure filtering/ordering path compiled and exercised on the machine this was
/// written on, and stops it rotting behind a `cfg` nobody here can build.
#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn dump_blocking(filtered: bool) -> UiaDump {
        let empty: Vec<RawElement> = Vec::new();
        build_dump(
            false,
            0,
            String::new(),
            [0, 0, 0, 0],
            &empty,
            filtered,
            0,
            false,
            0,
            "UI Automation is Windows-only; this build has the stub path compiled in",
        )
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Filtered dump of the FOREGROUND window: the elements a model would plausibly
/// be offered as click targets, in reading order, capped at 60.
///
/// This is the shape the eventual grounding feature would use. It is exposed
/// now only so a human can eyeball it against a screenshot and decide whether
/// the idea survives contact with real apps. Nothing calls it from Rust.
#[tauri::command]
pub async fn uia_dump() -> Result<UiaDump, String> {
    tauri::async_runtime::spawn_blocking(|| imp::dump_blocking(true))
        .await
        .map_err(|e| format!("uia dump task panicked: {e}"))
}

/// Unfiltered dump: every non-zero-area node the walk saw, capped at 600.
///
/// The diagnostic twin of `uia_dump`. When the filtered list comes back thin,
/// this says which of the two stories is true — the tree is empty (nothing to
/// ground against, fall back to pixels) or the tree is full and our filter is
/// eating it (fix the filter). Those need opposite responses, and without this
/// command they are indistinguishable.
#[tauri::command]
pub async fn uia_dump_all() -> Result<UiaDump, String> {
    tauri::async_runtime::spawn_blocking(|| imp::dump_blocking(false))
        .await
        .map_err(|e| format!("uia dump task panicked: {e}"))
}

// ---------------------------------------------------------------------------
// Tests — pure logic only. Nothing here touches COM, so it runs on the Mac.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn el(ct: &str, name: &str, x: i32, y: i32, w: i32, h: i32) -> RawElement {
        RawElement {
            control_type: ct.to_string(),
            name: name.to_string(),
            automation_id: String::new(),
            class_name: String::new(),
            x,
            y,
            w,
            h,
            enabled: true,
            offscreen: false,
            depth: 1,
            seq: 0,
        }
    }

    /// Assign walk order the way the real walk does, so tests exercise the
    /// same `seq` invariant the dedupe relies on.
    fn seqd(mut v: Vec<RawElement>) -> Vec<RawElement> {
        for (i, e) in v.iter_mut().enumerate() {
            e.seq = i;
        }
        v
    }

    // ---- control type classification ----

    #[test]
    fn interactable_accepts_clickables_and_rejects_containers() {
        for ct in ["Button", "Edit", "MenuItem", "ListItem", "CheckBox", "TabItem", "Hyperlink"] {
            assert!(is_interactable(ct), "{ct} should be interactable");
        }
        for ct in ["Text", "Pane", "Group", "Window", "Image", "ToolBar", "Custom", "Unknown"] {
            assert!(!is_interactable(ct), "{ct} should not be interactable");
        }
    }

    // ---- rect math ----

    #[test]
    fn center_is_the_rect_middle() {
        assert_eq!(center(100, 200, 40, 20), (120, 210));
    }

    #[test]
    fn center_of_odd_sized_rect_stays_inside_the_rect() {
        // 10..=16 wide (w=7): centre must be within [10, 17).
        let (cx, cy) = center(10, 10, 7, 7);
        assert!(cx >= 10 && cx < 17, "cx {cx} escaped the rect");
        assert!(cy >= 10 && cy < 17, "cy {cy} escaped the rect");
    }

    #[test]
    fn center_handles_negative_origin_secondary_monitor() {
        // A monitor left of the primary reports negative x. The centre must
        // stay inside the rect there too.
        let (cx, cy) = center(-1920, -100, 100, 50);
        assert_eq!((cx, cy), (-1870, -75));
        assert!(cx > -1920 && cx < -1820);
    }

    #[test]
    fn one_pixel_rect_centers_on_itself() {
        assert_eq!(center(5, 5, 1, 1), (5, 5));
    }

    // ---- filtering ----

    #[test]
    fn zero_area_is_dropped() {
        assert!(!keep_filtered(&el("Button", "OK", 0, 0, 0, 20), 0));
        assert!(!keep_filtered(&el("Button", "OK", 0, 0, 20, 0), 0));
        // negative width: some providers report right < left on collapsed nodes
        assert!(!keep_filtered(&el("Button", "OK", 0, 0, -5, 20), 0));
    }

    #[test]
    fn offscreen_and_disabled_are_dropped() {
        let mut off = el("Button", "OK", 0, 0, 40, 20);
        off.offscreen = true;
        assert!(!keep_filtered(&off, 0));

        let mut dis = el("Button", "OK", 0, 0, 40, 20);
        dis.enabled = false;
        assert!(!keep_filtered(&dis, 0));
    }

    #[test]
    fn nameless_and_idless_is_dropped_but_edit_survives() {
        assert!(!keep_filtered(&el("Button", "", 0, 0, 40, 20), 0));
        assert!(!keep_filtered(&el("Button", "   ", 0, 0, 40, 20), 0));
        // an unnamed Edit is still a legitimate target
        assert!(keep_filtered(&el("Edit", "", 0, 0, 200, 24), 0));
        // an automation id is identity enough for an unnamed button
        let mut with_id = el("Button", "", 0, 0, 40, 20);
        with_id.automation_id = "closeBtn".into();
        assert!(keep_filtered(&with_id, 0));
    }

    #[test]
    fn window_sized_backdrop_is_dropped() {
        let window_area = 1000i64 * 800;
        // 99% of the window: a backdrop, not a target.
        assert!(!keep_filtered(&el("ListItem", "row", 0, 0, 1000, 795), window_area));
        // a normal row survives
        assert!(keep_filtered(&el("ListItem", "row", 0, 0, 1000, 24), window_area));
        // with no known window area the guard must not fire
        assert!(keep_filtered(&el("ListItem", "row", 0, 0, 1000, 795), 0));
    }

    #[test]
    fn unfiltered_keeps_everything_with_area() {
        let mut e = el("Text", "", 0, 0, 10, 10);
        e.offscreen = true;
        e.enabled = false;
        assert!(keep_unfiltered(&e), "unfiltered view must not apply the model filter");
        assert!(!keep_unfiltered(&el("Text", "", 0, 0, 0, 10)));
    }

    // ---- ordering ----

    #[test]
    fn reading_order_is_rows_then_columns_not_tree_order() {
        // Tree order here is deliberately scrambled relative to layout.
        let raw = seqd(vec![
            el("Button", "bottom-right", 300, 100, 40, 20),
            el("Button", "top-right", 300, 10, 40, 20),
            el("Button", "top-left", 10, 12, 40, 20), // same row as top-right (12 vs 10)
            el("Button", "bottom-left", 10, 100, 40, 20),
        ]);
        let (out, _, _) = refine(&raw, true, 60, 0);
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["top-left", "top-right", "bottom-left", "bottom-right"]);
    }

    #[test]
    fn near_identical_tops_share_a_row_band() {
        // y=10 and y=22 are 12px apart — inside ROW_BAND(16) of each other, but
        // they straddle the band boundary at 16. This is the known limitation of
        // fixed banding; the assertion pins CURRENT behaviour so a future change
        // to the ordering rule is a deliberate, visible edit rather than a
        // silent reshuffle of every dump.
        let raw = seqd(vec![
            el("Button", "b", 300, 22, 40, 20),
            el("Button", "a", 10, 10, 40, 20),
        ]);
        let (out, _, _) = refine(&raw, true, 60, 0);
        assert_eq!(out[0].name, "a");
        assert_eq!(out[1].name, "b");
    }

    #[test]
    fn ordering_is_deterministic_for_identical_positions() {
        // Two same-position, different-size controls: seq breaks the tie, so
        // repeated dumps of a static screen give a stable list.
        let raw = seqd(vec![
            el("Button", "first", 10, 10, 40, 20),
            el("Button", "second", 10, 10, 60, 20),
        ]);
        let (out, _, _) = refine(&raw, true, 60, 0);
        assert_eq!(out[0].name, "first");
    }

    #[test]
    fn negative_coordinates_band_downward_not_toward_zero() {
        // y=-20 and y=-4 must land in different bands and sort in visual order.
        // Truncating division would put both in band 0 and hide the difference.
        let raw = seqd(vec![
            el("Button", "lower", 10, -4, 40, 20),
            el("Button", "upper", 10, -20, 40, 20),
        ]);
        let (out, _, _) = refine(&raw, true, 60, 0);
        assert_eq!(out[0].name, "upper");
        assert_eq!(out[1].name, "lower");
    }

    // ---- dedupe ----

    #[test]
    fn identical_rects_collapse_to_the_shallowest_node() {
        // The classic UIA shape: a Button wrapping a same-rect label.
        let mut button = el("Button", "Save", 10, 10, 60, 24);
        button.depth = 3;
        let mut inner = el("ListItem", "Save", 10, 10, 60, 24);
        inner.depth = 4;
        let raw = seqd(vec![button, inner]);
        let (out, matched, _) = refine(&raw, true, 60, 0);
        assert_eq!(matched, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].control_type, "Button", "the wrapper, not its inner label");
        assert_eq!(out[0].depth, 3);
    }

    #[test]
    fn dedupe_survives_input_not_in_walk_order() {
        let mut inner = el("ListItem", "Save", 10, 10, 60, 24);
        inner.seq = 9;
        let mut button = el("Button", "Save", 10, 10, 60, 24);
        button.seq = 4;
        // deliberately passed inner-first
        let (out, _, _) = refine(&[inner, button], true, 60, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].control_type, "Button");
    }

    #[test]
    fn different_rects_are_not_deduped() {
        let raw = seqd(vec![
            el("Button", "Save", 10, 10, 60, 24),
            el("Button", "Save", 80, 10, 60, 24), // same name, different place
        ]);
        let (out, _, _) = refine(&raw, true, 60, 0);
        assert_eq!(out.len(), 2);
    }

    // ---- indexing and capping ----

    #[test]
    fn indices_are_dense_and_match_list_position() {
        let raw = seqd((0..10).map(|i| el("Button", &format!("b{i}"), i * 50, 0, 40, 20)).collect());
        let (out, _, trunc) = refine(&raw, true, 60, 0);
        assert!(!trunc);
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.index, i);
        }
    }

    #[test]
    fn cap_truncates_and_reports_the_full_match_count() {
        // 100 buttons stacked one row apart, so reading order == construction order.
        let raw = seqd((0..100).map(|i| el("Button", &format!("b{i}"), 0, i * 40, 40, 20)).collect());
        let (out, matched, trunc) = refine(&raw, true, FILTERED_CAP, 0);
        assert_eq!(matched, 100);
        assert_eq!(out.len(), FILTERED_CAP);
        assert!(trunc);
        // truncation keeps the TOP of the reading order, not an arbitrary slice
        assert_eq!(out[0].name, "b0");
        assert_eq!(out[FILTERED_CAP - 1].name, format!("b{}", FILTERED_CAP - 1));
        assert_eq!(out[FILTERED_CAP - 1].index, FILTERED_CAP - 1);
    }

    #[test]
    fn cap_boundary_is_not_off_by_one() {
        let raw = seqd((0..FILTERED_CAP).map(|i| el("Button", &format!("b{i}"), 0, (i as i32) * 40, 40, 20)).collect());
        let (out, matched, trunc) = refine(&raw, true, FILTERED_CAP, 0);
        assert_eq!(matched, FILTERED_CAP);
        assert_eq!(out.len(), FILTERED_CAP);
        assert!(!trunc, "exactly at the cap is not truncated");
    }

    // ---- empty / dump assembly ----

    #[test]
    fn empty_walk_produces_a_well_formed_dump() {
        let d = build_dump(true, 42, "Notepad".into(), [0, 0, 800, 600], &[], true, 0, false, 3, "ok");
        assert!(d.ok);
        assert_eq!(d.hwnd, 42);
        assert_eq!(d.matched, 0);
        assert!(!d.truncated);
        assert!(d.elements.is_empty());
        assert!(d.filtered);
    }

    #[test]
    fn dump_click_points_are_the_rect_centers() {
        let raw = seqd(vec![el("Button", "OK", 100, 200, 40, 20)]);
        let d = build_dump(true, 1, "w".into(), [0, 0, 800, 600], &raw, true, 1, false, 1, "ok");
        assert_eq!((d.elements[0].cx, d.elements[0].cy), (120, 210));
    }

    #[test]
    fn dump_serializes_to_json_with_the_documented_field_names() {
        // The operator reads this JSON by hand out of a devtools console, so
        // the key names are part of the contract.
        let raw = seqd(vec![el("Button", "OK", 1, 2, 3, 4)]);
        let d = build_dump(true, 7, "w".into(), [0, 0, 800, 600], &raw, true, 1, false, 1, "ok");
        let j = serde_json::to_value(&d).unwrap();
        for k in ["ok", "hwnd", "window_title", "window_rect", "filtered", "visited",
                  "matched", "truncated", "walk_capped", "elapsed_ms", "note", "elements"] {
            assert!(j.get(k).is_some(), "missing dump field {k}");
        }
        let e = &j["elements"][0];
        for k in ["index", "control_type", "name", "automation_id", "class_name",
                  "x", "y", "w", "h", "cx", "cy", "enabled", "offscreen", "depth"] {
            assert!(e.get(k).is_some(), "missing element field {k}");
        }
        // empty strings become null rather than ""
        assert!(e["automation_id"].is_null());
    }

    /// The off-platform stub must still answer with the full shape (this test
    /// only runs where the stub is compiled, i.e. this Mac).
    #[cfg(not(windows))]
    #[test]
    fn stub_returns_a_shaped_not_ok_dump() {
        let d = imp::dump_blocking(true);
        assert!(!d.ok);
        assert!(d.elements.is_empty());
        assert!(d.note.contains("Windows-only"));
    }
}
