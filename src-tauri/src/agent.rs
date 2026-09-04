//! The Rust agent loop — the brain that turns a user task into computer-use
//! actions by streaming from a Claude model (via the local backend proxy) and
//! dispatching the model's `computer` tool calls onto `computer.rs`/`capture.rs`.
//!
//! Ported in spirit (not verbatim) from Anthropic's reference
//! `computer_use/loop.py::sampling_loop` and `tools/computer.py`. The shape:
//!   1. seed `messages` with the user task,
//!   2. stream one model turn from the backend (Anthropic raw Messages SSE),
//!   3. collect the assistant content (text + tool_use blocks),
//!   4. run each tool_use, append a `tool_result` user message,
//!   5. repeat until `stop_reason == end_turn` or there are no tool_uses.
//!
//! The model is shown a screenshot resized to the vision budget; it emits
//! coordinates in that image space. Before any click we feed the capture's
//! `sent_w`/`sent_h` into the driver via `set_screenshot_size` so `to_screen`
//! scales model coords by screen/sent — the load-bearing coordinate contract.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio_util::sync::CancellationToken;

use crate::{capture, with_computer, ComputerState};

// ---- configuration (env-overridable) --------------------------------------

pub(crate) fn backend_url() -> String {
    std::env::var("CU_BACKEND_URL").unwrap_or_else(|_| "http://localhost:8000".to_string())
}
/// BYOK: the per-turn model call goes DIRECTLY to the model endpoint with the
/// user's own key — it never touches our backend. The backend (`backend_url`) is
/// still used for run persistence only. This is that endpoint when nothing else
/// names one; see `resolve_endpoint` for the precedence ladder.
const DEFAULT_ANTHROPIC_BASE: &str = "https://api.anthropic.com";

/// Env opt-out for the worker guard (`anthropic_guard_error`). Set to `1`/`true`
/// on a worker that is *meant* to bill Anthropic directly.
const ALLOW_ANTHROPIC_ENV: &str = "CU_ALLOW_ANTHROPIC";

/// Whether the configured model endpoint is Anthropic's own API.
///
/// This is the ONLY thing that decides whether a BYOK key is required. The key
/// requirement exists because the key travels to Anthropic; point
/// `CU_ANTHROPIC_BASE` at a self-hosted server and there is no Anthropic
/// credential involved, so demanding one just blocks the run.
///
/// Deliberately derived from the endpoint rather than a user-facing "skip the
/// key" toggle: a toggle can be left on after switching back to Anthropic, and
/// the symptom is then an opaque 401 instead of a clear prompt. Host-based, so
/// it cannot fall out of sync with where requests actually go.
///
/// Takes the base as an ARGUMENT rather than reading the env itself, because the
/// endpoint is per-run now (see `ResolvedEndpoint`): every caller must answer
/// this about the endpoint THIS run drives, never the process-wide one.
fn endpoint_is_anthropic(base: &str) -> bool {
    let host = base
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split(['/', ':']).next().unwrap_or("");
    host.eq_ignore_ascii_case("api.anthropic.com") || host.is_empty()
}

/// Where the endpoint a run drives actually came from.
///
/// Reported to the UI because "which endpoint" and "who chose it" are different
/// questions with different fixes: a wrong `Fleet` value is corrected once in
/// Settings for every machine, a wrong `Env` value is one shell on one box that
/// somebody has to walk over to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointSource {
    /// Carried on the dispatched `run` frame — the operator's fleet setting.
    Fleet,
    /// This machine's `CU_ANTHROPIC_BASE`.
    Env,
    /// Nobody said; Anthropic's own API.
    Default,
}

/// The model endpoint ONE run drives, resolved once at run start and then passed
/// around instead of being re-derived. Everything that used to ask the process
/// "where do model calls go?" asks this instead — a run dispatched at a
/// self-hosted server and a shell holding a stale `CU_ANTHROPIC_BASE` disagree,
/// and silently answering with the shell's value is the exact class of bug this
/// type exists to make unrepresentable.
#[derive(Clone, Debug)]
pub struct ResolvedEndpoint {
    pub base: String,
    pub model: String,
    pub source: EndpointSource,
}

impl ResolvedEndpoint {
    fn is_anthropic(&self) -> bool {
        endpoint_is_anthropic(&self.base)
    }
    /// `{base}/v1/messages` — the only URL a turn is ever sent to.
    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base.trim_end_matches('/'))
    }
}

/// PRECEDENCE: **frame > env var > default**.
///
/// The frame wins because the operator configured it centrally and it travelled
/// here with the work. The env var stays second so an older backend — or an
/// unset fleet setting — behaves exactly as it did before any of this existed.
/// The default is Anthropic's own API, which is the whole reason
/// `anthropic_guard_error` has to exist. `model` walks the same ladder
/// independently, so a fleet that sets an endpoint but no model still honours
/// this machine's `CU_MODEL`.
///
/// Blank/whitespace values count as absent, so a settings field the operator
/// cleared falls through to the env var instead of resolving to `""`.
fn resolve_endpoint(frame_base: Option<&str>, frame_model: Option<&str>) -> ResolvedEndpoint {
    let present = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let (base, source) = match present(frame_base) {
        Some(b) => (b, EndpointSource::Fleet),
        None => match env("CU_ANTHROPIC_BASE") {
            Some(b) => (b, EndpointSource::Env),
            None => (DEFAULT_ANTHROPIC_BASE.to_string(), EndpointSource::Default),
        },
    };
    let model = present(frame_model)
        .or_else(|| env("CU_MODEL"))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    ResolvedEndpoint { base, model, source }
}

/// The last endpoint a dispatched `run` frame carried, remembered so the machine
/// panel can answer "what will this box drive?" between runs.
///
/// A worker cannot ask the backend for the fleet setting — its device token
/// never leaves Rust and `/settings` is session-scoped — so the only fleet value
/// it will ever hold is the one that rode in on a frame. Without this the panel
/// could show only the env var, which on a correctly-configured fleet is exactly
/// the wrong answer: it would read `api.anthropic.com` on a machine that has
/// been driving a self-hosted server all week. Empty until the first dispatched
/// run, and the panel says so rather than guessing.
static LAST_FLEET_ENDPOINT: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);

fn note_fleet_endpoint(base: &str, model: Option<&str>) {
    if let Ok(mut g) = LAST_FLEET_ENDPOINT.lock() {
        *g = Some((base.to_string(), model.map(str::to_string)));
    }
}

/// `pub(crate)`: the task-pickup loop (channel.rs) passes these raw values into
/// `start_run_internal` exactly as a dispatched frame would have carried them,
/// so a task run walks the same frame > env > default ladder as everything else.
pub(crate) fn last_fleet_endpoint() -> Option<(String, Option<String>)> {
    LAST_FLEET_ENDPOINT.lock().ok().and_then(|g| g.clone())
}

/// Resolve exactly as a run would, for the read-only UI commands — and for the
/// readback a task-pickup posts (channel.rs), which must promise the operator
/// the endpoint the approved run will actually drive. Shares one code path with
/// the real thing so "what would happen" and "what happens" cannot drift.
pub(crate) fn endpoint_for_display() -> ResolvedEndpoint {
    let fleet = last_fleet_endpoint();
    resolve_endpoint(
        fleet.as_ref().map(|(b, _)| b.as_str()),
        fleet.as_ref().and_then(|(_, m)| m.as_deref()),
    )
}

/// What the UI needs to decide between "enter your Anthropic key" and "verify
/// your self-hosted endpoint": the base URL to show, which mode we are in, and
/// — since the fleet can now override the shell — who chose it.
#[derive(serde::Serialize)]
pub struct ModelEndpoint {
    pub base: String,
    pub is_anthropic: bool,
    pub model: String,
    pub source: EndpointSource,
    /// True when this machine holds a worker enrollment AND the resolved
    /// endpoint is Anthropic's own API with no opt-out — i.e. the next
    /// dispatched run will be refused. Surfaced so the panel can say so before
    /// a run fails rather than after.
    pub blocked: bool,
}

#[tauri::command]
pub fn model_endpoint(app: AppHandle) -> ModelEndpoint {
    let ep = endpoint_for_display();
    let blocked = anthropic_guard_error(
        &ep.base,
        crate::credentials::is_enrolled(&app),
        allow_anthropic_env(),
    )
    .is_some();
    ModelEndpoint {
        is_anthropic: ep.is_anthropic(),
        base: ep.base,
        model: ep.model,
        source: ep.source,
        blocked,
    }
}

