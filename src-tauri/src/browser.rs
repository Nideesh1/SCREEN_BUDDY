//! browser.rs — launch a browser whose PAGE CONTENT the accessibility tree can
//! actually see.
//!
//! # Why this exists
//!
//! `uia.rs` can name and rect every control in the foreground window, which is
//! the difference between the model guessing pixel coordinates and citing
//! "element 7". On the operator's Windows worker we measured exactly what that
//! is worth in a browser:
//!
//! - Chrome launched WITH `--force-renderer-accessibility`: 31 elements,
//!   including page content — "Google Search", "I'm Feeling Lucky", the search
//!   combobox, the footer links, each with an exact screen rect.
//! - Chrome launched WITHOUT it: browser chrome only — tabs, reload, the
//!   address bar. The page itself is invisible to UIA.
//!
//! Chromium gates the renderer-side accessibility tree behind lazy activation:
//! it materialises only when a client asks for it in a way Chromium accepts (a
//! detected screen reader, or this flag). The `ForceRendererAccessibility`
//! **registry policy was REMOVED in Chrome 152** — `chrome://policy` now reports
//! it as "Unknown policy" — so there is no machine-wide switch left to flip.
//! The command-line flag is the only remaining route, and a flag can only be
//! applied by whoever starts the process. Hence: the agent must start the
//! browser itself. Most real work happens in a browser, so this is load-bearing.
//!
//! # The trap this module exists to avoid
//!
//! Chromium has a singleton-per-profile design. If a Chrome is ALREADY running
//! on the default profile and you run
//! `chrome.exe --force-renderer-accessibility https://example.com`, the new
//! process finds the existing one's singleton, hands it the URL, and exits.
//! **Your flag is silently discarded.** You get a new tab in an
//! accessibility-less browser, `uia_dump` comes back with browser chrome only,
//! and nothing anywhere reports an error. That is the worst possible failure
//! mode: everything downstream looks broken for no discoverable reason.
//!
//! Our answer is an **isolated profile**: every launch passes
//! `--user-data-dir=<app_data>/browser_profile`. A different user-data-dir is a
//! different singleton, so the process we start is genuinely our own and the
//! flag genuinely applies, no matter what else is running.
//!
//! The cost is real and worth stating plainly: our profile is NOT the user's
//! profile. No existing cookies, logins, extensions, bookmarks or saved
//! passwords. A task that needs an authenticated session must sign in inside our
//! profile once — the directory persists across launches, so that sign-in
//! sticks — rather than inheriting the operator's. We took that cost because the
//! alternatives are worse: refusing to launch while any Chrome is running makes
//! the capability unavailable exactly when the worker is being used, and
//! launching into the shared profile is the silent failure above.
//!
//! We still LOOK for a foreign browser instance and say so in the report, even
//! though the isolated profile makes it harmless. The operator will see two
//! Chrome windows and needs to know which one is the flagged one; the model
//! needs to know that the browser it can see in a screenshot may not be the one
//! it opened.
//!
//! # Platform scope
//!
//! Windows is the live worker. macOS works for real (we exec the binary inside
//! the .app bundle directly rather than going through `open -a`, so the args
//! reach Chromium and we get a real child handle to track). Linux is
//! best-effort via `PATH`. The pure logic — lookup order, argument
//! construction, URL validation, the reuse decision — is compiled and unit
//! tested on every platform, including this Mac, so it cannot rot behind a
//! `cfg` nobody here can build.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// Directory name under app data for our isolated Chromium profile. Persisted
/// (never wiped between launches) so a sign-in performed inside it survives —
/// that is the only thing that makes the isolated-profile trade-off tolerable.
const PROFILE_DIR: &str = "browser_profile";

/// Environment variable that overrides executable discovery entirely. Escape
/// hatch for the installs our search cannot predict (portable Chrome on a USB
/// stick, a locked-down image with a relocated Program Files). Named in the
/// not-found error so the operator can act on it without reading this file.
const EXE_OVERRIDE_ENV: &str = "SCREENBUDDY_BROWSER";

/// How long to wait for a URL-handoff process to exit before giving up on it.
/// When our instance is already running, re-running the exe with the same
/// `--user-data-dir` starts a short-lived process that passes the URL through
/// the singleton and exits — normally in well under a second. The wait exists
/// only so the report can honestly say whether the handoff was accepted.
const HANDOFF_WAIT_MS: u64 = 2000;
const HANDOFF_POLL_MS: u64 = 50;

/// Longest URL we will pass on the command line. Chromium itself accepts far
/// more, but a URL this long from a model is a bug or an injection attempt, and
/// Windows' command line has a hard limit we should stay nowhere near.
const MAX_URL_LEN: usize = 2048;

/// PATH separator for this platform. Split out as a value (rather than baked
/// into the splitter) so `path_candidates` can be tested with both conventions
/// on one machine.
const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Which Chromium we are driving. Chrome first, Edge as fallback: Edge is the
/// same engine, takes the identical `--force-renderer-accessibility` and
/// `--user-data-dir` flags, and ships with Windows — so a worker image with no
/// Chrome still gets an accessibility-capable browser instead of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserKind {
    Chrome,
    Edge,
}

impl BrowserKind {
    fn label(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Chrome",
            BrowserKind::Edge => "Edge",
        }
    }
}

