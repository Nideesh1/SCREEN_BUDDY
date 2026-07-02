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
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio_util::sync::CancellationToken;

use crate::{capture, with_computer, ComputerState};

// ---- configuration (env-overridable) --------------------------------------

fn backend_url() -> String {
    std::env::var("CU_BACKEND_URL").unwrap_or_else(|_| "http://localhost:8000".to_string())
}
/// BYOK: the per-turn model call goes DIRECTLY to Anthropic with the user's own
/// key — it never touches our backend. The backend (`backend_url`) is still used
/// for run persistence only.
fn anthropic_base() -> String {
    std::env::var("CU_ANTHROPIC_BASE").unwrap_or_else(|_| "https://api.anthropic.com".to_string())
}
/// Anthropic Messages API version header.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header enabling the official enhanced computer-use tool
/// (`computer_20251124`). We send this directly now (the backend used to add it).
const ANTHROPIC_BETA: &str = "computer-use-2025-11-24";
fn model() -> String {
    // Default to Claude Opus 4.8 (claude-opus-4-8); we send a concrete id in the
    // Messages body straight to Anthropic.
    std::env::var("CU_MODEL").unwrap_or_else(|_| "claude-opus-4-8".to_string())
}

const MAX_ITERS: usize = 150;
const MAX_TOKENS: u32 = 4096;
/// Keep only the N most recent screenshots in context; older ones are replaced
/// with a placeholder so the conversation doesn't balloon (loop.py's
/// keep-N-recent image pruning). Set to 2 (was 3): one fewer live full image per
/// turn trims tokens, and the rolling cache breakpoint + the official tool's
/// zoom action cover settled-context and on-demand-detail respectively.
const KEEP_RECENT_IMAGES: usize = 2;

const SYSTEM_PROMPT: &str = "You are ScreenBuddy, a computer-use agent operating a macOS desktop on the \
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
        "screenshot" => match capture::take_screenshot() {
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

/// Attach the session bearer (if any) to an outgoing request.
fn with_bearer(req: reqwest::RequestBuilder, auth: &str) -> reqwest::RequestBuilder {
    if auth.is_empty() {
        req
    } else {
        req.header("authorization", format!("Bearer {auth}"))
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
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: &str,
    status: &str,
) {
    let url = format!("{base}/runs/{run_id}");
    let body = json!({ "status": status });
    match with_bearer(client.patch(&url).json(&body), auth).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("[runs] status '{status}': HTTP {}", r.status()),
        Err(e) => eprintln!("[runs] status '{status}': request failed: {e}"),
    }
}

/// `POST /runs/{id}/events` — best effort, logs and swallows failures.
#[allow(clippy::too_many_arguments)]
async fn runs_event(
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
    match with_bearer(client.post(&url).json(&body), auth).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("[runs] event '{ev_type}': HTTP {}", r.status()),
        Err(e) => eprintln!("[runs] event '{ev_type}': request failed: {e}"),
    }
}