/// Whether `CU_ALLOW_ANTHROPIC` opts this machine out of the worker guard.
fn allow_anthropic_env() -> bool {
    matches!(
        std::env::var(ALLOW_ANTHROPIC_ENV).ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// The guard: an enrolled worker must not silently drive `api.anthropic.com`.
///
/// Returns the refusal message, or `None` to proceed. What this prevents is a
/// failure that *succeeds*: with the fleet endpoint missing, the run still
/// works, still produces a normal transcript, and quietly bills Anthropic —
/// nothing in the product ever contradicts the belief that it was self-hosted.
/// Nobody catches that. So the silent success is converted into a loud failure.
///
/// THE ASYMMETRY IS DELIBERATE. This applies only to an ENROLLED WORKER (a
/// machine holding a device token), never to an operator on a session
/// credential. An operator spending their own BYOK key on their own laptop is
/// the normal case and always was; a fleet node doing it is an unattended
/// machine spending someone else's money on a choice nobody made. Same endpoint,
/// different blast radius — hence different rules. If you are here to "simplify"
/// by applying it to everyone, you will break every personal install.
fn anthropic_guard_error(base: &str, is_worker: bool, allowed: bool) -> Option<String> {
    if !is_worker || allowed || !endpoint_is_anthropic(base) {
        return None;
    }
    Some(format!(
        "Refusing to run: this machine is an enrolled fleet worker and the model \
endpoint resolved to Anthropic's own API ({base}), so the run would bill \
Anthropic while looking self-hosted. Set the fleet model endpoint in Settings, \
or set {ALLOW_ANTHROPIC_ENV}=1 on this machine to allow it deliberately."
    ))
}

/// Reachability probe for a self-hosted endpoint — the counterpart to
/// `validate_anthropic_key` on the BYOK path. Sends the smallest possible real
/// Messages request rather than pinging a health route, because a server can be
/// listening and still be unable to serve this API shape; only a round trip
/// through `/v1/messages` proves a run would work.
#[tauri::command]
pub async fn check_model_endpoint() -> Result<String, String> {
    // Probe the endpoint a run would ACTUALLY use — a fleet value that arrived on
    // a frame included — so "verify" and "run" can never test different hosts.
    let ep = endpoint_for_display();
    let url = ep.messages_url();
    let body = json!({
        "model": ep.model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("client: {e}"))?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let txt: String = txt.chars().take(300).collect();
        return Err(format!("{url} returned {status}: {txt}"));
    }
    Ok(ep.model)
}

/// Whether to attach a fresh screenshot to the tool_result of every action that
/// changes what is on screen, instead of waiting for the model to ask for one.
///
/// Anthropic's models reliably call `screenshot` between actions, so the extra
/// image would be pure token cost there and this defaults OFF for them. Other
/// models frequently do not: a run that types, presses Return and types again
/// without ever looking is flying blind, and it *reports success* because
/// nothing ever contradicted it. Defaults ON for any non-Anthropic endpoint.
///
/// `CU_AUTO_SCREENSHOT=1/0` forces it either way.
///
/// `base` is the endpoint THIS run resolved to, not the process-wide one: a run
/// dispatched at a self-hosted server from a machine whose shell still points at
/// Anthropic needs the auto-screenshots, and deciding off the env var would hand
/// it the Anthropic default and leave the run flying blind.
fn auto_screenshot_enabled(base: &str) -> bool {
    match std::env::var("CU_AUTO_SCREENSHOT").ok().as_deref() {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => !endpoint_is_anthropic(base),
    }
}

/// Actions after which the screen may look different. `screenshot` and `zoom`
/// already return an image; `wait` is how a model asks to observe a settling UI,
/// so it is included deliberately.
fn action_changes_screen(action: &str) -> bool {
    matches!(
        action,
        "left_click"
            | "right_click"
            | "middle_click"
            | "double_click"
            | "triple_click"
            | "left_click_drag"
            | "left_mouse_down"
            | "left_mouse_up"
            | "mouse_move"
            | "type"
            | "key"
            | "hold_key"
            | "scroll"
            | "wait"
    )
}

/// How long to let the UI settle before the auto-screenshot. A capture taken the
/// instant after a click routinely shows the PREVIOUS frame — which is worse
/// than no screenshot, because the model then "sees" that its action did
/// nothing and undoes or repeats it.
const AUTO_SCREENSHOT_SETTLE: Duration = Duration::from_millis(600);

/// Anthropic Messages API version header.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header enabling the official enhanced computer-use tool
/// (`computer_20251124`). We send this directly now (the backend used to add it).
const ANTHROPIC_BETA: &str = "computer-use-2025-11-24";
/// Default model id: Claude Opus 4.8. We send a concrete id in the Messages body
/// straight to the endpoint; `resolve_endpoint` owns the precedence ladder.
const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// Default hard iteration cap. Long unattended runs routinely need more than
/// this, so it is only a default — see `max_iters`.
const DEFAULT_MAX_ITERS: usize = 150;
/// Hard iteration cap for one run. Overridable because a 24/7 unattended run
/// against a self-hosted model can legitimately need hundreds of turns, while
/// an interactive run wants a low ceiling. `CU_MAX_ITERS=0` means UNBOUNDED —
/// only set that when something else (cancellation, a watchdog) can stop the
/// run, since nothing else will.
fn max_iters() -> usize {
    std::env::var("CU_MAX_ITERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_ITERS)
}
/// Consecutive assistant turns with zero content blocks before we abort the run
/// as FAILED. See the guard in `run_agent` — an empty turn is a dropped turn,
/// not a finish, and must never be reported as a completion.
const MAX_EMPTY_TURNS: usize = 3;
const MAX_TOKENS: u32 = 4096;
/// Retries for a failed model call before the run is failed. Unattended runs
/// span hours, so a single transport hiccup must not be terminal — but a
/// genuinely broken endpoint should still surface quickly rather than spin.
const MAX_TURN_RETRIES: usize = 3;
/// Retries for a failed screen capture. Capture fails transiently while a
/// display sleeps, a Space switches or a monitor is re-configured; a moment
/// later it succeeds.
const MAX_CAPTURE_RETRIES: usize = 2;
/// Pause between capture retries. Deliberately short: `dispatch_action` is
/// synchronous (it holds the Computer mutex), so this blocks its thread.
const CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(300);
/// Keep only the N most recent screenshots in context; older ones are replaced
/// with a placeholder so the conversation doesn't balloon (loop.py's
/// keep-N-recent image pruning). Set to 2 (was 3): one fewer live full image per
/// turn trims tokens, and the rolling cache breakpoint + the official tool's
/// zoom action cover settled-context and on-demand-detail respectively.
const KEEP_RECENT_IMAGES: usize = 2;

/// Host OS as the model should think of it, plus the accelerator modifier that
/// actually works there. Getting this wrong is not cosmetic: the model picks
/// shortcuts from it, and a model told "macOS" reaches for cmd+L, Spotlight and
/// the Dock — none of which exist on Windows.
#[cfg(target_os = "windows")]
const HOST_OS: (&str, &str) = ("Windows", "ctrl");
#[cfg(target_os = "macos")]
const HOST_OS: (&str, &str) = ("macOS", "cmd");
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const HOST_OS: (&str, &str) = ("Linux", "ctrl");

/// Build the system prompt for the host platform. Was a `const &str` hardcoded
/// to macOS; it is a function now only so the OS name and modifier can vary.
fn system_prompt() -> String {
    let (os, modifier) = HOST_OS;
    format!(
        "{}\n\nYou are operating a {os} desktop. Use {os}-native conventions: the \
keyboard shortcut modifier is `{modifier}` (for example `{modifier}+c` to copy, \
`{modifier}+v` to paste), and application menus, window controls and file paths follow \
{os} conventions. Do not use shortcuts or UI affordances from another operating system.",
        SYSTEM_PROMPT_BASE
    )
}

const SYSTEM_PROMPT_BASE: &str = "You are ScreenBuddy, a computer-use agent operating a desktop on the \
user's behalf. You see the screen via screenshots and act through the `computer` tool \
(mouse, keyboard, scroll, clipboard). Coordinates are pixels in the most recent \
screenshot, origin top-left. Take a screenshot before acting when you are unsure of the \
current state. Work in small, verifiable steps: act, then screenshot to confirm. When the \
task is complete, stop and summarize what you did. Reference materials for this task are \
provided at the start of the conversation; consult them as needed. To enter a saved username \
or password, call the use_credential tool with the target label and field — never type \
credentials yourself, and never ask the user for them. When scrolling to find content, \
scroll in larger steps (a scroll_amount of about 5-10) rather than tiny increments, so you \
move through pages quickly; take a screenshot after scrolling to check your position.";

// ---- frontend event names -------------------------------------------------

pub const EV_TURN: &str = "agent://turn";
pub const EV_TEXT: &str = "agent://text";
pub const EV_ACTION: &str = "agent://action";
pub const EV_SCREENSHOT: &str = "agent://screenshot";
pub const EV_DONE: &str = "agent://done";
pub const EV_ERROR: &str = "agent://error";
pub const EV_RUN_STARTED: &str = "agent://run_started";

// ---- agent task state (cancellation) --------------------------------------

/// Holds the cancellation token for the in-flight agent task (if any). Managed
/// as Tauri state so `stop_agent_task` can cancel a run started by
/// `start_agent_task`.
#[derive(Default)]
pub struct AgentState(pub std::sync::Mutex<Option<CancellationToken>>);

/// RAII lease held by a running `run_agent`. On drop — i.e. whenever the run
/// ends by ANY path (normal completion, failure, max-iterations, early return,
/// or panic) — it releases the AgentState so the next `start_agent_task` can
/// run. Without this, a run that finishes on its own leaves a live
/// CancellationToken in the state and every subsequent start wrongly fails with
/// "an agent task is already running" until the app is restarted.
struct RunLease {
    app: AppHandle,
    token: CancellationToken,
}

impl Drop for RunLease {
    fn drop(&mut self) {
        // Mark this run finished (shared Arc — flips the state's clone too).
        self.token.cancel();
        // Clear the lease, but only if it's still ours: a newer run may have
        // already replaced it, and that one's token won't be cancelled.
        if let Some(state) = self.app.try_state::<AgentState>() {
            if let Ok(mut g) = state.0.lock() {
                if g.as_ref().map_or(false, |t| t.is_cancelled()) {
                    *g = None;
                }
            }
        }
    }
}

// ---- computer tool schema (custom tool, implemented locally) --------------

/// Anthropic's OFFICIAL enhanced computer-use tool (`computer_20251124`).
///
/// This is the schema-LESS server-defined tool: it carries NO `input_schema` and
/// NO `description` — the action schema (screenshot/click/scroll/key/type/zoom/…)
/// is built into the model. We only declare the display geometry. `enable_zoom`
/// turns on the built-in `zoom` action so the model can recover fine detail from
/// our deliberately low-resolution base screenshots.
///
/// `display_width_px` / `display_height_px` MUST equal the actual pixel
/// dimensions of the screenshots we send (the sent_w/sent_h the capture pipeline
/// produces) — the load-bearing coordinate contract. We send the required
/// `computer-use-2025-11-24` beta header directly on the Anthropic request.
fn computer_tool(display_w: u32, display_h: u32) -> Value {
    json!({
        "type": "computer_20251124",
        "name": "computer",
        "display_width_px": display_w,
        "display_height_px": display_h,
        "display_number": 1,
        "enable_zoom": true
    })
}

/// The `use_credential` tool schema. Lets the model inject a stored secret
/// WITHOUT ever seeing its value: the app looks the secret up locally and types
/// it via the computer driver. The model only learns `{ok:true/false}`.
fn use_credential_tool() -> Value {
    json!({
        "name": "use_credential",
        "description": "Type a stored credential into the currently focused field WITHOUT ever \
seeing its value. Call this instead of asking for or typing a password yourself. Provide the \
target label (e.g. 'mail.google.com' or 'Amazon — desktop app') and which field to type \
('username' or 'password'). The application types the secret locally; you will only receive \
{ok:true/false}.",
        "input_schema": {
            "type": "object",
            "properties": {
                "target": {"type": "string"},
                "field": {"type": "string", "enum": ["username", "password"]}
            },
            "required": ["target", "field"]
        }
    })
}

// ---- SSE parsing ----------------------------------------------------------

#[derive(Debug)]
enum BlockAcc {
    Text(String),
    Tool { id: String, name: String, json: String },
}

/// Accumulates Anthropic Messages streaming events into assistant content
/// blocks. Shared by the live streaming path and the unit tests (which feed
/// canned SSE bytes). When `app` is `Some`, emits frontend deltas as they
/// arrive; tests pass `None`.
struct SseAccumulator {
    blocks: BTreeMap<u64, BlockAcc>,
    stop_reason: Option<String>,
    error: Option<String>,
    done: bool,
    /// Best-effort token usage scraped from `message_start` / `message_delta`.
    input_tokens: u64,
    output_tokens: u64,
    /// Prompt-cache usage from `message_start` (lets us confirm the pinned set
    /// is billed once per run, not re-billed full-price every turn).
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

impl SseAccumulator {
    fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            stop_reason: None,
            error: None,
            done: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    /// Feed one raw SSE line. Ignores `event:` lines and blanks; acts on
    /// `data:` lines (whose JSON carries a `type` matching the event name).
    fn feed_line(&mut self, line: &str, app: Option<&AppHandle>) {
        let rest = match line.strip_prefix("data:") {
            Some(r) => r.trim(),
            None => return,
        };
        if rest.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(rest) {
            Ok(v) => v,
            Err(_) => return, // tolerate partial/keepalive payloads
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("content_block_start") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let cb = &v["content_block"];
                match cb.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        let init = cb.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        self.blocks.insert(idx, BlockAcc::Text(init.to_string()));
                        // Anthropic opens a text block empty and streams the body
                        // as `text_delta`s, so this is normally "". A server that
                        // synthesizes its stream from a non-streamed upstream can
                        // instead put the WHOLE text here and send no deltas at
                        // all — and then the live timeline stayed blank while the
                        // persisted transcript (assembled from the final content
                        // array, not from deltas) had every line. Emit it here too
                        // so both paths see the same text either way.
                        if !init.is_empty() {
                            if let Some(app) = app {
                                let _ = app.emit(EV_TEXT, json!({ "delta": init }));
                            }
                        }
                    }
                    Some("tool_use") => {
                        let id = cb.get("id").and_then(|t| t.as_str()).unwrap_or("").to_string();
                        let name =
                            cb.get("name").and_then(|t| t.as_str()).unwrap_or("").to_string();
                        self.blocks.insert(
                            idx,
                            BlockAcc::Tool { id, name, json: String::new() },
                        );
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = &v["delta"];
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let t = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if let Some(BlockAcc::Text(s)) = self.blocks.get_mut(&idx) {
                            s.push_str(t);
                        }
                        if let Some(app) = app {
                            let _ = app.emit(EV_TEXT, json!({ "delta": t }));
                        }
                    }
                    Some("input_json_delta") => {
                        let pj =
                            delta.get("partial_json").and_then(|t| t.as_str()).unwrap_or("");
                        if let Some(BlockAcc::Tool { json, .. }) = self.blocks.get_mut(&idx) {
                            json.push_str(pj);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if let Some(BlockAcc::Tool { name, json, .. }) = self.blocks.get(&idx) {
                    if let Some(app) = app {
                        let input: Value =
                            serde_json::from_str(json).unwrap_or_else(|_| json!({}));
                        let _ = app.emit(EV_ACTION, json!({ "name": name, "input": input }));
                    }
                }
            }
            Some("message_start") => {
                let usage = &v["message"]["usage"];
                if let Some(n) = usage.get("input_tokens").and_then(|t| t.as_u64()) {
                    self.input_tokens = n;
                }
                if let Some(n) = usage.get("output_tokens").and_then(|t| t.as_u64()) {
                    self.output_tokens = n;
                }
                if let Some(n) = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|t| t.as_u64())
                {
                    self.cache_creation_input_tokens = n;
                }
                if let Some(n) = usage.get("cache_read_input_tokens").and_then(|t| t.as_u64()) {
                    self.cache_read_input_tokens = n;
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v["delta"].get("stop_reason").and_then(|s| s.as_str()) {
                    self.stop_reason = Some(sr.to_string());
                }
                // `message_delta` carries cumulative output_tokens for the turn.
                if let Some(n) = v["usage"].get("output_tokens").and_then(|t| t.as_u64()) {
                    self.output_tokens = n;
                }
            }
            Some("message_stop") => {
                self.done = true;
            }
            Some("error") => {
                let msg = v["error"]
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown stream error");
                self.error = Some(msg.to_string());
            }
            _ => {}
        }
    }

    /// Convert accumulated blocks into Messages-API assistant content blocks.
    fn into_content(self) -> Vec<Value> {
        self.blocks
            .into_values()
            .map(|b| match b {
                BlockAcc::Text(text) => json!({"type": "text", "text": text}),
                BlockAcc::Tool { id, name, json } => {
                    let input: Value = serde_json::from_str(&json).unwrap_or_else(|_| json!({}));
                    json!({"type": "tool_use", "id": id, "name": name, "input": input})
                }
            })
            .collect()
    }
}

#[derive(Debug)]
enum TurnError {
    Cancelled,
    Http(String),
}

/// The parsed outcome of one streamed model turn.
struct TurnOk {
    content: Vec<Value>,
    stop: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
}

/// POST one turn DIRECTLY to Anthropic's `/v1/messages` (BYOK) and parse the
/// streamed SSE into assistant content + stop_reason + token usage. Emits
/// text/action deltas to the frontend as they arrive. Honors cancellation
/// mid-stream. The user's `api_key` is sent as `x-api-key` and never logged.
async fn stream_turn(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    app: &AppHandle,
    token: &CancellationToken,
) -> Result<TurnOk, TurnError> {
    use futures_util::StreamExt;

    // BYOK: the model call goes straight to Anthropic with the user's own key.
    // No backend session token here — `x-api-key` + the version/beta headers are
    // exactly what Anthropic's Messages API expects.
    let req = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", ANTHROPIC_BETA)
        .json(body);
    let resp = req
        .send()
        .await
        .map_err(|e| TurnError::Http(format!("request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Err(TurnError::Http(format!("anthropic {status}: {txt}")));
    }

    let mut stream = resp.bytes_stream();
    let mut acc = SseAccumulator::new();
    let mut buf = String::new();

    loop {
        let chunk = tokio::select! {
            _ = token.cancelled() => return Err(TurnError::Cancelled),
            c = stream.next() => c,
        };
        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => return Err(TurnError::Http(format!("stream error: {e}"))),
            None => break, // stream ended
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // Process all complete lines currently in the buffer.
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            acc.feed_line(line, Some(app));
        }
        if acc.done {
            break;
        }
    }
    // Flush any trailing partial line (no terminating newline).
    if !buf.is_empty() {
        let line = buf.trim_end_matches(['\r', '\n']).to_string();
        acc.feed_line(&line, Some(app));
    }

    if let Some(err) = acc.error.take() {
        return Err(TurnError::Http(err));
    }
    let stop = acc.stop_reason.clone();
    let input_tokens = acc.input_tokens;
    let output_tokens = acc.output_tokens;
    let cache_creation_input_tokens = acc.cache_creation_input_tokens;
    let cache_read_input_tokens = acc.cache_read_input_tokens;
    Ok(TurnOk {
        content: acc.into_content(),
        stop,
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
    })
}

// ---- action dispatch ------------------------------------------------------

struct ActionOutcome {
    content: Vec<Value>,
    is_error: bool,
}

fn ok_text(s: impl Into<String>) -> ActionOutcome {
    ActionOutcome { content: vec![json!({"type": "text", "text": s.into()})], is_error: false }
}
fn err_text(s: impl Into<String>) -> ActionOutcome {
    ActionOutcome { content: vec![json!({"type": "text", "text": s.into()})], is_error: true }
}
fn image_block(b64: &str) -> Value {
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": "image/jpeg", "data": b64}
    })
}

fn coord(input: &Value, key: &str) -> Option<(i32, i32)> {
    let a = input.get(key)?.as_array()?;
    if a.len() != 2 {
        return None;
    }
    Some((a[0].as_i64()? as i32, a[1].as_i64()? as i32))
}

fn modifiers(input: &Value) -> Vec<String> {
    input
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `capture::take_screenshot` with bounded retries. A capture is the one thing
/// every turn depends on, and its failures are usually momentary (the display
/// asleep, a Space mid-switch); surfacing the first one as a tool error costs
/// the model a turn and can derail a long run. Blocking sleeps because the only
/// callers are synchronous (`dispatch_action` holds the Computer mutex).
fn take_screenshot_retrying() -> Result<capture::Capture, capture::CaptureError> {
    let mut attempt: usize = 0;
    loop {
        match capture::take_screenshot() {
            Ok(cap) => return Ok(cap),
            Err(e) if attempt < MAX_CAPTURE_RETRIES => {
                attempt += 1;
                eprintln!(
                    "[agent] capture failed ({e}); retry {attempt}/{MAX_CAPTURE_RETRIES}"
                );
                std::thread::sleep(CAPTURE_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Map a single `computer` tool action onto the driver/capture and build the
/// tool_result content. `last_sent` tracks the most recent screenshot's
/// (sent_w, sent_h) so clicks can re-assert the coordinate contract.
fn dispatch_action(
    app: &AppHandle,
    state: &ComputerState,
    action: &str,
    input: &Value,
    last_sent: &mut Option<(u32, u32)>,
) -> ActionOutcome {
    use crate::computer::ScrollDir;

    // Re-assert the (sent_w,sent_h) contract on the driver before a coordinate
    // action so model-space coords scale correctly.
    let set_size = |c: &mut crate::computer::Computer| {
        if let Some((w, h)) = *last_sent {
            c.set_screenshot_size(w as i32, h as i32);
        }
    };

    match action {
        "screenshot" => match take_screenshot_retrying() {
            Ok(cap) => {
                *last_sent = Some((cap.sent_w, cap.sent_h));
                // Opportunistically update an already-initialized driver.
                if let Ok(mut g) = state.0.lock() {
                    if let Some(c) = g.as_mut() {
                        c.set_screenshot_size(cap.sent_w as i32, cap.sent_h as i32);
                    }
                }
                let _ = app.emit(
                    EV_SCREENSHOT,
                    json!({
                        "jpeg_base64": cap.jpeg_base64,
                        "sent_w": cap.sent_w, "sent_h": cap.sent_h,
                        "screen_w": cap.screen_w, "screen_h": cap.screen_h
                    }),
                );
                ActionOutcome { content: vec![image_block(&cap.jpeg_base64)], is_error: false }
            }
            Err(e) => err_text(e.to_string()),
        },

        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
            let Some((x, y)) = coord(input, "coordinate") else {
                return err_text(format!("{action} requires `coordinate` [x, y]"));
            };
            let mods = modifiers(input);
            let mref: Vec<&str> = mods.iter().map(|s| s.as_str()).collect();
            let r = with_computer(state, |c| {
                set_size(c);
                match action {
                    "left_click" => c.left_click(x, y, &mref),
                    "right_click" => c.right_click(x, y, &mref),
                    "middle_click" => c.middle_click(x, y, &mref),
                    "double_click" => c.double_click(x, y, &mref),
                    "triple_click" => c.triple_click(x, y, &mref),
                    _ => unreachable!(),
                }
                .map_err(|e| e.to_string())
            });
            match r {
                Ok(()) => ok_text(format!("{action} at ({x}, {y})")),
                Err(e) => err_text(e),
            }
        }

        "mouse_move" => {
            let Some((x, y)) = coord(input, "coordinate") else {
                return err_text("mouse_move requires `coordinate` [x, y]");
            };
            match with_computer(state, |c| {
                set_size(c);
                c.mouse_move(x, y).map_err(|e| e.to_string())
            }) {
                Ok(()) => ok_text(format!("moved to ({x}, {y})")),
                Err(e) => err_text(e),
            }
        }

        "left_click_drag" => {
            let (Some(start), Some(end)) =
                (coord(input, "start_coordinate"), coord(input, "coordinate"))
            else {
                return err_text("left_click_drag requires `start_coordinate` and `coordinate`");
            };
            let mods = modifiers(input);
            let mref: Vec<&str> = mods.iter().map(|s| s.as_str()).collect();
            match with_computer(state, |c| {
                set_size(c);
                c.left_click_drag(start, end, &mref).map_err(|e| e.to_string())
            }) {
                Ok(()) => ok_text(format!("dragged {start:?} -> {end:?}")),
                Err(e) => err_text(e),
            }
        }

        "left_mouse_down" | "left_mouse_up" => {
            let maybe = coord(input, "coordinate");
            let down = action == "left_mouse_down";
            match with_computer(state, |c| {
                set_size(c);
                if let Some((x, y)) = maybe {
                    c.mouse_move(x, y).map_err(|e| e.to_string())?;
                }
                if down {
                    c.left_mouse_down().map_err(|e| e.to_string())
                } else {
                    c.left_mouse_up().map_err(|e| e.to_string())
                }
            }) {
                Ok(()) => ok_text(action.to_string()),
                Err(e) => err_text(e),
            }
        }

        "scroll" => {
            let Some((x, y)) = coord(input, "coordinate") else {
                return err_text("scroll requires `coordinate` [x, y]");
            };
            let amount = input.get("scroll_amount").and_then(|a| a.as_i64()).unwrap_or(3) as i32;
            let dir = match input.get("scroll_direction").and_then(|d| d.as_str()) {
                Some("up") => ScrollDir::Up,
                Some("down") => ScrollDir::Down,
                Some("left") => ScrollDir::Left,
                Some("right") => ScrollDir::Right,
                _ => return err_text("scroll requires `scroll_direction` (up|down|left|right)"),
            };
            match with_computer(state, |c| {
                set_size(c);
                c.scroll(x, y, dir, amount).map_err(|e| e.to_string())
            }) {
                Ok(()) => ok_text(format!("scrolled {:?} by {amount} at ({x}, {y})", dir)),
                Err(e) => err_text(e),
            }
        }

        "type" => {
            let text = input.get("text").and_then(|t| t.as_str()).unwrap_or("");
            match with_computer(state, |c| c.type_text(text).map_err(|e| e.to_string())) {
                Ok(()) => ok_text(format!("typed {} chars", text.chars().count())),
                Err(e) => err_text(e),
            }
        }

        "key" => {
            let Some(chord) = input.get("text").and_then(|t| t.as_str()) else {
                return err_text("key requires `text` (e.g. 'cmd+v')");
            };
            match with_computer(state, |c| c.key(chord).map_err(|e| e.to_string())) {
                Ok(()) => ok_text(format!("pressed {chord}")),
                Err(e) => err_text(e),
            }
        }

        "hold_key" => {
            let Some(chord) = input.get("text").and_then(|t| t.as_str()) else {
                return err_text("hold_key requires `text`");
            };
            let secs = input.get("duration").and_then(|d| d.as_f64()).unwrap_or(1.0).clamp(0.0, 60.0);
            match with_computer(state, |c| {
                c.hold_key(chord, Duration::from_secs_f64(secs)).map_err(|e| e.to_string())
            }) {
                Ok(()) => ok_text(format!("held {chord} for {secs}s")),
                Err(e) => err_text(e),
            }
        }

        "cursor_position" => {
            match with_computer(state, |c| c.cursor_position().map_err(|e| e.to_string())) {
                Ok((x, y)) => ok_text(format!("cursor at ({x}, {y}) in screen space")),
                Err(e) => err_text(e),
            }
        }

        "read_clipboard" => {
            match with_computer(state, |c| c.read_clipboard().map_err(|e| e.to_string())) {
                Ok(s) => ok_text(s),
                Err(e) => err_text(e),
            }
        }

        "write_clipboard" => {
            let text = input.get("text").and_then(|t| t.as_str()).unwrap_or("");
            match with_computer(state, |c| c.write_clipboard(text).map_err(|e| e.to_string())) {
                Ok(()) => ok_text("clipboard set"),
                Err(e) => err_text(e),
            }
        }

        "wait" => {
            let secs = input.get("duration").and_then(|d| d.as_f64()).unwrap_or(1.0).clamp(0.0, 30.0);
            std::thread::sleep(Duration::from_secs_f64(secs));
            ok_text(format!("waited {secs}s"))
        }

        "zoom" => {
            let Some(region) = input.get("region").and_then(|r| r.as_array()) else {
                return err_text("zoom requires `region` [x1, y1, x2, y2]");
            };
            if region.len() != 4 {
                return err_text("zoom `region` must have 4 values [x1, y1, x2, y2]");
            }
            let Some((w, h)) = *last_sent else {
                return err_text("take a screenshot before zooming");
            };
            let reg = [
                region[0].as_i64().unwrap_or(0) as i32,
                region[1].as_i64().unwrap_or(0) as i32,
                region[2].as_i64().unwrap_or(0) as i32,
                region[3].as_i64().unwrap_or(0) as i32,
            ];
            match capture::zoom(reg, w, h) {
                Ok(z) => ActionOutcome {
                    content: vec![
                        json!({"type": "text", "text": z.note}),
                        image_block(&z.jpeg_base64),
                    ],
                    is_error: false,
                },
                Err(e) => err_text(e.to_string()),
            }
        }

        other => err_text(format!("unknown action: {other}")),
    }
}

fn tool_result(id: &str, outcome: ActionOutcome) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "is_error": outcome.is_error,
        "content": outcome.content
    })
}

/// Keep only the `keep` most recent screenshot images in the message history;
/// replace older `image` blocks (inside tool_result content) with a short text
/// placeholder so context stays bounded.
///
/// Index 0 (the seeded user message — which may carry the pinned reference set's
/// images) is exempt: its blocks are never stripped, so the cached prefix stays
/// byte-identical and the static set is billed once per run, not every turn.
fn prune_images(messages: &mut [Value], keep: usize) {
    // Collect (message_index, block_index, content_index) of every image block,
    // in chronological order.
    let mut positions: Vec<(usize, usize, usize)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        if mi == 0 {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(inner) = block.get("content").and_then(|c| c.as_array()) {
                    for (ci, ib) in inner.iter().enumerate() {
                        if ib.get("type").and_then(|t| t.as_str()) == Some("image") {
                            positions.push((mi, bi, ci));
                        }
                    }
                }
            }
        }
    }
    if positions.len() <= keep {
        return;
    }
    let strip = positions.len() - keep;
    for &(mi, bi, ci) in &positions[..strip] {
        messages[mi]["content"][bi]["content"][ci] =
            json!({"type": "text", "text": IMAGE_STUB});
    }
}

/// Text we replace pruned screenshots with. Also the marker `set_rolling_cache`
/// scans for to find the permanently-settled prefix.
const IMAGE_STUB: &str = "[screenshot removed to save context]";

/// Marker appended to a truncated text block. Distinct from `IMAGE_STUB` on
/// purpose: `set_rolling_cache` keys the cache frontier off `IMAGE_STUB`, and a
/// truncated text block must never be mistaken for a pruned screenshot.
const TEXT_STUB: &str = " […truncated to save context]";
/// Turns whose text is kept verbatim. Assistant reasoning and tool_result text
/// are what the model steers by in the near term; further back only the gist
/// matters, and on a 24/7 run the tail is what fills the context window.
const KEEP_RECENT_TURNS: usize = 40;
/// Only text blocks longer than this are worth truncating — short ones cost
/// nothing and truncating them would lose whole tool results (an error string,
/// a coordinate confirmation) for no gain.
const MAX_TEXT_CHARS: usize = 2000;
/// How much of the head of a long block survives. Enough that the block still
/// says what it was about; the rest is what actually costs tokens.
const TEXT_HEAD_CHARS: usize = 240;
/// Minimum number of blocks that must be truncatable before we truncate ANY.
///
/// WHY batch: every mutation behind the rolling cache breakpoint invalidates
/// the cached prefix. Truncating the one block that crossed the window each
/// turn would therefore cost a full prefix re-write EVERY turn. Waiting until a
/// batch has accumulated pays that cost once per ~batch turns instead. The
/// count only ever grows by a turn's worth of blocks, so this self-paces.
const TEXT_PRUNE_BATCH: usize = 20;

/// Keep the conversation's TEXT bounded the way `prune_images` bounds its
/// images: truncate long text blocks in turns older than `keep_turns`, leaving
/// a short marker behind.
///
/// Same exemption as `prune_images`: index 0 carries the pinned reference set
/// and the STATIC cache breakpoint, so it is never touched — mutating it would
/// bust the once-per-run cached prefix.
///
/// Structure is preserved exactly: no message and no block is ever removed, and
/// only the `text` field of a `text` block is rewritten. Deleting a message
/// would orphan a later `tool_result`'s `tool_use_id` and the API would reject
/// the conversation; dropping blocks would break the assistant/user alternation
/// and the tool_use/tool_result pairing. When in doubt this truncates less.
fn prune_text(messages: &mut [Value], keep_turns: usize) {
    // Two messages per turn: the assistant message and the user message
    // carrying its tool_results.
    let keep = keep_turns.saturating_mul(2);
    // Index 0 is exempt, so only messages[1..cutoff] are candidates.
    let cutoff = messages.len().saturating_sub(keep);
    if cutoff <= 1 {
        return;
    }

    // Collect (message, block, inner-block) positions of every long text block:
    // `None` inner index = a top-level assistant/user text block, `Some(ci)` =
    // a text block nested in a tool_result's content.
    let mut positions: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate().take(cutoff).skip(1) {
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if is_long_text(block) {
                        positions.push((mi, bi, None));
                    }
                }
                Some("tool_result") => {
                    if let Some(inner) = block.get("content").and_then(|c| c.as_array()) {
                        for (ci, ib) in inner.iter().enumerate() {
                            if ib.get("type").and_then(|t| t.as_str()) == Some("text")
                                && is_long_text(ib)
                            {
                                positions.push((mi, bi, Some(ci)));
                            }
                        }
                    }
                }
                // tool_use inputs are the model's own arguments (coordinates,
                // keys) — small, and mangling them would corrupt the pairing.
                _ => {}
            }
        }
    }
    if positions.len() < TEXT_PRUNE_BATCH {
        return;
    }
    for (mi, bi, ci) in positions {
        let slot = match ci {
            Some(ci) => &mut messages[mi]["content"][bi]["content"][ci]["text"],
            None => &mut messages[mi]["content"][bi]["text"],
        };
        if let Some(s) = slot.as_str() {
            *slot = Value::String(truncate_text(s));
        }
    }
}