/// The outcome of one `launch_browser`. `summary` is the single line handed to
/// the model as the tool result; every other field is for the operator's
/// console and for tests. Always this shape, on every platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchReport {
    pub ok: bool,
    pub kind: Option<BrowserKind>,
    /// Absolute path of the executable we actually ran — the fastest way to
    /// tell a per-user Chrome from a per-machine one when a worker misbehaves.
    pub exe: Option<String>,
    pub pid: Option<u32>,
    /// True when this process was started by us with
    /// `--force-renderer-accessibility`, i.e. when aiming by element inside the
    /// page is available. Recorded at launch, never inferred later.
    pub accessibility_forced: bool,
    /// True when we handed the URL to an instance WE had already launched
    /// (whose renderer accessibility is therefore already on) rather than
    /// starting a new process. This is reuse, not the discarded-flag trap.
    pub reused: bool,
    pub profile_dir: String,
    /// A browser of the same kind is running that we did not start. Harmless
    /// (different profile, different singleton) but visible on screen, so both
    /// the model and the operator are told rather than left to guess.
    pub other_instance_running: bool,
    pub url: Option<String>,
    pub summary: String,
}

/// Answer to "can I aim by element in a browser this turn?". Cheap: one
/// `try_wait` on our child plus one process listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserStatus {
    /// True when a browser we started is alive right now.
    pub ours_running: bool,
    pub pid: Option<u32>,
    pub kind: Option<BrowserKind>,
    pub accessibility_forced: bool,
    /// Last URL we asked our instance to open. Not "the current tab" — the user
    /// or the model may have navigated since; it is the last thing WE asked for.
    pub last_url: Option<String>,
    pub launched_at: Option<String>,
    pub other_instance_running: bool,
    pub note: String,
}

// ---------------------------------------------------------------------------
// Tracked instance
// ---------------------------------------------------------------------------

/// The browser this process started, if any.
///
/// A module-level static rather than Tauri managed state on purpose: this is
/// one optional child process with no initialization order to respect, and
/// keeping it here means the only edit any other file needs is the command
/// registration. `Child` is retained (not just the pid) because `try_wait` is
/// the only liveness check that cannot lie to us — a raw pid can be recycled by
/// the OS and would eventually have us reporting a stranger's process as ours.
static TRACKED: Mutex<Option<Tracked>> = Mutex::new(None);

struct Tracked {
    child: Child,
    pid: u32,
    kind: BrowserKind,
    /// Recorded, not assumed: every current launch path sets the flag, but a
    /// status read must report what was actually done rather than what this
    /// version of the code happens to do.
    accessibility_forced: bool,
    last_url: Option<String>,
    launched_at: String,
}

/// Lock the tracked slot and reap it if the browser has exited. Returns the
/// guard so callers can inspect and mutate under the same lock — checking
/// liveness and then acting on it must not be two separate critical sections.
fn tracked_reaped() -> std::sync::MutexGuard<'static, Option<Tracked>> {
    // A poisoned lock here means a previous caller panicked mid-update. The
    // worst-case content is a stale child handle, which the try_wait below
    // resolves anyway, so recovering beats propagating a panic into the agent
    // loop.
    let mut guard = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    let exited = match guard.as_mut() {
        Some(t) => matches!(t.child.try_wait(), Ok(Some(_)) | Err(_)),
        None => false,
    };
    if exited {
        *guard = None;
    }
    guard
}

// ---------------------------------------------------------------------------
// Pure logic: executable lookup
// ---------------------------------------------------------------------------

/// Executable file names for one browser on this platform, most-preferred
/// first. Distro packaging is why Linux gets a list: `google-chrome` and
/// `google-chrome-stable` are both real, and Chromium is a legitimate
/// last-resort (same flags, same engine).
fn exe_names(kind: BrowserKind) -> &'static [&'static str] {
    #[cfg(windows)]
    match kind {
        BrowserKind::Chrome => &["chrome.exe"],
        BrowserKind::Edge => &["msedge.exe"],
    }
    #[cfg(target_os = "macos")]
    match kind {
        // The binary inside the bundle. We exec it directly instead of
        // `open -a "Google Chrome" --args …` because `open` returns immediately
        // with no child handle (nothing to track, nothing to `try_wait`) and
        // silently drops `--args` onto an already-running instance — the same
        // discarded-flag trap this module exists to avoid.
        BrowserKind::Chrome => &["Google Chrome"],
        BrowserKind::Edge => &["Microsoft Edge"],
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    match kind {
        BrowserKind::Chrome => &["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"],
        BrowserKind::Edge => &["microsoft-edge", "microsoft-edge-stable"],
    }
}

/// Well-known absolute install locations for one browser, in the order we try
/// them.
///
/// Windows order is per-machine before per-user: a per-machine install under
/// Program Files is the enterprise-managed shape and is the same binary for
/// every account on the box, whereas `LOCALAPPDATA` is what a self-service
/// install leaves behind for one user. When both exist they are almost always
/// the same build, so the order matters mainly for determinism — but a
/// deterministic answer is exactly what a fleet needs when one worker behaves
/// differently from the rest.
///
/// Takes an environment accessor rather than reading `std::env` so the ordering
/// is testable on a machine that has none of these variables.
#[cfg(any(windows, test))]
fn windows_candidates(kind: BrowserKind, env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let suffix: &[&str] = match kind {
        BrowserKind::Chrome => &["Google", "Chrome", "Application", "chrome.exe"],
        BrowserKind::Edge => &["Microsoft", "Edge", "Application", "msedge.exe"],
    };
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|var| env(var))
        .map(|root| suffix.iter().fold(PathBuf::from(root), |p, seg| p.join(seg)))
        .collect()
}