/// Save a screenshot to LOCAL disk under the app data dir and return its
/// absolute path on success (for the follow-up screenshot event), or `None`
/// (logged) on any failure. Screenshots are the most sensitive data, so they
/// NEVER leave the user's Mac — there is no server upload here.
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
    match with_bearer(client.patch(&url).json(&body), auth).send().await {
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
    token: CancellationToken,
) {
    // Release the AgentState lock when this run ends, however it ends.
    let _lease = RunLease { app: app.clone(), token: token.clone() };

    let client = reqwest::Client::new();
    let base = backend;
    // BYOK: the per-turn model call goes DIRECTLY to Anthropic. `base` is kept
    // for run persistence (POST /runs, events, PATCH) with the session token.
    let url = format!("{}/v1/messages", anthropic_base());
    // Honor the caller-selected model (launcher's picker); fall back to the
    // env/default when none was supplied.
    let model = model_arg.unwrap_or_else(model);
    let cred_tool = use_credential_tool();

    // --- persistence bootstrap ---
    // The run row is minted by the FRONTEND via `POST /runs` before this task is
    // spawned; we receive the pre-created `run_id`, announce it, and stamp the
    // status to "running" (the backend stamps `started_at`). All downstream
    // persistence keeps the `Option<String>` shape so it stays best-effort.
    let _ = app.emit(EV_RUN_STARTED, json!({ "run_id": run_id }));
    runs_patch_status(&client, &base, &auth, &run_id, "running").await;
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

    // BYOK is mandatory: read the user's own Anthropic key once, up front. The
    // per-turn model call goes DIRECTLY to Anthropic with this key — it never
    // touches our backend. If no key is stored, fail fast with a clear,
    // user-facing error (not a raw 401 from a later request).
    let api_key = match crate::credentials::anthropic_key(&app) {
        Some(k) => k,
        None => {
            let msg = "No Anthropic API key set — add one in Settings";
            let _ = app.emit(EV_ERROR, json!({ "error": msg }));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &client, &base, &auth, rid, "failed", steps, total_in, total_out,
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
    let (disp_w, disp_h) = match capture::take_screenshot() {
        Ok(cap) => {
            last_sent = Some((cap.sent_w, cap.sent_h));
            let comp_state = app.state::<ComputerState>();
            if let Ok(mut g) = comp_state.0.lock() {
                if let Some(c) = g.as_mut() {
                    c.set_screenshot_size(cap.sent_w as i32, cap.sent_h as i32);
                }
            }
            (cap.sent_w, cap.sent_h)
        }
        Err(e) => {
            eprintln!("[agent] initial capture failed ({e}); sizing tool from monitor");
            capture::primary_sent_size().unwrap_or((1024, 768))
        }
    };
    let tool = computer_tool(disp_w, disp_h);

    for turn in 1..=MAX_ITERS {
        if token.is_cancelled() {
            let _ = app.emit(EV_DONE, json!({"reason": "cancelled"}));
            if let Some(rid) = &run_id {
                runs_finalize(
                    &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
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
                &client, &base, &auth, rid, s, "status",
                json!({"turn": turn, "state": "running"}), None, None,
            )
            .await;
        }

        let body = json!({
            "model": model,
            "system": SYSTEM_PROMPT,
            "messages": messages,
            "tools": [tool, cred_tool],
            "max_tokens": MAX_TOKENS,
            // The backend used to force streaming when it relayed; talking to
            // Anthropic directly, we must set it ourselves so the SSE parser has
            // events to consume.
            "stream": true,
        });

        let turn_ok = match stream_turn(&client, &url, &api_key, &body, &app, &token).await {
            Ok(r) => r,
            Err(TurnError::Cancelled) => {
                let _ = app.emit(EV_DONE, json!({"reason": "cancelled"}));
                if let Some(rid) = &run_id {
                    runs_finalize(
                        &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
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
                        &client, &base, &auth, rid, "failed", steps, total_in, total_out, total_cache_create, total_cache_read,
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
                    &client, &base, &auth, rid, s, "text",
                    json!({"text": turn_text}), None, None,
                )
                .await;
            }
        }

        messages.push(json!({"role": "assistant", "content": content.clone()}));

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
                    &client, &base, &auth, rid, "completed", steps, total_in, total_out, total_cache_create, total_cache_read,
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
                    let outcome =
                        dispatch_action(&app, &comp_state, action, &tu["input"], &mut last_sent);
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
                    &client, &base, &auth, rid, "cancelled", steps, total_in, total_out, total_cache_create, total_cache_read,
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
                    &client, &base, &auth, rid, s, "tool_use",
                    json!({"name": name, "input": input}), None, None,
                )
                .await;
                for shot in shots {
                    // Save the jpeg to LOCAL disk (never uploaded) and record the
                    // absolute file path in the screenshot event so the UI can load
                    // it back off the user's Mac.
                    let fseq = bump(&mut shot_seq);
                    if let Some(local_path) =
                        runs_save_screenshot_local(&app, rid, fseq, shot)
                    {
                        let s = bump(&mut seq);
                        runs_event(
                            &client, &base, &auth, rid, s, "screenshot",
                            json!({}), Some(&local_path), Some("screenshot_local"),
                        )
                        .await;
                    }
                }
            }
        }

        messages.push(json!({"role": "user", "content": results}));
        prune_images(&mut messages, KEEP_RECENT_IMAGES);
        // Re-place the rolling cache breakpoint on the newest settled (stubbed)
        // message AFTER pruning advances the frontier this turn.
        set_rolling_cache(&mut messages);
    }

    let _ = app.emit(EV_ERROR, json!({"error": "reached max iterations without finishing"}));
    let _ = app
        .notification()
        .builder()
        .title("ScreenBuddy — run failed")
        .body("reached max iterations without finishing")
        .show();
    if let Some(rid) = &run_id {
        runs_finalize(
            &client, &base, &auth, rid, "failed", steps, total_in, total_out, total_cache_create, total_cache_read,
            Value::Null, Some("reached max iterations without finishing"),
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
pub(crate) fn start_run_internal(
    app: &AppHandle,
    state: &AgentState,
    prompt: String,
    auth: String,
    pinned_set_ids: Vec<String>,
    run_id: String,
    model: Option<String>,
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