/// A text block long enough to be worth truncating. Already-truncated blocks
/// are short by construction, so this is what makes `prune_text` idempotent.
fn is_long_text(block: &Value) -> bool {
    block
        .get("text")
        .and_then(|t| t.as_str())
        .map_or(false, |s| s.chars().count() > MAX_TEXT_CHARS)
}

/// Head of `s` plus `TEXT_STUB`, cut on a char boundary (byte slicing would
/// panic mid-UTF-8, and model text is full of non-ASCII).
fn truncate_text(s: &str) -> String {
    let end = s
        .char_indices()
        .nth(TEXT_HEAD_CHARS)
        .map_or(s.len(), |(i, _)| i);
    format!("{}{}", &s[..end], TEXT_STUB)
}

/// Maintain ONE rolling `cache_control` breakpoint that tracks the pruning
/// frontier, so the cached conversation prefix matches turn-over-turn.
///
/// `messages[0]` owns the STATIC pinned breakpoint and is never touched here.
/// For every other message we (a) strip any `cache_control` from its top-level
/// content blocks — capping total breakpoints at 2 (messages[0] + this one),
/// well under Anthropic's max of 4 — then (b) re-add ONE breakpoint to the LAST
/// top-level block of the HIGHEST-index message that already contains a stubbed
/// screenshot (`IMAGE_STUB`).
///
/// WHY the newest-STUB message and not the newest message: live/kept screenshots
/// are still mutated by future pruning, so a breakpoint near them busts the
/// cache every turn. The newest stub sits just behind the pruning frontier — it
/// is permanently settled (byte-stable) and advances exactly one message per
/// turn, so the cached prefix lines up turn-over-turn. `cache_control` is a
/// directive (not cached bytes), so moving it each turn is the documented
/// multi-turn pattern. If no stub exists yet (run younger than the keep-window),
/// do nothing.
fn set_rolling_cache(messages: &mut Vec<Value>) {
    let mut newest_stub: Option<usize> = None;
    for (mi, msg) in messages.iter_mut().enumerate() {
        if mi == 0 {
            continue;
        }
        let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let mut has_stub = false;
        for block in blocks.iter_mut() {
            // (a) strip any stale breakpoint from this top-level block.
            if let Some(obj) = block.as_object_mut() {
                obj.remove("cache_control");
            }
            // Does this tool_result carry an already-stubbed screenshot?
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                if let Some(inner) = block.get("content").and_then(|c| c.as_array()) {
                    if inner.iter().any(|ib| {
                        ib.get("type").and_then(|t| t.as_str()) == Some("text")
                            && ib.get("text").and_then(|t| t.as_str()) == Some(IMAGE_STUB)
                    }) {
                        has_stub = true;
                    }
                }
            }
        }
        if has_stub {
            newest_stub = Some(mi);
        }
    }
    // (b) mark the newest settled (stub-bearing) message's last top-level block.
    if let Some(mi) = newest_stub {
        if let Some(blocks) = messages[mi].get_mut("content").and_then(|c| c.as_array_mut()) {
            if let Some(last) = blocks.last_mut().and_then(|b| b.as_object_mut()) {
                last.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            }
        }
    }
}