/// macOS install locations: the system-wide `/Applications` copy first, then
/// the per-user `~/Applications` one, which is where a Chrome installed without
/// admin rights lands.
#[cfg(any(target_os = "macos", test))]
fn macos_candidates(kind: BrowserKind, env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let (app, bin) = match kind {
        BrowserKind::Chrome => ("Google Chrome.app", "Google Chrome"),
        BrowserKind::Edge => ("Microsoft Edge.app", "Microsoft Edge"),
    };
    let mut out = vec![PathBuf::from("/Applications").join(app).join("Contents/MacOS").join(bin)];
    if let Some(home) = env("HOME") {
        out.push(PathBuf::from(home).join("Applications").join(app).join("Contents/MacOS").join(bin));
    }
    out
}

/// Every `<dir>/<name>` pair a PATH variable implies, in PATH order then name
/// order. Empty PATH entries are skipped: on Windows a stray `;;` would
/// otherwise resolve to a bare relative name and pick up whatever sits in the
/// current directory.
fn path_candidates(path_var: &str, sep: char, names: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in path_var.split(sep) {
        if dir.is_empty() {
            continue;
        }
        for name in names {
            out.push(Path::new(dir).join(name));
        }
    }
    out
}

/// Pull the `(Default)` value out of `reg query … /ve` output.
///
/// The registry's `App Paths` key is the authoritative answer for an install in
/// a location nothing else predicts, and reading it costs one subprocess with
/// no new crate. Output looks like:
///
/// ```text
/// HKEY_LOCAL_MACHINE\SOFTWARE\…\App Paths\chrome.exe
///     (Default)    REG_SZ    C:\Program Files\Google\Chrome\Application\chrome.exe
/// ```
///
/// The value itself may contain spaces, so we split on the type token and take
/// everything after it rather than splitting on whitespace.
#[cfg(any(windows, test))]
fn parse_reg_default(out: &str) -> Option<String> {
    for line in out.lines() {
        if !line.contains("(Default)") {
            continue;
        }
        for ty in ["REG_SZ", "REG_EXPAND_SZ"] {
            if let Some((_, value)) = line.split_once(ty) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Ask the registry where a browser is installed. Windows-only, and a best
/// effort: any failure just means this lookup step found nothing.
#[cfg(windows)]
fn registry_app_path(exe: &str) -> Option<PathBuf> {
    // HKCU first: a per-user install writes its own App Paths entry, and when
    // both exist the per-user one is the copy this account actually launches.
    for root in ["HKCU", "HKLM"] {
        let key = format!(
            "{root}\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}"
        );
        let out = no_window_command(Path::new("reg")).args(["query", &key, "/ve"]).output().ok();
        if let Some(out) = out {
            if out.status.success() {
                if let Some(p) = parse_reg_default(&String::from_utf8_lossy(&out.stdout)) {
                    return Some(PathBuf::from(p));
                }
            }
        }
    }
    None
}

/// Well-known locations for this platform. One function so `find_browser` has
/// no `cfg` in its body.
fn fixed_candidates(kind: BrowserKind, env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    #[cfg(windows)]
    return windows_candidates(kind, env);
    #[cfg(target_os = "macos")]
    return macos_candidates(kind, env);
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        // Linux has no stable absolute layout; PATH is the convention. Named
        // arguments still consumed so the signature is uniform.
        let _ = (kind, env);
        Vec::new()
    }
}

/// Locate a browser, or explain precisely what was searched.
///
/// Order: `SCREENBUDDY_BROWSER` override → Chrome (well-known paths, then PATH,
/// then the registry) → Edge (same three). Chrome is first because it is what
/// we measured; Edge is a real fallback rather than a courtesy because it takes
/// the identical flags.
fn find_browser(env: &dyn Fn(&str) -> Option<String>) -> Result<(BrowserKind, PathBuf), String> {
    find_browser_with(env, &|p| p.is_file())
}

/// `find_browser` with the filesystem injected. Split out purely so the lookup
/// ORDER can be tested: this Mac has a real `/Applications/Google Chrome.app`,
/// so a test against the real filesystem would assert whatever happens to be
/// installed on the machine running it.
fn find_browser_with(
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<(BrowserKind, PathBuf), String> {
    if let Some(over) = env(EXE_OVERRIDE_ENV) {
        let p = PathBuf::from(&over);
        if !exists(&p) {
            // Loud rather than silently falling through to the search: someone
            // set this deliberately, and quietly ignoring it would send them
            // hunting the wrong problem.
            return Err(format!(
                "{EXE_OVERRIDE_ENV} is set to `{over}` but that is not a file. \
                 Point it at a Chrome/Edge executable or unset it."
            ));
        }
        // The override cannot say which engine flavour it is; Chrome is the
        // right guess and only affects the label in reports.
        return Ok((BrowserKind::Chrome, p));
    }

    let mut searched: Vec<String> = Vec::new();
    for kind in [BrowserKind::Chrome, BrowserKind::Edge] {
        for p in fixed_candidates(kind, env) {
            if exists(&p) {
                return Ok((kind, p));
            }
            searched.push(p.display().to_string());
        }
        if let Some(path_var) = env("PATH") {
            for p in path_candidates(&path_var, PATH_SEP, exe_names(kind)) {
                if exists(&p) {
                    return Ok((kind, p));
                }
            }
        }
        #[cfg(windows)]
        if let Some(p) = registry_app_path(exe_names(kind)[0]) {
            if exists(&p) {
                return Ok((kind, p));
            }
        }
    }

    Err(format!(
        "No Chrome or Edge executable found. Searched: {}; every directory on PATH for {}; \
         and the registry App Paths keys. Install Google Chrome, or set {EXE_OVERRIDE_ENV} to \
         the full path of a Chromium-based browser executable.",
        if searched.is_empty() { "(no well-known locations on this platform)".to_string() } else { searched.join(", ") },
        exe_names(BrowserKind::Chrome).join("/"),
    ))
}

// ---------------------------------------------------------------------------
// Pure logic: arguments and URL validation
// ---------------------------------------------------------------------------

/// The command line for a launch.
///
/// - `--force-renderer-accessibility` is the entire point of this module.
/// - `--user-data-dir` is what makes the flag actually apply (see the module
///   docs): a distinct profile is a distinct singleton, so we get our own
///   process instead of handing a URL to someone else's flagless one.
/// - `--no-first-run` / `--no-default-browser-check` suppress the welcome tab
///   and the "make Chrome your default?" bubble. Our profile is brand new on
///   first launch, so without these the model's first screenshot is a wizard,
///   not the page it asked for.
///
/// The URL goes last, as Chromium expects a positional URL after its switches.
/// Nothing here is shell-escaped and nothing needs to be: these become argv
/// entries directly (`Command::arg`), there is no shell anywhere in the path,
/// and `validate_url` has already refused anything that could be read as a
/// switch.
fn build_args(profile: &Path, url: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--force-renderer-accessibility".to_string(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    if let Some(u) = url {
        args.push(u.to_string());
    }
    args
}

/// Accept only a plain `http`/`https` URL.
///
/// Deliberately strict, because this string comes from a model and lands on a
/// command line:
/// - A leading `-` would be parsed by Chromium as a switch, letting a crafted
///   "URL" turn into `--load-extension=…`. The scheme check refuses it, and
///   this is the reason the check exists at all.
/// - Whitespace and control characters would split or corrupt the argument on
///   Windows' string-based command line, and no legal URL contains them raw.
/// - `file:`, `javascript:` and `chrome:` are refused: a browser opened to
///   drive the web has no business being pointed at the local disk, the page's
///   own JS context, or the browser's internals by a model.
fn validate_url(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("url is empty".into());
    }
    if url.len() > MAX_URL_LEN {
        return Err(format!("url is {} chars; limit is {MAX_URL_LEN}", url.len()));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url contains whitespace or control characters".into());
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(format!(
            "url must start with http:// or https:// (got `{}`)",
            &url[..url.len().min(40)]
        ));
    }
    // A scheme with nothing after it is not a destination, and Chromium would
    // open a blank tab that looks like a successful navigation.
    if lower.trim_start_matches("https://").trim_start_matches("http://").is_empty() {
        return Err("url has a scheme but no host".into());
    }
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Pure logic: the reuse decision
// ---------------------------------------------------------------------------

/// What a launch should do about processes that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchPlan {
    /// A browser WE launched is alive. Re-running the exe against the same
    /// profile hands the URL to it through the singleton. The flag on that
    /// second invocation is indeed discarded — but the target process already
    /// has renderer accessibility on, so this is genuine reuse and the
    /// capability is intact.
    HandToOurs,
    /// Nothing of ours is alive. Start a new process. Because it is on our own
    /// user-data-dir, no other running browser can absorb it, so the flag
    /// takes effect regardless of what else is on screen.
    Fresh,
}

/// The whole decision, isolated so it can be tested without a browser.
///
/// Note what does NOT appear here: whether a foreign browser is running. With
/// the isolated profile it cannot affect the outcome, so letting it change the
/// plan would only add a way to fail. It is reported, never acted on.
fn plan_launch(ours_alive: bool) -> LaunchPlan {
    if ours_alive {
        LaunchPlan::HandToOurs
    } else {
        LaunchPlan::Fresh
    }
}

// ---------------------------------------------------------------------------
// Process listing (impure, with a pure parser)
// ---------------------------------------------------------------------------

/// A `Command` that never flashes a console window on Windows. Same precedent
/// and same constant as `video::sidecar_command`: `tasklist` and `reg` are
/// console programs, and without this each status read pops a black window over
/// whatever the worker is driving — and into the next screenshot.
fn no_window_command(bin: &Path) -> Command {
    // The `mut` is only used inside the Windows block below; off-platform it
    // would warn. (`video::sidecar_command` carries that warning; this does
    // not need to repeat it.)
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut cmd = Command::new(bin);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Is any process other than `our_pid` present in `tasklist /NH /FO CSV`
/// output?
///
/// A COUNT would be meaningless: Chromium runs one process per tab, per
/// extension and per GPU/utility service, so a single browser window is a dozen
/// `chrome.exe` rows. All we can honestly answer is "yes, there is browser
/// activity we did not start", which is exactly what the report claims. Rows
/// look like `"chrome.exe","4312","Console","1","250,000 K"`; when nothing
/// matches, tasklist prints an `INFO:` line instead.
#[cfg(any(windows, test))]
fn tasklist_has_other(out: &str, our_pid: Option<u32>) -> bool {
    out.lines().any(|line| {
        let mut fields = line.split(',').map(|f| f.trim().trim_matches('"'));
        let Some(name) = fields.next() else { return false };
        if !name.to_ascii_lowercase().ends_with(".exe") {
            return false;
        }
        match (fields.next().and_then(|p| p.parse::<u32>().ok()), our_pid) {
            (Some(pid), Some(ours)) => pid != ours,
            (Some(_), None) => true,
            (None, _) => false,
        }
    })
}

/// Same question for `pgrep -x` output: one pid per line.
///
/// Only the non-Windows branch below calls this, but the tests exercise it on
/// every platform — a parser is worth checking wherever it is read, and the
/// alternative (gating the tests too) would leave it unverified on the machine
/// most of this work happens on. `cfg(any(...))` rather than `allow(dead_code)`
/// so a genuine future orphaning still shows up as a warning.
#[cfg(any(not(windows), test))]
fn pgrep_has_other(out: &str, our_pid: Option<u32>) -> bool {
    out.lines().filter_map(|l| l.trim().parse::<u32>().ok()).any(|pid| Some(pid) != our_pid)
}

/// Is a browser of this kind running that we did not start?
///
/// Best-effort by contract: if the probe fails we say "no", because a false
/// alarm in the report is worse than a missing note — the model would be told
/// to distrust the only browser on screen.
fn other_instance_running(kind: BrowserKind, our_pid: Option<u32>) -> bool {
    #[cfg(windows)]
    {
        let name = exe_names(kind)[0];
        let out = no_window_command(Path::new("tasklist"))
            .args(["/FI", &format!("IMAGENAME eq {name}"), "/NH", "/FO", "CSV"])
            .output();
        match out {
            Ok(o) => tasklist_has_other(&String::from_utf8_lossy(&o.stdout), our_pid),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        // `-x` is an exact match on the process name, which keeps "Google
        // Chrome Helper" (there are dozens) out of the answer.
        exe_names(kind).iter().any(|name| {
            match no_window_command(Path::new("pgrep")).args(["-x", name]).output() {
                Ok(o) => pgrep_has_other(&String::from_utf8_lossy(&o.stdout), our_pid),
                Err(_) => false,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

fn profile_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join(PROFILE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create browser profile dir: {e}"))?;
    Ok(dir)
}

/// The one line the model reads. It has to carry three facts and nothing else:
/// whether element-level aiming is available, in WHICH window, and whether
/// another browser on screen is a decoy.
fn summarize(kind: BrowserKind, pid: u32, reused: bool, url: Option<&str>, other: bool) -> String {
    let name = kind.label();
    let mut s = if reused {
        format!("Reused the {name} window this agent already opened (pid {pid}); renderer accessibility is on in it.")
    } else {
        format!("Launched {name} with renderer accessibility forced on (pid {pid}, isolated profile).")
    };
    if let Some(u) = url {
        s.push_str(&format!(" Opened {u}."));
    }
    s.push_str(" Page content in this window is visible to the accessibility tree, so elements can be targeted by name instead of estimated pixel coordinates.");
    if other {
        s.push_str(&format!(
            " NOTE: another {name} is also running that this agent did not start. That one does NOT have accessibility forced and uses a different profile (different logins). Work in the window this action opened."
        ));
    }
    s
}

/// Launch (or reuse) a browser with renderer accessibility forced on.
///
/// Blocking: it spawns a process and polls a short handoff. Callers on an async
/// path must wrap it in `spawn_blocking`; `dispatch_action` is already
/// synchronous, so the agent hook can call it directly.
pub(crate) fn launch_blocking(app: &AppHandle, url: Option<&str>) -> Result<LaunchReport, String> {
    let url = match url {
        Some(u) => Some(validate_url(u)?),
        None => None,
    };
    let profile = profile_dir(app)?;

    // Hold the tracked slot across the whole decision: checking "is ours alive"
    // and then acting on the answer must be one critical section, or two
    // concurrent launches both decide Fresh and we end up with two browsers and
    // one handle.
    let mut guard = tracked_reaped();

    match plan_launch(guard.is_some()) {
        LaunchPlan::HandToOurs => {
            let t = guard.as_mut().expect("plan_launch said ours is alive");
            let (kind, pid) = (t.kind, t.pid);
            // Without a URL there is nothing to hand over; the window is
            // already open and already flagged, so report and stop.
            if let Some(u) = url.as_deref() {
                let (_, exe) = find_browser(&|k| std::env::var(k).ok())?;
                hand_off_url(&exe, &profile, u)?;
                t.last_url = Some(u.to_string());
            }
            let other = other_instance_running(kind, Some(pid));
            Ok(LaunchReport {
                ok: true,
                kind: Some(kind),
                exe: None,
                pid: Some(pid),
                accessibility_forced: t.accessibility_forced,
                reused: true,
                profile_dir: profile.display().to_string(),
                other_instance_running: other,
                url: url.clone(),
                summary: summarize(kind, pid, true, url.as_deref(), other),
            })
        }
        LaunchPlan::Fresh => {
            let (kind, exe) = find_browser(&|k| std::env::var(k).ok())?;
            let args = build_args(&profile, url.as_deref());
            let child = no_window_command(&exe)
                .args(&args)
                .spawn()
                .map_err(|e| format!("failed to start {} ({}): {e}", kind.label(), exe.display()))?;
            let pid = child.id();
            let other = other_instance_running(kind, Some(pid));
            *guard = Some(Tracked {
                child,
                pid,
                kind,
                accessibility_forced: true,
                last_url: url.clone(),
                launched_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            });
            Ok(LaunchReport {
                ok: true,
                kind: Some(kind),
                exe: Some(exe.display().to_string()),
                pid: Some(pid),
                accessibility_forced: true,
                reused: false,
                profile_dir: profile.display().to_string(),
                other_instance_running: other,
                url: url.clone(),
                summary: summarize(kind, pid, false, url.as_deref(), other),
            })
        }
    }
}

/// Pass a URL to our already-running instance through Chromium's singleton.
///
/// The spawned process normally exits immediately after delivering the URL. We
/// poll for that so a failure is visible; if it is still alive at the deadline
/// we return Ok anyway — the URL almost certainly arrived, and the caller's
/// report is about the tracked window, which is unaffected either way. The
/// handle is dropped, which on Unix leaves a short-lived zombie until the app
/// exits; acceptable for a process that reaps itself in milliseconds, and
/// Windows (the live worker) has no such notion.
fn hand_off_url(exe: &Path, profile: &Path, url: &str) -> Result<(), String> {
    let args = build_args(profile, Some(url));
    let mut child = no_window_command(exe)
        .args(&args)
        .spawn()
        .map_err(|e| format!("failed to hand url to the running browser: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(HANDOFF_WAIT_MS);
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return Ok(()),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(HANDOFF_POLL_MS)),
        }
    }
    Ok(())
}

/// Is a browser we started still up, and does it have accessibility forced?
fn status_blocking() -> BrowserStatus {
    let guard = tracked_reaped();
    match guard.as_ref() {
        Some(t) => {
            let other = other_instance_running(t.kind, Some(t.pid));
            BrowserStatus {
                ours_running: true,
                pid: Some(t.pid),
                kind: Some(t.kind),
                accessibility_forced: t.accessibility_forced,
                last_url: t.last_url.clone(),
                launched_at: Some(t.launched_at.clone()),
                other_instance_running: other,
                note: if other {
                    "A browser we started is running with renderer accessibility forced on. Another browser we did not start is also running; it has a different profile and does NOT expose page content to the accessibility tree.".into()
                } else {
                    "A browser we started is running with renderer accessibility forced on; page elements can be targeted by name.".into()
                },
            }
        }
        None => {
            // Report on Chrome specifically here: we have no launch to tell us
            // which kind is relevant, and Chrome is what we would launch.
            let other = other_instance_running(BrowserKind::Chrome, None);
            BrowserStatus {
                ours_running: false,
                pid: None,
                kind: None,
                accessibility_forced: false,
                last_url: None,
                launched_at: None,
                other_instance_running: other,
                note: if other {
                    // The honest half of the trap: we cannot inspect a foreign
                    // process's flags, and assuming the worst is right, because
                    // the default is off.
                    "No browser started by this agent is running. A browser IS running that we did not start — assume its page content is NOT visible to the accessibility tree. Use launch_browser to open one that is.".into()
                } else {
                    "No browser is running. Use launch_browser to open one with page content visible to the accessibility tree.".into()
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Agent hook
// ---------------------------------------------------------------------------
//
// NOT wired into `agent.rs` from here. That file is being edited concurrently,
// and a merge conflict in the agent loop is a worse outcome than a one-line
// hook. The two functions below are everything the wiring needs; see the
// module-level note in the task report for the exact lines.
//
// Note that this CANNOT be an action on the `computer` tool: that tool is
// Anthropic's server-defined `computer_20251124`, which carries no
// `input_schema` — its action list lives inside the model and we cannot add to
// it. So this is a separate custom tool, exactly like `use_credential`.

/// The tool schema the model sees. The description spends its words on the one
/// thing the model cannot discover for itself: that page elements are only
/// nameable in a window this tool opened.
pub(crate) fn tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "launch_browser",
        "description": "Open a web browser whose page content is visible to the accessibility \
tree, so page elements can be targeted by name instead of by estimated pixel coordinates. Use \
this INSTEAD of clicking a browser icon or an existing browser window: a browser started any \
other way exposes only its own toolbar (tabs, reload, address bar) and none of the page. The \
window this opens uses a separate profile, so it does not inherit existing logins — sign in \
inside it once if the task needs an account. Calling this again reuses the window it already \
opened and navigates it.",
        "input_schema": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Optional http:// or https:// URL to open. Omit to just open the browser."
                }
            },
            "required": []
        }
    })
}

/// Run one `launch_browser` tool call and return the text for its tool_result.
///
/// Blocking, like the credential injection beside which it sits: it spawns
/// a process and may poll a URL handoff for up to two seconds. That is bounded
/// and happens once per call, not per turn.
pub(crate) fn tool_result_text(
    app: &AppHandle,
    input: &serde_json::Value,
) -> Result<String, String> {
    let url = input.get("url").and_then(|u| u.as_str()).filter(|u| !u.trim().is_empty());
    launch_blocking(app, url).map(|r| r.summary)
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Open a browser whose page content the accessibility tree can see, optionally
/// at `url`. Off the main thread: it spawns processes and shells out to a
/// process listing.
#[tauri::command]
pub async fn launch_browser(app: AppHandle, url: Option<String>) -> Result<LaunchReport, String> {
    tauri::async_runtime::spawn_blocking(move || launch_blocking(&app, url.as_deref()))
        .await
        .map_err(|e| format!("launch browser task panicked: {e}"))?
}

/// Whether aiming by element is available in a browser right now.
#[tauri::command]
pub async fn browser_status() -> Result<BrowserStatus, String> {
    tauri::async_runtime::spawn_blocking(status_blocking)
        .await
        .map_err(|e| format!("browser status task panicked: {e}"))
}

// ---------------------------------------------------------------------------
// Tests — pure logic only. No process is spawned, so these run on any machine.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment made of a fixed table, so lookup order is testable on a
    /// machine that has none of the real variables.
    fn fake_env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn windows_lookup_prefers_per_machine_then_per_user() {
        let env = fake_env(&[
            ("ProgramFiles", "C:\\Program Files"),
            ("ProgramFiles(x86)", "C:\\Program Files (x86)"),
            ("LOCALAPPDATA", "C:\\Users\\w\\AppData\\Local"),
        ]);
        let got: Vec<String> = windows_candidates(BrowserKind::Chrome, &env)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(got.len(), 3);
        assert!(got[0].starts_with("C:\\Program Files"), "per-machine first: {got:?}");
        assert!(got[1].contains("(x86)"), "then 32-bit: {got:?}");
        assert!(got[2].contains("AppData"), "per-user last: {got:?}");
        assert!(got.iter().all(|p| p.ends_with("chrome.exe")));
    }

    /// A missing variable drops its candidate instead of producing a path
    /// rooted at nothing — a bare `Google\Chrome\...` would resolve against the
    /// current directory.
    #[test]
    fn windows_lookup_skips_absent_variables() {
        let env = fake_env(&[("LOCALAPPDATA", "C:\\Users\\w\\AppData\\Local")]);
        let got = windows_candidates(BrowserKind::Edge, &env);
        assert_eq!(got.len(), 1);
        assert!(got[0].display().to_string().ends_with("msedge.exe"));
    }

    #[test]
    fn macos_lookup_prefers_system_applications() {
        let env = fake_env(&[("HOME", "/Users/w")]);
        let got = macos_candidates(BrowserKind::Chrome, &env);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("/Applications"));
        assert!(got[1].starts_with("/Users/w/Applications"));
        // We exec the bundle binary, not the .app directory.
        assert!(got[0].display().to_string().ends_with("Contents/MacOS/Google Chrome"));
    }

    #[test]
    fn path_lookup_walks_directories_in_order_and_skips_empties() {
        let got = path_candidates("/a::/b", ':', &["chrome", "chromium"]);
        let got: Vec<String> = got.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(got, vec!["/a/chrome", "/a/chromium", "/b/chrome", "/b/chromium"]);
    }

    #[test]
    fn path_lookup_handles_the_windows_separator() {
        let got = path_candidates("C:\\bin;D:\\tools", ';', &["chrome.exe"]);
        assert_eq!(got.len(), 2);
        assert!(got[1].display().to_string().contains("D:\\tools"));
    }

    /// The registry value may contain spaces, so the parser must keep the tail
    /// of the line intact rather than splitting on whitespace.
    #[test]
    fn reg_default_value_survives_spaces_in_the_path() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\...\\chrome.exe\r\n    (Default)    REG_SZ    C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe\r\n\r\n";
        assert_eq!(
            parse_reg_default(out).as_deref(),
            Some("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe")
        );
        assert_eq!(parse_reg_default("ERROR: The system was unable to find..."), None);
        assert_eq!(parse_reg_default(""), None);
    }

    /// The two flags that make this feature work must both be present, and the
    /// URL must be the last argument (Chromium reads switches first).
    #[test]
    fn args_carry_the_flag_the_profile_and_a_trailing_url() {
        let args = build_args(Path::new("/tmp/prof dir"), Some("https://example.com/x"));
        assert_eq!(args[0], "--force-renderer-accessibility");
        assert_eq!(args[1], "--user-data-dir=/tmp/prof dir", "one argv entry, no quoting");
        assert!(args.contains(&"--no-first-run".to_string()));
        assert_eq!(args.last().unwrap(), "https://example.com/x");
    }

    #[test]
    fn args_omit_the_url_when_there_is_none() {
        let args = build_args(Path::new("/tmp/p"), None);
        assert_eq!(args[0], "--force-renderer-accessibility");
        assert!(args.iter().all(|a| a.starts_with("--")), "no positional arg: {args:?}");
    }

    #[test]
    fn urls_accept_only_http_and_https() {
        assert_eq!(validate_url("https://example.com").unwrap(), "https://example.com");
        assert_eq!(validate_url("  http://example.com/a?b=c#d  ").unwrap(), "http://example.com/a?b=c#d");
        assert_eq!(validate_url("HTTPS://Example.COM").unwrap(), "HTTPS://Example.COM", "scheme check is case-insensitive but the url is passed through verbatim");
        assert!(validate_url("").is_err());
        assert!(validate_url("   ").is_err());
        assert!(validate_url("example.com").is_err(), "no scheme");
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("chrome://settings").is_err());
        assert!(validate_url("https://").is_err(), "scheme with no host");
    }

    /// The injection case this validation exists for: anything Chromium would
    /// read as a switch must never reach the command line.
    #[test]
    fn urls_refuse_anything_that_looks_like_a_switch() {
        assert!(validate_url("--load-extension=/tmp/evil").is_err());
        assert!(validate_url("-https://example.com").is_err());
        assert!(validate_url("https://ok.com --load-extension=/tmp/evil").is_err(), "the space is refused, so it cannot split into a second argument");
        assert!(validate_url("https://ok.com\n--headless").is_err());
        assert!(validate_url("https://ok.com\u{0}").is_err());
        assert!(validate_url(&format!("https://e.com/{}", "a".repeat(MAX_URL_LEN))).is_err());
    }

    /// The core of the already-running trap. Our own live instance is reused
    /// (its renderer accessibility is already on); anything else means a fresh
    /// process, which our isolated profile guarantees is genuinely new.
    #[test]
    fn reuse_decision_depends_only_on_our_own_instance() {
        assert_eq!(plan_launch(true), LaunchPlan::HandToOurs);
        assert_eq!(plan_launch(false), LaunchPlan::Fresh);
    }

    #[test]
    fn tasklist_rows_other_than_ours_count_as_a_foreign_browser() {
        let out = "\"chrome.exe\",\"4312\",\"Console\",\"1\",\"250,000 K\"\r\n\"chrome.exe\",\"77\",\"Console\",\"1\",\"9,000 K\"\r\n";
        assert!(tasklist_has_other(out, Some(4312)), "pid 77 is not ours");
        assert!(tasklist_has_other(out, None), "with no tracked pid every row is foreign");
        assert!(!tasklist_has_other("\"chrome.exe\",\"4312\",\"Console\",\"1\",\"250,000 K\"\r\n", Some(4312)));
    }

    /// tasklist prints prose, not rows, when the filter matches nothing.
    #[test]
    fn tasklist_no_match_message_is_not_a_process() {
        let out = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(!tasklist_has_other(out, None));
        assert!(!tasklist_has_other("", None));
    }

    #[test]
    fn pgrep_pids_other_than_ours_count_as_a_foreign_browser() {
        assert!(pgrep_has_other("4312\n77\n", Some(4312)));
        assert!(!pgrep_has_other("4312\n", Some(4312)));
        assert!(pgrep_has_other("4312\n", None));
        assert!(!pgrep_has_other("", None));
    }

    /// The model's one line must always say whether element aiming is on, and
    /// must warn about a decoy window whenever one exists.
    #[test]
    fn summary_states_the_capability_and_flags_a_decoy_window() {
        let s = summarize(BrowserKind::Chrome, 42, false, Some("https://e.com"), false);
        assert!(s.contains("accessibility forced on"));
        assert!(s.contains("https://e.com"));
        assert!(!s.contains("NOTE:"));

        let s = summarize(BrowserKind::Edge, 42, true, None, true);
        assert!(s.contains("Reused"), "reuse must not read as a fresh launch");
        assert!(s.contains("NOTE:"), "a foreign browser must be called out: {s}");
        assert!(s.contains("does NOT have accessibility forced"));
    }

    /// A misconfigured override is an error, never a silent fall-through to the
    /// normal search — the operator set it on purpose.
    #[test]
    fn a_bad_executable_override_is_loud() {
        let env = fake_env(&[(EXE_OVERRIDE_ENV, "/nope/not/a/browser")]);
        let err = find_browser_with(&env, &|_| false).unwrap_err();
        assert!(err.contains(EXE_OVERRIDE_ENV), "{err}");
        assert!(err.contains("not a file"), "{err}");
    }

    /// The override wins over everything, including a perfectly good installed
    /// Chrome — that is the whole point of an escape hatch.
    #[test]
    fn the_override_is_taken_before_any_search() {
        let env = fake_env(&[(EXE_OVERRIDE_ENV, "/opt/portable/chrome"), ("HOME", "/Users/w")]);
        let (_, exe) = find_browser_with(&env, &|_| true).unwrap();
        assert_eq!(exe, PathBuf::from("/opt/portable/chrome"));
    }

    /// Chrome is exhausted (well-known paths AND PATH) before Edge is
    /// considered: Chrome is the browser we measured the accessibility tree on.
    #[test]
    fn chrome_is_searched_before_edge() {
        let env = fake_env(&[("PATH", "/usr/bin"), ("HOME", "/Users/w")]);
        // Everything "exists", so the first candidate in order wins.
        let (kind, _) = find_browser_with(&env, &|_| true).unwrap();
        assert_eq!(kind, BrowserKind::Chrome);

        // Only Edge is installed anywhere: the fallback is real, not decorative.
        let edge_only = |p: &Path| {
            let s = p.display().to_string();
            s.contains("Edge") || s.contains("edge")
        };
        let (kind, exe) = find_browser_with(&env, &edge_only).unwrap();
        assert_eq!(kind, BrowserKind::Edge, "found {}", exe.display());
    }

    /// The schema is what the model reads before deciding whether to use this
    /// instead of clicking a browser icon, so the two facts it cannot infer —
    /// that an icon-launched browser exposes no page content, and that our
    /// window has its own logins — must both survive edits to the wording.
    #[test]
    fn tool_schema_tells_the_model_why_to_prefer_it() {
        let s = tool_schema();
        assert_eq!(s["name"], "launch_browser");
        let d = s["description"].as_str().unwrap();
        assert!(d.contains("accessibility tree"), "{d}");
        assert!(d.contains("INSTEAD of clicking"), "{d}");
        assert!(d.contains("does not inherit existing logins"), "{d}");
        // `url` must be optional: opening a blank browser is a legitimate call.
        assert!(s["input_schema"]["required"].as_array().unwrap().is_empty());
    }

    /// Nothing found must tell the operator what was searched and what to do,
    /// not just "not found".
    #[test]
    fn not_found_error_is_actionable() {
        let env = fake_env(&[("HOME", "/Users/w"), ("PATH", "/usr/bin")]);
        let err = find_browser_with(&env, &|_| false).unwrap_err();
        assert!(err.contains("Install Google Chrome"), "{err}");
        assert!(err.contains(EXE_OVERRIDE_ENV), "{err}");
        assert!(err.contains("Searched:"), "the searched paths must be named: {err}");
    }
}