// ---- run persistence (best-effort) ----------------------------------------
//
// Mirror the live computer-use run into the backend `/runs` store so the web
// dashboard can replay it. Every call here is BEST-EFFORT: failures are logged
// via `eprintln!` and swallowed so persistence can never break (or even slow to
// a halt) the actual computer-use loop. All calls reuse the same Bearer session
// token already sent to `cu-stream`.

/// Attach this machine's bearer credential (if any) to an outgoing request.
///
/// The single place run persistence decides what it is authenticating AS. `auth`
/// is the session token the frontend passed down; `credentials::backend_credential`
/// overrides it with the stored device token on an enrolled worker. Call sites
/// stay ignorant of which — there is one credential, chosen once, and adding a
/// second answer here is how the two classes would start to blur.
pub(crate) fn with_bearer(
    app: &AppHandle,
    req: reqwest::RequestBuilder,
    auth: &str,
) -> reqwest::RequestBuilder {
    match crate::credentials::backend_credential(app, auth) {
        Some(cred) => req.header("authorization", format!("Bearer {cred}")),
        None => req,
    }
}

/// Monotonic event sequence helper.
fn bump(seq: &mut i64) -> i64 {
    let s = *seq;
    *seq += 1;
    s
}

/// Concatenate the `text` blocks of an assistant content array.
fn assistant_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                b.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// `PATCH /runs/{id}` non-terminal status update (e.g. "running") — best effort,
/// logs and swallows failures. The run row itself is minted by the FRONTEND via
/// `POST /runs` before the agent task is spawned; this only stamps the status so
/// the backend records `started_at`.
async fn runs_patch_status(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: &str,
    status: &str,
) {
    let url = format!("{base}/runs/{run_id}");
    let body = json!({ "status": status });
    match with_bearer(app, client.patch(&url).json(&body), auth).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            eprintln!("[runs] status '{status}': HTTP {}", r.status());
            crate::device::note_rejection(app, r.status().as_u16(), "runs");
        }
        Err(e) => eprintln!("[runs] status '{status}': request failed: {e}"),
    }
}

/// `POST /runs/{id}/events` — best effort, logs and swallows failures.
#[allow(clippy::too_many_arguments)]
async fn runs_event(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: &str,
    seq: i64,
    ev_type: &str,
    data: Value,
    artifact_object: Option<&str>,
    artifact_kind: Option<&str>,
) {
    let url = format!("{base}/runs/{run_id}/events");
    let mut body = json!({ "type": ev_type, "seq": seq, "data": data });
    if let Some(obj) = artifact_object {
        body["artifact_object"] = json!(obj);
    }
    if let Some(kind) = artifact_kind {
        body["artifact_kind"] = json!(kind);
    }
    match with_bearer(app, client.post(&url).json(&body), auth).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("[runs] event '{ev_type}': HTTP {}", r.status()),
        Err(e) => eprintln!("[runs] event '{ev_type}': request failed: {e}"),
    }
}

/// Save a screenshot to LOCAL disk under the app data dir and return its
/// absolute path on success (for the follow-up screenshot event), or `None`
/// (logged) on any failure.
///
/// This is the OFFLINE record and the only copy guaranteed to exist: the same
/// frame is also mirrored to object storage (`screenshots::enqueue_run_shot`),
/// but that is best-effort and droppable, so the disk write stays unconditional
/// and stays first.
///
/// Path scheme: `<app_data_dir>/runs/<run_id>/<shot_seq>.jpg`. The base64 jpeg
/// is decoded to raw bytes before writing. This is synchronous (no `.await`);
/// it does no network I/O.
fn runs_save_screenshot_local(
    app: &AppHandle,
    run_id: &str,
    shot_seq: i64,
    jpeg_base64: &str,
) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};

    let base_dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[runs] local save: app_data_dir unavailable: {e}");
            return None;
        }
    };
    let dir = base_dir.join("runs").join(run_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[runs] local save: create_dir_all failed: {e}");
        return None;
    }

    let bytes = match B64.decode(jpeg_base64) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[runs] local save: base64 decode failed: {e}");
            return None;
        }
    };

    let path = dir.join(format!("{shot_seq}.jpg"));
    if let Err(e) = std::fs::write(&path, &bytes) {
        eprintln!("[runs] local save: write failed: {e}");
        return None;
    }
    Some(path.to_string_lossy().to_string())
}

/// `PATCH /runs/{id}` terminal status update — best effort, logs failures.
#[allow(clippy::too_many_arguments)]
async fn runs_finalize(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: &str,
    status: &str,
    num_steps: i64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cache_read_input_tokens: u64,
    result: Value,
    error_message: Option<&str>,
) {
    // Every terminal path funnels through here, so this is the one place the
    // task-pickup loop (channel.rs) can learn how a run ended — a device token
    // cannot GET /runs/{id} back, so the worker's own record is the only copy
    // it can consult. Recorded BEFORE the network call: the outcome must be in
    // state before the RunLease drop cancels the token the pickup waits on.
    crate::channel::note_run_outcome(app, run_id, status, error_message);
    // The DURABLE twin of the note above: the outcome slot dies with the app,
    // and the frames under runs/<id>/ say nothing about how the run ended, so
    // this is the one moment the machine can remember that for later (the
    // Machine screen's local run card). Best-effort like the local frame
    // writes — a full disk loses a breadcrumb, never the run.
    crate::runs_local::write_outcome(app, run_id, status, error_message, num_steps);
    let url = format!("{base}/runs/{run_id}");
    let body = json!({
        "status": status,
        "num_steps": num_steps,
        "total_input_tokens": total_input_tokens,
        "total_output_tokens": total_output_tokens,
        "total_cache_creation_input_tokens": total_cache_creation_input_tokens,
        "total_cache_read_input_tokens": total_cache_read_input_tokens,
        "result": result,
        "error_message": error_message,
    });
    match with_bearer(app, client.patch(&url).json(&body), auth).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("[runs] finalize '{status}': HTTP {}", r.status()),
        Err(e) => eprintln!("[runs] finalize '{status}': request failed: {e}"),
    }
}

// ---- the loop -------------------------------------------------------------

async fn run_agent(
    app: AppHandle,
    prompt: String,
    auth: String,
    backend: String,
    pinned_set_ids: Vec<String>,
    run_id: String,
    model_arg: Option<String>,
    endpoint_arg: Option<String>,
    token: CancellationToken,
) {
    // Release the AgentState lock when this run ends, however it ends.
    let _lease = RunLease { app: app.clone(), token: token.clone() };

    let client = reqwest::Client::new();
    let base = backend;
    // Resolve the model endpoint ONCE, here, and use nothing else for the rest
    // of the run. `endpoint_arg`/`model_arg` are what the dispatched `run` frame
    // carried (or what the local launcher picked); everything below reads the
    // resolution rather than the environment. PRECEDENCE: frame > env > default.
    let ep = resolve_endpoint(endpoint_arg.as_deref(), model_arg.as_deref());
    if ep.source == EndpointSource::Fleet {
        note_fleet_endpoint(&ep.base, Some(&ep.model));
    }
    // BYOK: the per-turn model call goes DIRECTLY to that endpoint. `base` is
    // kept for run persistence (POST /runs, events, PATCH) with the session
    // token — two different servers, deliberately.
    let url = ep.messages_url();
    let model = ep.model.clone();
    let cred_tool = use_credential_tool();
    eprintln!("[agent] model endpoint {} ({:?})", ep.base, ep.source);

    // --- persistence bootstrap ---
    // The run row is minted by the FRONTEND via `POST /runs` before this task is
    // spawned; we receive the pre-created `run_id`, announce it, and stamp the
    // status to "running" (the backend stamps `started_at`). All downstream
    // persistence keeps the `Option<String>` shape so it stays best-effort.
    // `task` rides along because a worker's UI cannot ask the backend what it is
    // running: its device token never leaves Rust, so the machine panel is built
    // entirely from these events. Without it that panel can only name the run's
    // id and its last tool call — true, and useless to someone who walked up to
    // the machine wanting to know what it is doing.
    let _ = app.emit(EV_RUN_STARTED, json!({ "run_id": run_id, "task": prompt }));
    runs_patch_status(&app, &client, &base, &auth, &run_id, "running").await;
    let run_id: Option<String> = Some(run_id);
    let mut seq: i64 = 0;
    // Monotonic per-run screenshot index, used for the local file name
    // (`<app_data_dir>/runs/<run_id>/<shot_seq>.jpg`).
    let mut shot_seq: i64 = 0;
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut total_cache_create: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut last_text = String::new();
    let mut steps: i64 = 0;

    // The worker guard, BEFORE the key read: a worker that must not drive
    // Anthropic should be told so, not asked for an Anthropic key first (which
    // on a cold cache costs a biometric prompt on a machine nobody is at).
    // Failing here rather than in `start_run_internal` is deliberate — this path
    // finalizes the run row as "failed" with the reason, so the refusal shows up
    // in the admin's run list instead of only in the worker's stderr.
    if let Some(msg) = anthropic_guard_error(
        &ep.base,
        crate::credentials::is_enrolled(&app),
        allow_anthropic_env(),
    ) {
        let _ = app.emit(EV_ERROR, json!({ "error": msg }));
        if let Some(rid) = &run_id {
            runs_finalize(
                &app, &client, &base, &auth, rid, "failed", steps, total_in, total_out,
                total_cache_create, total_cache_read, Value::Null, Some(&msg),
            )
            .await;
        }
        return;
    }

    // BYOK is mandatory: read the user's own Anthropic key once, up front. The
    // per-turn model call goes DIRECTLY to the endpoint with this key — it never
    // touches our backend. If no key is stored, fail fast with a clear,
    // user-facing error (not a raw 401 from a later request).
    let api_key = match crate::credentials::anthropic_key(&app) {
        Some(k) => k,
        // A self-hosted endpoint has no Anthropic credential to supply, so a
        // missing key is not an error there — send an empty `x-api-key` and let
        // the endpoint decide. Only the real Anthropic API can actually be
        // blocked by this, so only it refuses up front. Keyed off THIS run's
        // resolved endpoint, not the env var: a run dispatched at a self-hosted
        // server from a shell with no `CU_ANTHROPIC_BASE` would otherwise be
        // refused for want of a key it does not need. See `endpoint_is_anthropic`.
        None if !ep.is_anthropic() => String::new(),
        None => {
            let msg = "No Anthropic API key set — add one in Settings";
            let _ = app.emit(EV_ERROR, json!({ "error": msg }));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &app, &client, &base, &auth, rid, "failed", steps, total_in, total_out,
                    total_cache_create, total_cache_read, Value::Null, Some(msg),
                )
                .await;
            }
            return;
        }
    };

    // Seed the conversation. With a pinned set, the FIRST user message is a
    // content ARRAY: the static reference blocks followed by the prompt with an
    // ephemeral cache breakpoint, so the reference prefix is cached and billed
    // once per run rather than re-billed full-price every turn. Without a set,
    // keep the bare-string seed and add no cache_control.
    // Concatenate the reference blocks from ALL selected sets, in order. Each
    // set contributes its blocks; the combined array is the cached prefix, with
    // the cache breakpoint on the trailing prompt text. An empty list behaves
    // exactly like the old single-set `None` case: a bare-string seed, no
    // cache_control.
    let mut content: Vec<Value> = pinned_set_ids
        .iter()
        .flat_map(|id| crate::pinned::load_blocks(&app, id))
        .collect();
    let mut messages: Vec<Value> = if content.is_empty() {
        vec![json!({"role": "user", "content": prompt})]
    } else {
        content.push(json!({
            "type": "text",
            "text": prompt,
            "cache_control": {"type": "ephemeral"},
        }));
        vec![json!({"role": "user", "content": content})]
    };
    let mut last_sent: Option<(u32, u32)> = None;

    // Size the OFFICIAL computer tool's display dims to match exactly what we
    // send. The coordinate contract requires display_width_px/display_height_px
    // == the resized screenshot's sent_w/sent_h. Take one real capture to learn
    // those dims (and seed `last_sent` + the driver size belt-and-suspenders); if
    // capture fails (e.g. Screen Recording permission missing), fall back to
    // computing the would-be sent size from the primary monitor.
    // We also KEEP this capture's jpeg (see `initial_shot` below) — it is a
    // free, correctly-sized picture of the screen the run starts on.
    let mut initial_shot: Option<String> = None;
    let (disp_w, disp_h) = match take_screenshot_retrying() {
        Ok(cap) => {
            last_sent = Some((cap.sent_w, cap.sent_h));
            let comp_state = app.state::<ComputerState>();
            if let Ok(mut g) = comp_state.0.lock() {
                if let Some(c) = g.as_mut() {
                    c.set_screenshot_size(cap.sent_w as i32, cap.sent_h as i32);
                }
            }
            initial_shot = Some(cap.jpeg_base64);
            (cap.sent_w, cap.sent_h)
        }
        Err(e) => {
            eprintln!("[agent] initial capture failed ({e}); sizing tool from monitor");
            capture::primary_sent_size().unwrap_or((1024, 768))
        }
    };
    let tool = computer_tool(disp_w, disp_h);

    // Consecutive empty assistant turns; reset by any turn that carries content.
    let mut empty_turns: usize = 0;

    // The diary drain. On an enrolled worker EVERY run drains — local,
    // dispatched or task-picked alike — because a nudge should reach whatever
    // run is actually in flight; on an un-enrolled machine `TurnDrain` disables
    // itself (there is no device credential to read the channel with). Cheap by
    // construction: one GET per turn boundary, and a drain failure never fails
    // the turn.
    let mut drain = crate::channel::TurnDrain::new(&app);

    // Resolved once per run so a mid-run env change can't make some turns carry
    // an auto-screenshot and others not.
    let auto_screenshot = auto_screenshot_enabled(&ep.base);
    eprintln!("[agent] auto-screenshot after actions: {auto_screenshot}");

    // Give the model the starting screen BEFORE its first move. Auto-screenshots
    // only cover turns that follow an action, so without this turn 1 is blind and
    // the model must spend a turn asking for a picture — which also makes the run
    // depend on the endpoint supporting the `screenshot` action at all. The image
    // is the capture we already took to size the tool, so it costs nothing extra.
    //
    // A SEPARATE message, deliberately not folded into messages[0]: that message
    // owns the static cache breakpoint and is exempt from `prune_images`, so an
    // image placed there would be re-sent verbatim on every turn for the whole
    // run. Here it retires normally once newer screenshots arrive.
    if auto_screenshot {
        if let Some(shot) = initial_shot.take() {
            messages.push(json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Here is the current screen before you begin."},
                    image_block(&shot),
                ],
            }));
        }
    }

    // Resolved once per run so a mid-run env change can't shift the ceiling
    // under us. 0 == unbounded (see `max_iters`), expressed as a `loop` because
    // a range can't say "no end".
    let max_iters = max_iters();
    let mut turn: usize = 0;
    loop {
        turn += 1;
        if max_iters != 0 && turn > max_iters {
            break;
        }
        if token.is_cancelled() {
            let _ = app.emit(EV_DONE, json!({"reason": "cancelled"}));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &app, &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
                    Value::Null, Some("cancelled by user"),
                )
                .await;
            }
            return;
        }
        let _ = app.emit(EV_TURN, json!({"turn": turn}));
        steps = turn as i64;

        // status event per turn
        if let Some(rid) = &run_id {
            let s = bump(&mut seq);
            runs_event(
                &app, &client, &base, &auth, rid, s, "status",
                json!({"turn": turn, "state": "running"}), None, None,
            )
            .await;
        }

        // Turn-boundary drain: pick up anything the operator appended to the
        // diary since the last boundary. Each nudge becomes its OWN user
        // message — never a mutation of messages[0], which owns the static
        // cache breakpoint and must stay byte-identical for the pinned prefix
        // to keep hitting. Other admin types are receipted inside the drain
        // without touching the conversation. The server cursor is advanced by
        // `turn_completed` below, only after this turn's response has fully
        // landed — a crash mid-turn must redeliver the batch, and the
        // deterministic receipt msg_ids make that redelivery harmless.
        for nudge in drain.at_boundary(&app, &client, &base).await {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": crate::channel::nudge_user_text(&nudge),
                }],
            }));
        }

        let body = json!({
            "model": model,
            // Rebuilt per turn rather than hoisted: it is a single `format!` of
            // a fixed string, invisible next to a screenshot upload, and it
            // serializes byte-identically every turn so the prompt-cache prefix
            // still hits.
            "system": system_prompt(),
            "messages": messages,
            "tools": [tool, cred_tool],
            "max_tokens": MAX_TOKENS,
            // The backend used to force streaming when it relayed; talking to
            // Anthropic directly, we must set it ourselves so the SSE parser has
            // events to consume.
            "stream": true,
        });

        // A 500, an overloaded backend or a dropped connection is a blip, not a
        // reason to abandon a multi-hour run — retry the turn with exponential
        // backoff before surfacing the failure. The request is idempotent from
        // our side (nothing has been appended to `messages` yet), so a retry
        // simply re-asks for the same turn. Cancellation is NOT retried: it is
        // the user's decision, and it ends the run immediately as before.
        let mut attempt: usize = 0;
        let attempted = loop {
            match stream_turn(&client, &url, &api_key, &body, &app, &token).await {
                Ok(r) => break Ok(r),
                Err(TurnError::Cancelled) => break Err(TurnError::Cancelled),
                Err(TurnError::Http(e)) => {
                    if attempt >= MAX_TURN_RETRIES {
                        break Err(TurnError::Http(e));
                    }
                    attempt += 1;
                    // 1s, 2s, 4s — long enough to ride out a restart or a rate
                    // limit, short enough that a wedged run is still visible.
                    let delay = Duration::from_secs(1 << (attempt - 1));
                    eprintln!(
                        "[agent] turn {turn} failed ({e}); retry {attempt}/{MAX_TURN_RETRIES} in {}s",
                        delay.as_secs()
                    );
                    // A stop pressed during backoff must take effect now, not
                    // after the sleep — fold it into the same cancelled path.
                    let cancelled = tokio::select! {
                        _ = token.cancelled() => true,
                        _ = tokio::time::sleep(delay) => false,
                    };
                    if cancelled {
                        break Err(TurnError::Cancelled);
                    }
                }
            }
        };
        let turn_ok = match attempted {
            Ok(r) => r,
            Err(TurnError::Cancelled) => {
                let _ = app.emit(EV_DONE, json!({"reason": "cancelled"}));
                if let Some(rid) = &run_id {
                    runs_finalize(
                        &app, &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
                        Value::Null, Some("cancelled by user"),
                    )
                    .await;
                }
                return;
            }
            Err(TurnError::Http(e)) => {
                let _ = app.emit(EV_ERROR, json!({"error": e.clone()}));
                let _ = app
                    .notification()
                    .builder()
                    .title("ScreenBuddy — run failed")
                    .body(e.clone())
                    .show();
                if let Some(rid) = &run_id {
                    runs_finalize(
                        &app, &client, &base, &auth, rid, "failed", steps, total_in, total_out, total_cache_create, total_cache_read,
                        Value::Null, Some(&e),
                    )
                    .await;
                }
                return;
            }
        };
        let TurnOk {
            content,
            stop,
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } = turn_ok;
        total_in += input_tokens;
        total_out += output_tokens;
        total_cache_create += cache_creation_input_tokens;
        total_cache_read += cache_read_input_tokens;

        // text event with the accumulated assistant text for this turn
        let turn_text = assistant_text(&content);
        if !turn_text.is_empty() {
            last_text = turn_text.clone();
        }
        if let Some(rid) = &run_id {
            if !turn_text.is_empty() {
                let s = bump(&mut seq);
                runs_event(
                    &app, &client, &base, &auth, rid, s, "text",
                    json!({"text": turn_text}), None, None,
                )
                .await;
            }
        }

        // An assistant turn with ZERO content blocks is never a legitimate
        // finish — the model spent output tokens and we got nothing back. It
        // means the turn was dropped somewhere in transport (a proxy that maps
        // a terminal action to `end_turn` and discards its text does exactly
        // this). Left alone it is indistinguishable from success: the
        // `tool_uses.is_empty()` branch below would finalize the run as
        // "completed" with an empty summary, and we would also push an
        // empty-content assistant message that the next request rejects.
        //
        // Retry the turn instead. Only after `MAX_EMPTY_TURNS` consecutive
        // empties do we give up, and then as a FAILURE — never as a silent
        // completion. Any non-empty turn resets the counter.
        if content.is_empty() {
            empty_turns += 1;
            eprintln!("[agent] empty assistant turn ({empty_turns}/{MAX_EMPTY_TURNS}); retrying");
            if empty_turns < MAX_EMPTY_TURNS {
                continue;
            }
            let msg = "model returned empty responses; run aborted";
            let _ = app.emit(EV_ERROR, json!({ "error": msg }));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &app, &client, &base, &auth, rid, "failed", steps, total_in, total_out,
                    total_cache_create, total_cache_read, Value::Null, Some(msg),
                )
                .await;
            }
            return;
        }
        empty_turns = 0;

        messages.push(json!({"role": "assistant", "content": content.clone()}));

        // The turn that consumed this boundary's drained batch has completed —
        // the model's full response is in hand — so acknowledge consumption to
        // the server NOW, before the end_turn check: waiting for the next
        // boundary would leave the final turn's batch unacknowledged on every
        // clean run end. The empty-turn retry above deliberately does not reach
        // here: a dropped turn did not consume anything.
        drain.turn_completed(&app, &client, &base).await;

        let tool_uses: Vec<&Value> = content
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .collect();

        if stop.as_deref() == Some("end_turn") || tool_uses.is_empty() {
            let _ = app.emit(EV_DONE, json!({"reason": "end_turn", "turns": turn}));
            // Best-effort native notification (fires even if no UI is mounted /
            // the app is backgrounded). Never break the loop on failure.
            let _ = app
                .notification()
                .builder()
                .title("ScreenBuddy — run complete")
                .body(format!("Finished in {turn} turns"))
                .show();
            if let Some(rid) = &run_id {
                runs_finalize(
                    &app, &client, &base, &auth, rid, "completed", steps, total_in, total_out, total_cache_create, total_cache_read,
                    json!({"summary": last_text}), None,
                )
                .await;
            }
            return;
        }

        // Dispatch each action. Hold the Computer state ONLY for the synchronous
        // dispatch (no `.await` inside this block) so the MutexGuard / tauri
        // `State` never crosses an await point — keeping the future `Send`.
        // We collect what to persist and PUT it after the state scope ends.
        let mut cancelled = false;
        let (results, persisted): (Vec<Value>, Vec<(String, Value, Vec<String>)>) = {
            let comp_state = app.state::<ComputerState>();
            let mut results: Vec<Value> = Vec::with_capacity(tool_uses.len());
            let mut persisted: Vec<(String, Value, Vec<String>)> = Vec::new();
            for tu in tool_uses {
                if token.is_cancelled() {
                    cancelled = true;
                    break;
                }
                let id = tu.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = tu.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name == "computer" {
                    let action =
                        tu["input"].get("action").and_then(|a| a.as_str()).unwrap_or("");
                    let mut outcome =
                        dispatch_action(&app, &comp_state, action, &tu["input"], &mut last_sent);

                    // Show the model what its action did. Without this, a model
                    // that never calls `screenshot` acts on a stale view for the
                    // whole run and then reports success — see
                    // `auto_screenshot_enabled`. Skipped when the outcome already
                    // carries an image (`screenshot`/`zoom`) so we never send two,
                    // and skipped on failure so an error keeps its own message.
                    if auto_screenshot && !outcome.is_error && action_changes_screen(action) {
                        let has_image = outcome.content.iter().any(|b| {
                            b.get("type").and_then(|t| t.as_str()) == Some("image")
                        });
                        if !has_image {
                            std::thread::sleep(AUTO_SCREENSHOT_SETTLE);
                            match take_screenshot_retrying() {
                                Ok(cap) => {
                                    last_sent = Some((cap.sent_w, cap.sent_h));
                                    if let Ok(mut g) = comp_state.0.lock() {
                                        if let Some(c) = g.as_mut() {
                                            c.set_screenshot_size(
                                                cap.sent_w as i32,
                                                cap.sent_h as i32,
                                            );
                                        }
                                    }
                                    let _ = app.emit(
                                        EV_SCREENSHOT,
                                        json!({
                                            "jpeg_base64": cap.jpeg_base64,
                                            "sent_w": cap.sent_w, "sent_h": cap.sent_h,
                                            "screen_w": cap.screen_w, "screen_h": cap.screen_h
                                        }),
                                    );
                                    outcome.content.push(image_block(&cap.jpeg_base64));
                                }
                                // A failed auto-capture must not fail the action
                                // the model actually asked for; it just means this
                                // turn carries no image.
                                Err(e) => eprintln!("[agent] auto-screenshot failed: {e}"),
                            }
                        }
                    }

                    // Pull any screenshot (jpeg base64) out of the tool_result
                    // image blocks before the outcome is moved into tool_result.
                    let shots: Vec<String> = outcome
                        .content
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("image") {
                                b.get("source")
                                    .and_then(|s| s.get("data"))
                                    .and_then(|d| d.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                    persisted.push((name.to_string(), tu["input"].clone(), shots));
                    results.push(tool_result(id, outcome));
                } else if name == "use_credential" {
                    // Inject a stored secret WITHOUT it ever entering model
                    // context: look it up locally and type it via the driver. The
                    // secret is NEVER placed into the tool_result, an emitted
                    // event, a persisted run record, or a log line — the model
                    // only ever learns {ok:true/false}.
                    let target =
                        tu["input"].get("target").and_then(|t| t.as_str()).unwrap_or("");
                    let field =
                        tu["input"].get("field").and_then(|f| f.as_str()).unwrap_or("");
                    let outcome = match crate::credentials::lookup(&app, target, field) {
                        Some(secret) => match with_computer(&comp_state, |c| {
                            c.type_text(&secret).map_err(|e| e.to_string())
                        }) {
                            Ok(_) => ok_text("{\"ok\": true}"),
                            Err(_) => err_text("{\"ok\": false, \"error\": \"typing failed\"}"),
                        },
                        None => {
                            ok_text("{\"ok\": false, \"error\": \"no credential for target\"}")
                        }
                    };
                    // Persist a redacted record (target label + field name only,
                    // never the secret value) so the run log shows the action.
                    persisted.push((
                        name.to_string(),
                        json!({"target": target, "field": field}),
                        Vec::new(),
                    ));
                    results.push(tool_result(id, outcome));
                } else {
                    results.push(tool_result(id, err_text(format!("unknown tool: {name}"))));
                }
            }
            (results, persisted)
        };

        if cancelled {
            let _ = app.emit(EV_DONE, json!({"reason": "cancelled"}));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &app, &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
                    Value::Null, Some("cancelled by user"),
                )
                .await;
            }
            return;
        }

        // Persist dispatched actions (tool_use events) + their screenshots now
        // that the Computer state guard is dropped.
        if let Some(rid) = &run_id {
            for (name, input, shots) in &persisted {
                let s = bump(&mut seq);
                runs_event(
                    &app, &client, &base, &auth, rid, s, "tool_use",
                    json!({"name": name, "input": input}), None, None,
                )
                .await;
                for shot in shots {
                    // Save the jpeg to LOCAL disk and record the absolute file
                    // path in the screenshot event so the UI can load it back
                    // off this machine.
                    let fseq = bump(&mut shot_seq);
                    // The event's seq is allocated HERE, before the upload is
                    // enqueued, so both destinations describe the frame with the
                    // SAME number. They used to disagree: the local event took
                    // the loop's `seq` while the upload carried `shot_seq`, a
                    // separate counter. That produced two rows per frame, and
                    // the uploaded one landed at a seq the console's since_seq
                    // cursor had already passed — so the frames the operator
                    // came to see were the one thing an incremental poll could
                    // never deliver. `shot_seq` still names the local FILE,
                    // where a dense 0,1,2… is what makes a run's directory
                    // readable by hand.
                    let s = bump(&mut seq);
                    // ...and mirror the SAME bytes off-machine so the admin
                    // console can watch this run without a remote desktop. This
                    // is `shot` — the image block the model was actually sent,
                    // already downscaled to the vision budget — not a fresh
                    // capture, so the console shows the evidence rather than a
                    // re-enactment, at a fraction of the bytes.
                    //
                    // Fire-and-forget by construction: `enqueue_run_shot` spawns
                    // and returns, so the turn never waits on storage, and it
                    // drops the frame outright when its in-flight ceiling is
                    // reached rather than growing a backlog behind the loop.
                    // Not gated on the local write: the two destinations fail
                    // independently.
                    crate::screenshots::enqueue_run_shot(
                        &app, &client, &base, &auth, rid, s, shot,
                    );
                    if let Some(local_path) =
                        runs_save_screenshot_local(&app, rid, fseq, shot)
                    {
                        runs_event(
                            &app, &client, &base, &auth, rid, s, "screenshot",
                            json!({}), Some(&local_path), Some("screenshot_local"),
                        )
                        .await;
                    }
                }
            }
        }

        messages.push(json!({"role": "user", "content": results}));
        prune_images(&mut messages, KEEP_RECENT_IMAGES);
        // Images are the bulk, but on a run of hundreds of turns assistant and
        // tool_result TEXT alone will fill the window; bound it too.
        prune_text(&mut messages, KEEP_RECENT_TURNS);
        // Re-place the rolling cache breakpoint on the newest settled (stubbed)
        // message AFTER pruning advances the frontier this turn.
        set_rolling_cache(&mut messages);
    }

    // Only reachable with a bounded cap; name the number, since it is now
    // configurable and "max iterations" alone no longer says which one was hit.
    let msg = format!("reached max iterations ({max_iters}) without finishing");
    let _ = app.emit(EV_ERROR, json!({ "error": msg.clone() }));
    let _ = app
        .notification()
        .builder()
        .title("ScreenBuddy — run failed")
        .body(msg.clone())
        .show();
    if let Some(rid) = &run_id {
        runs_finalize(
            &app, &client, &base, &auth, rid, "failed", steps, total_in, total_out, total_cache_create, total_cache_read,
            Value::Null, Some(&msg),
        )
        .await;
    }
}

// ---- Tauri commands -------------------------------------------------------

/// Shared run-start path: lock-check AgentState (reject if a non-cancelled token
/// already exists), install a fresh CancellationToken, and spawn `run_agent` on
/// the background runtime. Both the `start_agent_task` command (user-initiated)
/// and the remote WebSocket listener (backend-initiated) funnel through here so
/// a remotely-started run is indistinguishable from a normal one — same lock,
/// same RunLease, same persistence. Returns "an agent task is already running"
/// (verbatim) when busy, so callers can detect contention.
///
/// `endpoint` is the model endpoint the caller was told to use — the dispatched
/// `run` frame's value for a remote run, `None` for a local launch that has no
/// opinion. It is passed through untouched; `run_agent` resolves it against the
/// env var and the default (frame > env var > default).
pub(crate) fn start_run_internal(
    app: &AppHandle,
    state: &AgentState,
    prompt: String,
    auth: String,
    pinned_set_ids: Vec<String>,
    run_id: String,
    model: Option<String>,
    endpoint: Option<String>,
    backend: String,
) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| format!("agent state poisoned: {e}"))?;
    if let Some(existing) = guard.as_ref() {
        if !existing.is_cancelled() {
            return Err("an agent task is already running".to_string());
        }
    }
    let token = CancellationToken::new();
    *guard = Some(token.clone());
    drop(guard);

    tauri::async_runtime::spawn(run_agent(
        app.clone(),
        prompt,
        auth,
        backend,
        pinned_set_ids,
        run_id,
        model,
        endpoint,
        token,
    ));
    Ok(())
}

/// Start the agent loop for `prompt` on a background tokio task. Returns
/// immediately; progress is reported via `agent://*` events. Errors if a task
/// is already running.
#[tauri::command]
pub fn start_agent_task(
    app: AppHandle,
    state: tauri::State<'_, AgentState>,
    prompt: String,
    auth: Option<String>,
    pinned_set_ids: Vec<String>,
    run_id: String,
    model: Option<String>,
    model_endpoint: Option<String>,
    backend: Option<String>,
) -> Result<(), String> {
    // Run-persistence base comes from the frontend (its VITE_CU_BACKEND_URL, which
    // is correct in release builds). Fall back to the env/localhost default only
    // when the caller didn't supply one.
    let backend = backend.unwrap_or_else(backend_url);

    start_run_internal(
        &app,
        &state,
        prompt,
        auth.unwrap_or_default(),
        pinned_set_ids,
        run_id,
        model,
        model_endpoint,
        backend,
    )
}

/// Cancel the in-flight agent task (if any). Safe to call when nothing runs.
#[tauri::command]
pub fn stop_agent_task(state: tauri::State<'_, AgentState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| format!("agent state poisoned: {e}"))?;
    if let Some(token) = guard.take() {
        token.cancel();
    }
    Ok(())
}

// ---- tests (no real Claude / no OS input) ---------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- model endpoint resolution + the worker guard ---------------------

    /// `resolve_endpoint` and `auto_screenshot_enabled` read process env, which
    /// is shared by every test thread. Serialize the ones that set it and put it
    /// back afterwards, so a test can neither see another test's value nor leave
    /// one behind for the rest of the suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard(Vec<(&'static str, Option<String>)>);

    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(k, v)| {
                    let prev = std::env::var(k).ok();
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                    (*k, prev)
                })
                .collect();
            EnvGuard(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, prev) in &self.0 {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// The whole point of the change: a dispatched run drives the endpoint it was
    /// told to, even on a machine whose shell says something else. Before this,
    /// the env var won and the frame was not carried at all.
    #[test]
    fn frame_endpoint_beats_env_var() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[
            ("CU_ANTHROPIC_BASE", Some("http://stale-shell:9999")),
            ("CU_MODEL", Some("shell-model")),
        ]);
        let ep = resolve_endpoint(Some("http://fleet-llm:8080"), Some("fleet-model"));
        assert_eq!(ep.base, "http://fleet-llm:8080");
        assert_eq!(ep.model, "fleet-model");
        assert_eq!(ep.source, EndpointSource::Fleet);
    }

    /// An older backend omits the field entirely; that machine must behave
    /// exactly as it did before, which means the env var.
    #[test]
    fn env_var_used_when_the_frame_omits_the_endpoint() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[
            ("CU_ANTHROPIC_BASE", Some("http://self-hosted:8080")),
            ("CU_MODEL", None),
        ]);
        let ep = resolve_endpoint(None, None);
        assert_eq!(ep.base, "http://self-hosted:8080");
        assert_eq!(ep.model, DEFAULT_MODEL);
        assert_eq!(ep.source, EndpointSource::Env);
    }

    #[test]
    fn default_endpoint_when_nothing_says_otherwise() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[("CU_ANTHROPIC_BASE", None), ("CU_MODEL", None)]);
        let ep = resolve_endpoint(None, None);
        assert_eq!(ep.base, DEFAULT_ANTHROPIC_BASE);
        assert_eq!(ep.source, EndpointSource::Default);
        assert!(ep.is_anthropic());
    }

    /// An operator who clears the settings field sends `""`, not nothing. That
    /// must fall through the ladder rather than resolve to an empty base — an
    /// empty base builds the URL "/v1/messages" and fails with a transport error
    /// that names nothing.
    #[test]
    fn blank_frame_endpoint_falls_through_to_the_env_var() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[("CU_ANTHROPIC_BASE", Some("http://self-hosted:8080"))]);
        let ep = resolve_endpoint(Some("   "), None);
        assert_eq!(ep.base, "http://self-hosted:8080");
        assert_eq!(ep.source, EndpointSource::Env);
    }

    /// Endpoint and model walk the ladder independently: a fleet that sets one
    /// and not the other must not drag the machine's value for the other along.
    #[test]
    fn model_resolves_independently_of_the_endpoint() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[
            ("CU_ANTHROPIC_BASE", Some("http://stale-shell:9999")),
            ("CU_MODEL", Some("shell-model")),
        ]);
        let ep = resolve_endpoint(Some("http://fleet-llm:8080"), None);
        assert_eq!(ep.base, "http://fleet-llm:8080");
        assert_eq!(ep.model, "shell-model");
        assert_eq!(ep.source, EndpointSource::Fleet);
    }

    #[test]
    fn messages_url_does_not_double_the_slash() {
        let ep = ResolvedEndpoint {
            base: "http://self-hosted:8080/".into(),
            model: DEFAULT_MODEL.into(),
            source: EndpointSource::Fleet,
        };
        assert_eq!(ep.messages_url(), "http://self-hosted:8080/v1/messages");
    }

    /// Auto-screenshot follows THIS run's endpoint. A fleet run at a self-hosted
    /// server from a shell with no override used to get the Anthropic default
    /// (off) and fly blind.
    #[test]
    fn auto_screenshot_follows_the_runs_endpoint_not_the_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::set(&[("CU_AUTO_SCREENSHOT", None), ("CU_ANTHROPIC_BASE", None)]);
        assert!(!auto_screenshot_enabled(DEFAULT_ANTHROPIC_BASE));
        assert!(auto_screenshot_enabled("http://self-hosted:8080"));
    }

    /// The failure this whole change exists to stop: a fleet worker pointed at
    /// Anthropic. It must refuse, not succeed quietly.
    #[test]
    fn worker_pointed_at_anthropic_is_refused() {
        let err = anthropic_guard_error(DEFAULT_ANTHROPIC_BASE, true, false)
            .expect("an enrolled worker on api.anthropic.com must be refused");
        assert!(err.contains("api.anthropic.com"), "names the endpoint: {err}");
        assert!(err.contains(ALLOW_ANTHROPIC_ENV), "names the opt-out: {err}");
    }

    /// The asymmetry. Same endpoint, operator machine (session credential): this
    /// is BYOK on your own laptop and has always been the normal case.
    #[test]
    fn operator_on_anthropic_is_unaffected() {
        assert!(anthropic_guard_error(DEFAULT_ANTHROPIC_BASE, false, false).is_none());
    }

    #[test]
    fn worker_on_a_self_hosted_endpoint_runs() {
        assert!(anthropic_guard_error("http://self-hosted:8080", true, false).is_none());
    }

    #[test]
    fn worker_may_opt_in_to_anthropic_explicitly() {
        assert!(anthropic_guard_error(DEFAULT_ANTHROPIC_BASE, true, true).is_none());
    }

    /// An empty base is treated as Anthropic by `endpoint_is_anthropic` (it is
    /// what the default resolves to), so the guard must catch that too rather
    /// than waving through a misconfiguration that lands on the default host.
    #[test]
    fn worker_with_an_empty_base_is_refused_too() {
        assert!(anthropic_guard_error("", true, false).is_some());
    }

    fn feed(blob: &str) -> SseAccumulator {
        let mut acc = SseAccumulator::new();
        for line in blob.split('\n') {
            acc.feed_line(line, None);
        }
        acc
    }

    /// A turn where the model emits one `computer` left_click tool_use, split
    /// across input_json_delta chunks, then stops with stop_reason "tool_use".
    /// Verifies the parser assembles the tool_use block and that the loop's
    /// dispatch/terminate logic would run a tool (not end the turn).
    #[test]
    fn parses_tool_use_turn_and_builds_tool_result() {
        let sse = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[]}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Clicking the button.\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"computer\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"action\\\":\\\"left_\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"click\\\",\\\"coordinate\\\":[120,240]}\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":1}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}

event: message_stop
data: {\"type\":\"message_stop\"}
";
        let acc = feed(sse);
        assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
        assert!(acc.error.is_none());
        let stop = acc.stop_reason.clone();
        let content = acc.into_content();
        assert_eq!(content.len(), 2, "text + tool_use");

        let tool_uses: Vec<&Value> = content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .collect();
        assert_eq!(tool_uses.len(), 1);
        let tu = tool_uses[0];
        assert_eq!(tu["name"], "computer");
        assert_eq!(tu["input"]["action"], "left_click");
        assert_eq!(tu["input"]["coordinate"][0], 120);
        assert_eq!(tu["input"]["coordinate"][1], 240);

        // The loop would NOT terminate here (stop_reason != end_turn and a
        // tool_use is present) — it would dispatch the action. Confirm the
        // terminate gate agrees.
        let should_continue = stop.as_deref() != Some("end_turn") && !tool_uses.is_empty();
        assert!(should_continue, "turn with a tool_use must not terminate");

        // And a tool_result is buildable from a (would-be) successful dispatch.
        let id = tu["id"].as_str().unwrap();
        let res = tool_result(id, ok_text("left_click at (120, 240)"));
        assert_eq!(res["tool_use_id"], "toolu_1");
        assert_eq!(res["is_error"], false);
        assert_eq!(res["content"][0]["type"], "text");
    }

    /// A plain text turn ending with stop_reason "end_turn" terminates the loop.
    #[test]
    fn terminates_on_end_turn() {
        let sse = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"All done.\"}}
data: {\"type\":\"content_block_stop\",\"index\":0}
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}
data: {\"type\":\"message_stop\"}
";
        let acc = feed(sse);
        assert_eq!(acc.stop_reason.as_deref(), Some("end_turn"));
        let stop = acc.stop_reason.clone();
        let content = acc.into_content();
        let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
        let should_terminate = stop.as_deref() == Some("end_turn") || tool_uses.is_empty();
        assert!(should_terminate);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "All done.");
    }

    /// An `error` SSE event surfaces as a stream error.
    #[test]
    fn surfaces_stream_error() {
        let sse = "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n";
        let acc = feed(sse);
        assert_eq!(acc.error.as_deref(), Some("overloaded"));
    }

    /// Image pruning keeps only the N most recent screenshots.
    #[test]
    fn prunes_old_images() {
        let img = || json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "x"}});
        // Index 0 is the (exempt) seeded user message; the 5 image-bearing
        // tool_result messages that follow are the prunable ones.
        let mut messages = vec![json!({"role": "user", "content": [{"type": "text", "text": "seed"}]})];
        for i in 0..5 {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("t{i}"),
                    "is_error": false,
                    "content": [img()]
                }]
            }));
        }
        prune_images(&mut messages, 2);
        let mut images = 0;
        let mut placeholders = 0;
        for m in &messages {
            let inner = &m["content"][0]["content"][0];
            match inner["type"].as_str() {
                Some("image") => images += 1,
                Some("text") => placeholders += 1,
                _ => {}
            }
        }
        assert_eq!(images, 2, "two most recent images kept");
        assert_eq!(placeholders, 3, "three older images replaced");
    }

    /// Build a conversation of `turns` assistant/tool_result pairs, each
    /// carrying one long text block (assistant text + tool_result text), after
    /// the exempt seed at index 0.
    fn text_convo(turns: usize) -> Vec<Value> {
        let long = |tag: &str| format!("{tag} {}", "x".repeat(MAX_TEXT_CHARS + 10));
        let mut messages =
            vec![json!({"role": "user", "content": [{"type": "text", "text": long("seed")}]})];
        for i in 0..turns {
            messages.push(json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": long(&format!("think{i}"))},
                    {"type": "tool_use", "id": format!("t{i}"), "name": "computer",
                     "input": {"action": "screenshot"}}
                ]
            }));
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("t{i}"),
                    "is_error": false,
                    "content": [{"type": "text", "text": long(&format!("result{i}"))}]
                }]
            }));
        }
        messages
    }

    /// Text pruning truncates old blocks, leaves the recent window and the
    /// exempt seed verbatim, and never touches message/block structure.
    #[test]
    fn prunes_old_text() {
        // Two turns kept; enough older turns to clear the batch threshold.
        let turns = 2 + TEXT_PRUNE_BATCH;
        let mut messages = text_convo(turns);
        let before = messages.len();
        prune_text(&mut messages, 2);

        assert_eq!(messages.len(), before, "no message added or removed");
        // Seed is exempt.
        assert!(!messages[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with(TEXT_STUB));
        // The oldest turn's assistant text and tool_result text are truncated.
        assert!(messages[1]["content"][0]["text"].as_str().unwrap().ends_with(TEXT_STUB));
        assert!(messages[2]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with(TEXT_STUB));
        // The two most recent turns (last 4 messages) are untouched.
        for m in &messages[before - 4..] {
            let s = serde_json::to_string(m).unwrap();
            assert!(!s.contains(TEXT_STUB), "recent turns kept verbatim");
        }
        // tool_use / tool_result pairing survives intact.
        for i in 0..turns {
            let id = format!("t{i}");
            assert_eq!(messages[1 + i * 2]["content"][1]["id"], json!(id));
            assert_eq!(messages[2 + i * 2]["content"][0]["tool_use_id"], json!(id));
        }

        // Idempotent: a second pass finds nothing long enough left to batch.
        let snapshot = messages.clone();
        prune_text(&mut messages, 2);
        assert_eq!(messages, snapshot, "second pass is a no-op");
    }

    /// Below the batch threshold nothing is truncated — we pay the cached-prefix
    /// invalidation once per batch, not once per turn.
    #[test]
    fn prune_text_batches() {
        // One prunable turn == 2 long blocks, well under TEXT_PRUNE_BATCH.
        let mut messages = text_convo(3);
        let snapshot = messages.clone();
        prune_text(&mut messages, 2);
        assert_eq!(messages, snapshot, "under the batch threshold: no-op");
    }

    /// Short text is never truncated, however old, and the truncation marker is
    /// distinct from IMAGE_STUB so the rolling cache frontier is unaffected.
    #[test]
    fn prune_text_leaves_short_blocks_and_stubs() {
        let mut messages = text_convo(2 + TEXT_PRUNE_BATCH);
        // Turn 0's tool_result is a pruned screenshot, not long text.
        messages[2]["content"][0]["content"][0] = json!({"type": "text", "text": IMAGE_STUB});
        // Turn 1's assistant text is short.
        messages[3]["content"][0]["text"] = json!("ok");
        prune_text(&mut messages, 2);
        assert_eq!(messages[2]["content"][0]["content"][0]["text"], json!(IMAGE_STUB));
        assert_eq!(messages[3]["content"][0]["text"], json!("ok"));
        assert!(!IMAGE_STUB.contains(TEXT_STUB.trim()));
    }

    /// The rolling cache breakpoint lands on exactly ONE message (index >= 1) —
    /// the newest one carrying a stubbed screenshot — and re-running it does not
    /// accumulate markers.
    #[test]
    fn rolling_cache_marks_newest_stub_only() {
        let stub = || json!({"type": "text", "text": IMAGE_STUB});
        let img = || json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "x"}});
        let tr = |inner: Value| {
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result", "tool_use_id": "t", "is_error": false,
                    "content": [inner]
                }]
            })
        };
        // index 0 seed (exempt), then two stubbed + two live screenshots.
        let mut messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "seed"}]}),
            tr(stub()), // 1 — older stub
            tr(stub()), // 2 — NEWEST stub (the breakpoint target)
            tr(img()),  // 3 — live (still mutable)
            tr(img()),  // 4 — live (still mutable)
        ];

        // Count cache_control on top-level content blocks of messages[1..].
        let count_cc = |msgs: &Vec<Value>| -> usize {
            msgs.iter()
                .skip(1)
                .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                .filter(|b| b.get("cache_control").is_some())
                .count()
        };

        set_rolling_cache(&mut messages);
        assert_eq!(count_cc(&messages), 1, "exactly one rolling breakpoint");
        // It's on the newest-stub message (index 2), last top-level block.
        let last = messages[2]["content"].as_array().unwrap().last().unwrap();
        assert!(last.get("cache_control").is_some(), "marked on newest stub");
        // Seed (index 0) is untouched here.
        assert!(messages[0]["content"][0].get("cache_control").is_none());

        // Idempotent: re-running strips then re-adds — does not accumulate.
        set_rolling_cache(&mut messages);
        assert_eq!(count_cc(&messages), 1, "no marker accumulation");
        assert!(messages[2]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .get("cache_control")
            .is_some());
    }

    /// With no stubbed screenshots yet (run younger than the keep-window),
    /// set_rolling_cache adds no breakpoint.
    #[test]
    fn rolling_cache_noop_without_stub() {
        let img = || json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "x"}});
        let mut messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "seed"}]}),
            json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "t", "is_error": false, "content": [img()]
            }]}),
        ];
        set_rolling_cache(&mut messages);
        let any_cc = messages
            .iter()
            .skip(1)
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .any(|b| b.get("cache_control").is_some());
        assert!(!any_cc, "no breakpoint when nothing is settled yet");
    }
}
