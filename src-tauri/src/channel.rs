//! The diary client — this worker's end of the backend's per-machine channel
//! (`/channel/{device_id}` on the backend) and the task-pickup loop that turns
//! queued tasks into runs.
//!
//! The backend keeps ONE append-only, seq-ordered message log per worker; both
//! sides write into it. This module is everything the desktop does with it:
//!
//!   - append `status` / `receipt` / `question` entries (sender-minted msg_id,
//!     so a retried POST replays idempotently and can never duplicate),
//!   - read admin→worker messages after a seq (optionally long-polling),
//!   - advance the worker-owned consumption cursor (forward-only),
//!   - `ask_operator`: post a `question` and block until a `verdict` answers it,
//!   - the idle task-pickup loop (`task_pickup_loop`): poll `/tasks/next`,
//!     read the task back to the operator, and start the approved work through
//!     `agent::start_run_internal` — the same path every other run takes,
//!   - the turn-boundary drain (`TurnDrain`) the agent loop calls so a nudge
//!     posted mid-run reaches the model at the next request.
//!
//! Two rules shape everything here:
//!
//!   1. msg_ids are DETERMINISTIC wherever a message describes a fact that can
//!      be re-derived (`readback-{task_id}[-g{hash}]`, `receipt-{msg_id}`,
//!      `outcome-{run_id}`). The backend replays a known msg_id as the original
//!      row, so a crash-and-redeliver cycle re-sends the same message and the
//!      log stays clean — idempotency is carried by the NAME, not by local
//!      state that a crash would lose. The `-g{hash}` suffix exists because a
//!      SENT-BACK task (awaiting_verdict→queued with a `last_directive`) is a
//!      NEW question, not a replay of the old one — see `generation_suffix`.
//!
//!   2. The server cursor is advanced only AFTER the turn that consumed a batch
//!      has actually completed. A crash between fetch and completion therefore
//!      redelivers the batch — which rule 1 makes safe — instead of silently
//!      dropping messages the model never saw.
//!
//! Everything network-facing is best-effort or retried, never fatal to a run;
//! the one deliberate exception is `ask_operator`, which retries FOREVER —
//! a backend blip during a readback wait must never silently unblock a worker
//! that is supposed to be standing still.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::agent::{backend_url, with_bearer};

// ---- tuning ---------------------------------------------------------------

/// How often the idle worker asks `/tasks/next` when the doorbell is silent.
/// The doorbell (`{"type":"mail"}` on the remote WS) makes new work land in
/// milliseconds when the socket is up; this interval is only the reconcile
/// floor for a machine whose socket is down or whose doorbell got lost.
const TASK_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Long-poll hold we ask the backend for when waiting on a verdict. Matches the
/// backend's WAIT_MAX_SECONDS (25s, chosen there to stay under 30s proxy idle
/// timeouts) — asking for more would just be clamped.
const VERDICT_WAIT_SECS: f64 = 25.0;

/// Backoff bounds for retried channel requests (same envelope as remote.rs's
/// reconnect: 1s → 30s, doubling).
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Bounded retries for the task-lifecycle PATCHes. Unlike the verdict wait,
/// these have a sane failure mode (leave the task where it is and tell the
/// operator via a status message), so they should give up rather than wedge
/// the pickup loop forever on a dead backend.
const PATCH_RETRIES: usize = 4;

/// Page size for the drain's one GET per turn. The backend caps at 500; asking
/// for the max means a burst of operator messages is still one request.
const DRAIN_LIMIT: usize = 500;

/// How much of a task's spec the readback echoes. The full spec can be 20KB and
/// the channel payload cap is 16KB serialized; the readback is a comprehension
/// check, not an archive — the operator wrote the spec and has it open.
const READBACK_SPEC_CHARS: usize = 2000;

/// Bounds for the checklist and send-back note inside the readback, same 16KB
/// payload-cap reasoning as READBACK_SPEC_CHARS. Worst case the readback text
/// is ~2000 (spec) + ~1200 (checklist) + ~500 (directive) + ~300 (fixed lines)
/// ≈ 4000 chars — comfortably inside the cap even at 4 bytes/char. The RUN
/// prompt is deliberately NOT truncated (see `task_run_spec`): the spec is
/// server-capped and the model, not the channel, is that budget's owner.
const READBACK_CHECKLIST_CHARS: usize = 1200;
const READBACK_CHECKLIST_ITEM_CHARS: usize = 200;
const READBACK_DIRECTIVE_CHARS: usize = 500;

// ---- managed state ---------------------------------------------------------

/// A latching wake-up: `ring` from the WS read loop, `wait` from the idle
/// poller. The pending flag is what makes a ring that arrives while the poller
/// is BUSY (mid-poll, mid-task) survive until its next wait instead of being
/// lost — `Notify` alone stores at most one permit for a future waiter, and the
/// flag makes that guarantee inspectable (and testable) rather than implicit.
/// Any number of rings between two waits collapse into ONE wake: the doorbell
/// says "you have mail", never how much, and the poll that follows reconciles.
pub struct Doorbell {
    pending: AtomicBool,
    notify: Notify,
}

impl Doorbell {
    fn new() -> Self {
        Self { pending: AtomicBool::new(false), notify: Notify::new() }
    }

    /// Note that mail exists and wake the waiter, if any. Cheap, sync, safe to
    /// call from the WS read loop.
    pub fn ring(&self) {
        self.pending.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Consume the pending flag. True at most once per ring-burst.
    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }

    /// Wait until rung, `timeout` elapses, or `cancel` fires. Returns true when
    /// the wake was the doorbell (the caller may want to poll immediately).
    async fn wait(&self, timeout: Duration, cancel: &CancellationToken) -> bool {
        if self.take_pending() {
            return true;
        }
        tokio::select! {
            _ = cancel.cancelled() => false,
            _ = self.notify.notified() => self.take_pending() || true,
            _ = tokio::time::sleep(timeout) => self.take_pending(),
        }
    }
}

/// How a run ended, recorded by `agent::runs_finalize` (the single funnel every
/// terminal path already goes through) so the pickup loop can move the task
/// forward without a backend read — a device token cannot GET /runs/{id}, so
/// the only copy of the outcome a worker can consult is its own.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub run_id: String,
    pub status: String,
    pub error: Option<String>,
}

/// Managed state for the diary. One per app, created in lib.rs.
pub struct ChannelState {
    pub doorbell: Doorbell,
    /// Cancels the pickup loop's waits so the app can shut down cleanly.
    pub shutdown: CancellationToken,
    /// The most recent run outcome (see `RunOutcome`). One slot, not a queue:
    /// the AgentState lock guarantees one run at a time, and the pickup loop
    /// reads it immediately after the run's token cancels.
    outcome: Mutex<Option<RunOutcome>>,
    /// msg_ids of verdicts `ask_operator` already consumed, so the boundary
    /// drain — which will see the same rows again (see `ask_operator` for why)
    /// — can receipt them honestly as "answered our question" rather than as
    /// strays.
    handled_verdicts: Mutex<HashSet<String>>,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            doorbell: Doorbell::new(),
            shutdown: CancellationToken::new(),
            outcome: Mutex::new(None),
            handled_verdicts: Mutex::new(HashSet::new()),
        }
    }
}

/// Wake the idle poller: the backend appended admin mail. Called from the WS
/// read loop on a `{"type":"mail"}` frame. Best-effort by design — a lost ring
/// costs one poll interval of latency, never data, because both the idle poll
/// and the turn-boundary drain reconcile against the cursor regardless.
pub fn ring_doorbell(app: &AppHandle) {
    if let Some(state) = app.try_state::<ChannelState>() {
        state.doorbell.ring();
    }
}

/// Record how a run ended. Called by `agent::runs_finalize` — every terminal
/// path (completed / failed / cancelled / refused) already funnels through it,
/// so one call site covers them all, and the outcome is durably in state before
/// the RunLease drop cancels the token the pickup loop is waiting on.
pub(crate) fn note_run_outcome(app: &AppHandle, run_id: &str, status: &str, error: Option<&str>) {
    if let Some(state) = app.try_state::<ChannelState>() {
        if let Ok(mut g) = state.outcome.lock() {
            *g = Some(RunOutcome {
                run_id: run_id.to_string(),
                status: status.to_string(),
                error: error.map(str::to_string),
            });
        }
    }
}

fn take_outcome_for(app: &AppHandle, run_id: &str) -> Option<RunOutcome> {
    let state = app.try_state::<ChannelState>()?;
    let mut g = state.outcome.lock().ok()?;
    match g.as_ref() {
        Some(o) if o.run_id == run_id => g.take(),
        _ => None,
    }
}

fn mark_verdict_handled(app: &AppHandle, msg_id: &str) {
    if let Some(state) = app.try_state::<ChannelState>() {
        if let Ok(mut g) = state.handled_verdicts.lock() {
            g.insert(msg_id.to_string());
        }
    }
}

fn was_verdict_handled(app: &AppHandle, msg_id: &str) -> bool {
    app.try_state::<ChannelState>()
        .and_then(|s| s.handled_verdicts.lock().ok().map(|mut g| g.remove(msg_id)))
        .unwrap_or(false)
}

// ---- HTTP primitives -------------------------------------------------------

/// One diary entry as the wire wants it. `requires_reply` is derived from the
/// type (a question MUST set it; the backend 422s otherwise) so no caller can
/// build the contradiction.
fn message_body(
    msg_id: &str,
    mtype: &str,
    task_id: Option<&str>,
    in_reply_to: Option<&str>,
    payload: Value,
) -> Value {
    let mut body = json!({
        "msg_id": msg_id,
        "type": mtype,
        "requires_reply": mtype == "question",
        "payload": payload,
    });
    if let Some(t) = task_id {
        body["task_id"] = json!(t);
    }
    if let Some(r) = in_reply_to {
        body["in_reply_to"] = json!(r);
    }
    body
}

/// POST one message. One attempt; the caller decides the retry policy, because
/// "how hard to try" differs (a receipt is best-effort, a question is forever).
/// Returns the backend's `{msg_id, seq, server_ts}` ack.
async fn post_message(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    body: &Value,
) -> Result<Value, String> {
    let url = format!("{base}/channel/{device_id}/messages");
    let resp = with_bearer(app, client.post(&url).json(body), "")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let txt: String = txt.chars().take(300).collect();
        return Err(format!("HTTP {status}: {txt}"));
    }
    resp.json::<Value>().await.map_err(|e| format!("bad ack: {e}"))
}

/// POST with retry-forever-until-cancelled. Safe to hammer because the
/// sender-minted msg_id makes every replay idempotent: the worst a flaky
/// network can do is waste a seq, never duplicate the message. Returns `None`
/// only on cancellation.
async fn post_message_retrying(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    body: &Value,
    cancel: &CancellationToken,
) -> Option<Value> {
    let mut backoff = BACKOFF_START;
    loop {
        match post_message(app, client, base, device_id, body).await {
            Ok(ack) => return Some(ack),
            Err(e) => {
                let mtype = body.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                eprintln!("[channel] append '{mtype}' failed ({e}); retrying in {}s", backoff.as_secs());
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// GET the admin→worker messages strictly after `since_seq`, oldest first.
/// `wait` > 0 turns it into a long-poll (the backend holds up to 25s).
async fn admin_messages_since(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    since_seq: u64,
    wait: f64,
) -> Result<Vec<Value>, String> {
    let url = format!(
        "{base}/channel/{device_id}/messages?since_seq={since_seq}&from=admin&limit={DRAIN_LIMIT}&wait={wait}"
    );
    let resp = with_bearer(app, client.get(&url), "")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let txt: String = txt.chars().take(300).collect();
        return Err(format!("HTTP {status}: {txt}"));
    }
    // GET messages returns a BARE array, not an envelope.
    resp.json::<Vec<Value>>().await.map_err(|e| format!("bad body: {e}"))
}

/// Learn the server-side cursor. The read route (`GET /channel/{id}/cursor`) is
/// operator-only, so a device recovers its own high-water mark by REPLAYING the
/// PUT with 0: the backend answers a fresh channel with `{consumed_through: 0}`
/// (idempotent insert/replay) and an advanced one with a 409 whose detail
/// carries the current value — either way the value comes back, and a PUT of 0
/// can never move a forward-only cursor. Documented backend behavior, not a
/// probe of luck: the 409 detail exists exactly so a stale writer learns where
/// the cursor actually stands.
async fn cursor_fetch(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
) -> Result<u64, String> {
    let url = format!("{base}/channel/{device_id}/cursor");
    let resp = with_bearer(app, client.put(&url).json(&json!({"consumed_through": 0})), "")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("bad body: {e}"))?;
    if status.is_success() {
        return body
            .get("consumed_through")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "cursor reply missing consumed_through".to_string());
    }
    if status.as_u16() == 409 {
        // {"detail": {"error": "...", "consumed_through": N}}
        return body["detail"]
            .get("consumed_through")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "409 without consumed_through".to_string());
    }
    Err(format!("HTTP {status}: {body}"))
}

/// Advance the server cursor to `n`. A 409 (regression) is treated as success:
/// it means the server already stands PAST `n` — which can only happen if some
/// other instance of us advanced it — and "the messages are consumed" is then
/// already true.
async fn cursor_advance(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    n: u64,
) -> Result<(), String> {
    let url = format!("{base}/channel/{device_id}/cursor");
    let resp = with_bearer(app, client.put(&url).json(&json!({"consumed_through": n})), "")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 409 {
        return Ok(());
    }
    let txt = resp.text().await.unwrap_or_default();
    let txt: String = txt.chars().take(300).collect();
    Err(format!("HTTP {status}: {txt}"))
}

// ---- pure logic (unit-tested) ----------------------------------------------

/// Words that, opening a verdict, mean "do not proceed".
const REJECTION_WORDS: &[&str] = &["reject", "rejected", "deny", "denied", "abort", "stop"];

/// THE APPROVAL RULE, deliberately liberal: a verdict approves unless it
/// carries an EXPLICIT rejection marker. The operator answering a readback is
/// typing into a free-text box on a phone as often as clicking a button, and
/// the failure modes are asymmetric — treating "yes go ahead" as a rejection
/// stalls a machine until a human notices, while treating an ambiguous grunt as
/// approval starts work the operator is watching anyway (and can kill).
///
/// A verdict REJECTS when:
///   - a boolean `approved`/`approve` in the payload is `false`, or
///   - the first word of `decision`/`verdict`/`answer`/`text` (first of those
///     keys present; trailing `.,!?:;` stripped, case-insensitive) is one of
///     REJECTION_WORDS, or
///   - that field is exactly "no" — alone, so "no problem, go ahead" approves.
/// Everything else — including an empty payload — approves.
pub fn verdict_approves(payload: &Value) -> bool {
    for key in ["approved", "approve"] {
        if let Some(b) = payload.get(key).and_then(|v| v.as_bool()) {
            return b;
        }
    }
    let text = ["decision", "verdict", "answer", "text"]
        .iter()
        .find_map(|k| payload.get(*k).and_then(|v| v.as_str()));
    let Some(text) = text else { return true };
    let trimmed = text.trim().to_lowercase();
    if trimmed.trim_end_matches(['.', ',', '!', '?', ':', ';']) == "no" {
        return false;
    }
    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(['.', ',', '!', '?', ':', ';']);
    !REJECTION_WORDS.contains(&first)
}

/// Head of `s` cut on a char boundary with an ellipsis marker (byte slicing
/// would panic mid-UTF-8; same shape as agent.rs's `truncate_text`).
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// One line describing where the work would happen, from the task's
/// `workspace` object, or None when the task has none.
fn workspace_summary(workspace: &Value) -> Option<String> {
    let repo = workspace.get("repo").and_then(|r| r.as_str())?;
    let mode = workspace.get("mode").and_then(|m| m.as_str()).unwrap_or("scratch");
    let mut line = format!("{repo} ({mode}");
    if let Some(branch) = workspace.get("branch").and_then(|b| b.as_str()) {
        line.push_str(&format!(", branch {branch}"));
    }
    if let Some(subdir) = workspace.get("subdir").and_then(|s| s.as_str()) {
        line.push_str(&format!(", subdir {subdir}"));
    }
    line.push(')');
    Some(line)
}

/// One checklist entry as this module reasons about it. The wire shape is
/// `[{item_id, text, approved, added_at}]`; only these three fields matter
/// here — the worker READS the checklist and never writes it (add/delete/
/// approve are operator-only moves).
#[derive(Debug, PartialEq)]
pub struct ChecklistItem {
    pub item_id: String,
    pub text: String,
    pub approved: bool,
}

/// Parse a task's `checklist` value. Liberal on the envelope for the same
/// reason `payload_text` is: the contract is a bare array, but a `{items: []}`
/// wrapper from an older serializer should degrade to the items, not to a
/// silently checklist-less readback. Entries without text are skipped —
/// there is nothing to echo or to owe.
fn checklist_items(checklist: &Value) -> Vec<ChecklistItem> {
    let arr = checklist
        .as_array()
        .or_else(|| checklist.get("items").and_then(|i| i.as_array()));
    let Some(arr) = arr else { return Vec::new() };
    arr.iter()
        .filter_map(|it| {
            let text = it.get("text").and_then(|t| t.as_str())?;
            Some(ChecklistItem {
                item_id: it.get("item_id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                text: text.to_string(),
                approved: it.get("approved").and_then(|a| a.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

/// The checklist as readback lines — `[done]` for items the operator already
/// approved (approvals persist across send-backs; only `[ ]` items are still
/// owed), or None when the task has no checklist. Bounded per item and in
/// total: the readback is a comprehension check, not an archive.
pub fn checklist_readback_lines(checklist: &Value) -> Option<String> {
    let items = checklist_items(checklist);
    if items.is_empty() {
        return None;
    }
    let lines: Vec<String> = items
        .iter()
        .map(|it| {
            let mark = if it.approved { "[done]" } else { "[ ]" };
            format!("{mark} {}", truncate_chars(&it.text, READBACK_CHECKLIST_ITEM_CHARS))
        })
        .collect();
    Some(truncate_chars(&lines.join("\n"), READBACK_CHECKLIST_CHARS))
}

/// The `-g{n}` msg_id suffix for a task's readback question (empty on a first
/// pickup). WHY: the backend replays a known msg_id as the ORIGINAL row —
/// including the verdict that already answered it — so a sent-back task
/// re-posting bare `readback-{task_id}` would inherit the PRE-send-back
/// approval and start work without a fresh confirm, silently defeating the
/// send-back. The server bumps `sent_back_count` atomically on every
/// send-back precisely to be this generation number: unambiguous even when
/// two consecutive send-backs carry a byte-identical note and unchanged
/// approvals (the case a content hash cannot tell apart — this replaced an
/// earlier directive-hash scheme for exactly that residue). A crash mid-wait
/// re-reads the same counter from the same task row and correctly REPLAYS its
/// own question — idempotency stays carried by the name.
fn generation_suffix(sent_back_count: u64) -> String {
    if sent_back_count == 0 {
        return String::new();
    }
    format!("-g{sent_back_count}")
}

/// The deterministic msg_id for a task's readback question — bare on a first
/// pickup (unchanged from v1), generation-suffixed after a send-back. See
/// `generation_suffix` for the WHY.
pub fn readback_msg_id(task_id: &str, sent_back_count: u64) -> String {
    format!("readback-{task_id}{}", generation_suffix(sent_back_count))
}

/// Assemble the readback text — the worker's echo of what it believes it was
/// asked to do, posted as the `question` the operator must answer before any
/// work starts.
///
/// v1 readback is an ECHO, not a model call: the title and spec restated, the
/// workspace named, and the model/endpoint this machine will drive. That is
/// deliberately cheap and deliberately literal — the gate it implements is
/// "did the right task reach the right machine with the right configuration",
/// which an echo answers exactly; "did the agent UNDERSTAND the spec" needs a
/// model in the loop and is a later phase's readback.
///
/// `checklist` is the pre-rendered lines from `checklist_readback_lines`
/// (echoed so the operator confirms which items still gate `done`), and
/// `last_directive` is the operator's own send-back note — echoed back
/// labeled as such, because on a re-readback THAT note is what the operator
/// most needs to see restated.
pub fn readback_text(
    title: &str,
    spec: &str,
    workspace: Option<&str>,
    checklist: Option<&str>,
    last_directive: Option<&str>,
    model: &str,
    endpoint_base: &str,
) -> String {
    let mut text = format!(
        "Readback — confirm before this machine starts.\n\
         Task: {title}\n\
         Spec: {}",
        truncate_chars(spec, READBACK_SPEC_CHARS)
    );
    if let Some(ws) = workspace {
        text.push_str(&format!("\nWorkspace: {ws}"));
    }
    if let Some(cl) = checklist {
        text.push_str(&format!(
            "\nDefinition of done ([done] = already approved by the operator):\n{cl}"
        ));
    }
    if let Some(d) = last_directive.filter(|d| !d.trim().is_empty()) {
        text.push_str(&format!(
            "\nOperator's note from sending this task back: {}",
            truncate_chars(d.trim(), READBACK_DIRECTIVE_CHARS)
        ));
    }
    text.push_str(&format!(
        "\nThis machine will drive {model} at {endpoint_base}.\n\
         Reply with a verdict: anything without an explicit rejection approves."
    ));
    text
}

/// The spec as `start_run_internal` receives it: the operator's spec verbatim,
/// then — only when the task carries them — a clearly delimited section with
/// the definition of done and the send-back note. The model must see all
/// three or a sent-back task re-runs the ORIGINAL ask: unapproved items are
/// the goals still owed, approved ones are named as already accepted (redoing
/// them wastes the run and can un-do accepted work), and the send-back note
/// is the priority instruction because it is the operator's freshest word.
/// Untruncated on purpose — see READBACK_CHECKLIST_CHARS for why the caps
/// belong to the readback and not here.
pub fn task_run_spec(spec: &str, checklist: &Value, last_directive: Option<&str>) -> String {
    let items = checklist_items(checklist);
    let directive = last_directive.map(str::trim).filter(|d| !d.is_empty());
    if items.is_empty() && directive.is_none() {
        return spec.to_string();
    }
    let mut out = format!(
        "{spec}\n\n\
         ==== OPERATOR CONTEXT (appended by the worker at pickup) ===="
    );
    if let Some(d) = directive {
        out.push_str(&format!(
            "\nOperator's send-back note — the PRIORITY instruction; where it \
             conflicts with the spec above, the note wins:\n{d}"
        ));
    }
    if !items.is_empty() {
        let (done, owed): (Vec<_>, Vec<_>) = items.iter().partition(|it| it.approved);
        out.push_str("\nDefinition of done — items still owed:");
        if owed.is_empty() {
            out.push_str("\n(none — every checklist item is already accepted)");
        } else {
            for it in owed {
                out.push_str(&format!("\n- {}", it.text));
            }
        }
        if !done.is_empty() {
            out.push_str(
                "\nAlready accepted by the operator — do NOT redo or rework these:",
            );
            for it in done {
                out.push_str(&format!("\n- [done] {}", it.text));
            }
        }
    }
    out
}

/// The user message a nudge becomes inside a live run. Its own message, NOT
/// appended to messages[0]: that message owns the static cache breakpoint, and
/// mutating it would re-bill the whole pinned prefix every remaining turn.
pub fn nudge_user_text(nudge: &str) -> String {
    format!(
        "Message from the human supervising this run — take it into account \
         from here on: {nudge}"
    )
}

/// Best human-readable text of an admin message's payload, for injection or
/// receipts. Liberal on purpose: the console may send `{text}`, `{message}`, or
/// something richer, and a nudge we cannot parse should still reach the model
/// as its raw JSON rather than vanish.
pub fn payload_text(payload: &Value) -> String {
    for key in ["text", "message", "note"] {
        if let Some(s) = payload.get(key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    payload.to_string()
}

/// What the receipt for a drained admin message says. `handled` is true when
/// `ask_operator` already consumed the message (a verdict answering our
/// question re-surfaces in the drain; see `ask_operator` for why).
pub fn receipt_payload(mtype: &str, handled: bool) -> Value {
    match (mtype, handled) {
        ("nudge", _) => json!({
            "disposition": "injected",
            "note": "delivered to the model at the next turn boundary",
        }),
        ("verdict", true) => json!({
            "disposition": "handled",
            "note": "consumed by the question wait it answered",
        }),
        // A verdict nothing was waiting for: the question wait ended (crash,
        // restart) before this answer arrived. The task lifecycle is the truth
        // about what happens next; the receipt only records that the row was
        // seen and is not acted on here.
        ("verdict", false) => json!({
            "disposition": "superseded",
            "note": "no question wait was pending for this verdict",
        }),
        ("goal", _) => json!({
            "disposition": "superseded",
            "note": "this worker takes work through /tasks, not goal messages",
        }),
        (_, _) => json!({
            "disposition": "noted",
        }),
    }
}

/// The pure cursor/batch bookkeeping of the turn-boundary drain, split from the
/// I/O so the crash-semantics rule is testable: `note_batch` records what was
/// FETCHED (so the next GET never refetches it), `take_advance` releases what
/// may be ACKNOWLEDGED to the server — and yields nothing until the turn that
/// consumed the batch has completed. Between the two, a crash loses only the
/// local state, the server cursor stands still, and the batch is redelivered.
#[derive(Debug, Default, PartialEq)]
pub struct DrainCursor {
    /// Highest seq fetched locally — what the next GET's since_seq should be.
    fetched_through: u64,
    /// Highest seq whose consuming turn has NOT yet completed (or whose
    /// server acknowledgement has not yet succeeded).
    pending: Option<u64>,
}

impl DrainCursor {
    pub fn starting_at(seq: u64) -> Self {
        Self { fetched_through: seq, pending: None }
    }

    pub fn since_seq(&self) -> u64 {
        self.fetched_through
    }

    /// A batch up through `max_seq` was fetched and handed to the current turn.
    /// Batches accumulate: if a previous advance is still unacknowledged, the
    /// pending mark only ever moves forward.
    pub fn note_batch(&mut self, max_seq: u64) {
        if max_seq > self.fetched_through {
            self.fetched_through = max_seq;
        }
        self.pending = Some(self.pending.map_or(max_seq, |p| p.max(max_seq)));
    }

    /// The turn that consumed the pending batch(es) completed: yield the seq to
    /// acknowledge to the server, or None if nothing is pending.
    pub fn take_advance(&mut self) -> Option<u64> {
        self.pending.take()
    }

    /// The PUT for `n` failed; keep it pending so the next completion retries.
    /// A newer batch noted meanwhile wins the max (forward-only either way).
    pub fn advance_failed(&mut self, n: u64) {
        self.pending = Some(self.pending.map_or(n, |p| p.max(n)));
    }
}

// ---- the turn-boundary drain -----------------------------------------------

/// What one boundary drain hands the agent loop: nudge texts to inject, in log
/// order.
pub struct TurnDrain {
    /// None when this machine should not drain at all: only an enrolled worker
    /// has a diary (the channel routes want its device token, and the cursor
    /// PUT accepts nothing else). An operator's local runs are steered by the
    /// person sitting at the machine, not by a log nobody writes to.
    device_id: Option<String>,
    cursor: Option<DrainCursor>,
}

impl TurnDrain {
    pub fn new(app: &AppHandle) -> Self {
        let device_id = if crate::credentials::is_enrolled(app) {
            crate::device::device_id(app).ok()
        } else {
            None
        };
        Self { device_id, cursor: None }
    }

    /// The once-per-turn GET. Returns the nudge texts to inject; receipts every
    /// drained message. NEVER fails the turn: any error here is a log line and
    /// an empty batch — the run must survive a backend outage the way it
    /// survives one in run persistence.
    ///
    /// The server cursor is NOT advanced here — see `turn_completed`.
    pub async fn at_boundary(
        &mut self,
        app: &AppHandle,
        client: &reqwest::Client,
        base: &str,
    ) -> Vec<String> {
        let Some(device_id) = self.device_id.clone() else { return Vec::new() };

        // Lazily learn where the server cursor stands. One attempt per turn:
        // until it succeeds we drain nothing, because starting from a guessed 0
        // would replay the channel's entire history into this run as if it were
        // fresh steering.
        if self.cursor.is_none() {
            match cursor_fetch(app, client, base, &device_id).await {
                Ok(seq) => self.cursor = Some(DrainCursor::starting_at(seq)),
                Err(e) => {
                    eprintln!("[channel] drain: cursor fetch failed ({e}); draining nothing this turn");
                    return Vec::new();
                }
            }
        }
        let cursor = self.cursor.as_mut().expect("initialized above");

        let batch =
            match admin_messages_since(app, client, base, &device_id, cursor.since_seq(), 0.0)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[channel] drain: fetch failed ({e}); draining nothing this turn");
                    return Vec::new();
                }
            };
        if batch.is_empty() {
            return Vec::new();
        }

        let mut nudges = Vec::new();
        let mut max_seq = cursor.since_seq();
        for msg in &batch {
            let seq = msg.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            max_seq = max_seq.max(seq);
            let msg_id = msg.get("msg_id").and_then(|m| m.as_str()).unwrap_or("");
            let mtype = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let payload = msg.get("payload").cloned().unwrap_or_else(|| json!({}));

            if mtype == "nudge" {
                nudges.push(payload_text(&payload));
            }
            let handled = mtype == "verdict" && was_verdict_handled(app, msg_id);

            // Receipt every drained message so the operator's log shows what
            // this machine DID with each instruction, in order. The msg_id is
            // derived from the received message's, so a crash-and-redeliver
            // posts the SAME receipt and the backend replays it — no dupes.
            let receipt = message_body(
                &format!("receipt-{msg_id}"),
                "receipt",
                msg.get("task_id").and_then(|t| t.as_str()),
                Some(msg_id),
                receipt_payload(mtype, handled),
            );
            if let Err(e) = post_message(app, client, base, &device_id, &receipt).await {
                // Best-effort: the receipt will be re-posted (same msg_id) when
                // the un-advanced cursor redelivers this message after a crash;
                // a transient failure here just leaves the log's receipt late.
                eprintln!("[channel] drain: receipt for {msg_id} failed: {e}");
            }
        }
        cursor.note_batch(max_seq);
        nudges
    }

    /// The turn that consumed the last batch has completed (its model response
    /// fully landed): NOW acknowledge consumption to the server. Advancing any
    /// earlier would let a crash mid-turn drop messages the model never saw;
    /// advancing any later (the next boundary) would leave the final turn's
    /// batch permanently unacknowledged on every clean run end.
    pub async fn turn_completed(
        &mut self,
        app: &AppHandle,
        client: &reqwest::Client,
        base: &str,
    ) {
        let Some(device_id) = self.device_id.clone() else { return };
        let Some(cursor) = self.cursor.as_mut() else { return };
        let Some(n) = cursor.take_advance() else { return };
        if let Err(e) = cursor_advance(app, client, base, &device_id, n).await {
            eprintln!("[channel] cursor advance to {n} failed ({e}); will retry next turn");
            cursor.advance_failed(n);
        }
    }
}

// ---- ask_operator ----------------------------------------------------------

/// Post a `question` and block until a `verdict` answers it. Returns the
/// verdict message, or None only when `cancel` fires.
///
/// The wait retries FOREVER with backoff. This is the one place a "give up"
/// path would be a correctness bug rather than a mercy: the worker is blocked
/// BECAUSE a human must decide, and a backend blip that silently unblocked it
/// would convert "waiting for permission" into "acting without it".
///
/// The wait is a read-only OBSERVER of the log: it scans with its own local
/// since_seq (starting at the question's own seq — a verdict is always appended
/// after the question it answers) and never advances the shared cursor. Admin
/// messages that arrive during the wait (a nudge typed while deciding) are
/// deliberately left unconsumed for the next run's boundary drain, which is the
/// component that can actually act on them; consuming them here would receipt
/// steering into the void. The verdict row itself therefore ALSO resurfaces in
/// a later drain — `mark_verdict_handled` is how that drain knows to receipt it
/// as handled rather than as a stray.
async fn ask_operator(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    question_msg_id: &str,
    task_id: &str,
    payload: Value,
    cancel: &CancellationToken,
) -> Option<Value> {
    let body = message_body(question_msg_id, "question", Some(task_id), None, payload);
    let ack = post_message_retrying(app, client, base, device_id, &body, cancel).await?;
    // Replay-safe by construction: a deterministic msg_id means a worker that
    // crashed after asking re-posts the SAME question and gets the ORIGINAL row
    // (original seq) back — and then finds the verdict that may already have
    // answered it while it was down.
    let mut since = ack.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);

    let mut backoff = BACKOFF_START;
    loop {
        let batch = tokio::select! {
            _ = cancel.cancelled() => return None,
            r = admin_messages_since(app, client, base, device_id, since, VERDICT_WAIT_SECS) => r,
        };
        match batch {
            Ok(messages) => {
                backoff = BACKOFF_START;
                for msg in messages {
                    if let Some(seq) = msg.get("seq").and_then(|s| s.as_u64()) {
                        since = since.max(seq);
                    }
                    if msg.get("type").and_then(|t| t.as_str()) == Some("verdict")
                        && msg.get("in_reply_to").and_then(|r| r.as_str())
                            == Some(question_msg_id)
                    {
                        if let Some(id) = msg.get("msg_id").and_then(|m| m.as_str()) {
                            mark_verdict_handled(app, id);
                        }
                        return Some(msg);
                    }
                }
            }
            Err(e) => {
                // A network failure while blocked is NOT an answer. Back off
                // and re-ask; the question stands until a human settles it.
                eprintln!("[channel] verdict wait failed ({e}); retrying in {}s", backoff.as_secs());
                tokio::select! {
                    _ = cancel.cancelled() => return None,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

// ---- task lifecycle helpers ------------------------------------------------

/// PATCH a task's status with bounded retries. Transport errors retry (a blip
/// must not strand the lifecycle); HTTP errors do NOT (a 409/403 is the
/// transition table speaking, and re-sending the same illegal move never helps).
async fn patch_task_status(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    task_id: &str,
    status: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let url = format!("{base}/tasks/{task_id}");
    let body = json!({ "status": status });
    let mut backoff = BACKOFF_START;
    for attempt in 0..=PATCH_RETRIES {
        match with_bearer(app, client.patch(&url).json(&body), "").send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                let status_code = r.status();
                let txt = r.text().await.unwrap_or_default();
                let txt: String = txt.chars().take(300).collect();
                return Err(format!("HTTP {status_code}: {txt}"));
            }
            Err(e) if attempt < PATCH_RETRIES => {
                eprintln!("[tasks] PATCH {task_id} -> {status} failed ({e}); retrying");
                tokio::select! {
                    _ = cancel.cancelled() => return Err("cancelled".to_string()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(e) => return Err(format!("request failed: {e}")),
        }
    }
    unreachable!("loop returns on every path")
}

/// `GET /tasks/next` — this machine's oldest queued task, or None. An empty
/// queue is a 200 with `task: null`, so any error here is a real one.
async fn fetch_next_task(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
) -> Result<Option<(Value, (Option<String>, Option<String>))>, String> {
    let url = format!("{base}/tasks/next?device_id={}", urlencoding::encode(device_id));
    let resp = with_bearer(app, client.get(&url), "")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let txt: String = txt.chars().take(300).collect();
        return Err(format!("HTTP {status}: {txt}"));
    }
    let body: Value = resp.json().await.map_err(|e| format!("bad body: {e}"))?;
    // The fleet's model endpoint rides the pickup response, resolved by the
    // backend at this moment exactly as /agent/dispatch resolves it onto a run
    // frame. Without it, a freshly started worker that has never received a
    // dispatch frame has no fleet value at all: its first task read back
    // "claude-opus-4-8 at api.anthropic.com" and the guard stood ready to
    // refuse the run — the first live readback caught precisely that.
    let pick = |k: &str| body.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    let fleet = (pick("model_endpoint"), pick("model"));
    match body.get("task") {
        Some(Value::Null) | None => Ok(None),
        Some(task) => Ok(Some((task.clone(), fleet))),
    }
}

/// `POST /runs` — mint the run row for a task run. Every other run path has the
/// row minted for it (the frontend for local runs, the backend for dispatched
/// ones); a task run is the one kind born in Rust, so it mints its own here.
/// Bounded retries: without a run row the console cannot join task → run, and
/// starting invisible work on an unattended machine is worse than telling the
/// operator the pickup failed.
async fn create_run_row(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    task_title: &str,
    model: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let url = format!("{base}/runs");
    let body = json!({ "task": task_title, "model": model });
    let mut backoff = BACKOFF_START;
    for attempt in 0..=PATCH_RETRIES {
        match with_bearer(app, client.post(&url).json(&body), "").send().await {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.map_err(|e| format!("bad body: {e}"))?;
                return v
                    .get("run_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| "POST /runs returned no run_id".to_string());
            }
            Ok(r) => {
                let status = r.status();
                let txt = r.text().await.unwrap_or_default();
                let txt: String = txt.chars().take(300).collect();
                return Err(format!("HTTP {status}: {txt}"));
            }
            Err(e) if attempt < PATCH_RETRIES => {
                eprintln!("[tasks] POST /runs failed ({e}); retrying");
                tokio::select! {
                    _ = cancel.cancelled() => return Err("cancelled".to_string()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(e) => return Err(format!("request failed: {e}")),
        }
    }
    unreachable!("loop returns on every path")
}

/// Post a `status` entry, retrying until it lands or `cancel` fires. Status
/// messages here are the join keys the console leans on (task → run, task →
/// rejection), so they get the retry-forever treatment a bare receipt does not
/// — deterministic msg_ids keep the retries harmless.
async fn post_status(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    msg_id: &str,
    task_id: &str,
    payload: Value,
    cancel: &CancellationToken,
) {
    let body = message_body(msg_id, "status", Some(task_id), None, payload);
    let _ = post_message_retrying(app, client, base, device_id, &body, cancel).await;
}

// ---- the idle pickup loop --------------------------------------------------

/// Whether a run currently holds the AgentState (i.e. a live, non-cancelled
/// token is installed). The same test `start_run_internal` applies.
fn run_in_flight(app: &AppHandle) -> bool {
    app.try_state::<crate::agent::AgentState>()
        .and_then(|s| s.0.lock().ok().map(|g| g.as_ref().map_or(false, |t| !t.is_cancelled())))
        .unwrap_or(false)
}

/// The worker's idle loop: poll `/tasks/next` every `TASK_POLL_INTERVAL` (or
/// immediately on a doorbell ring), and walk each task through
/// readback → verdict → run → awaiting_verdict.
///
/// Spawned once at app setup and alive for the life of the app, but it POLLS
/// only while this machine is an enrolled worker with no run in flight — an
/// operator's Mac reaches the `is_enrolled` check, fails it, and goes back to
/// sleep without ever touching `/tasks`. Gating per-iteration rather than at
/// spawn time means a machine enrolled mid-session starts pulling work without
/// a restart, and one un-enrolled mid-session stops.
pub async fn task_pickup_loop(app: AppHandle) {
    let client = reqwest::Client::new();
    let base = backend_url();
    let shutdown = app.state::<ChannelState>().shutdown.clone();

    loop {
        if shutdown.is_cancelled() {
            return;
        }
        if crate::credentials::is_enrolled(&app) && !run_in_flight(&app) {
            if let Ok(device_id) = crate::device::device_id(&app) {
                match fetch_next_task(&app, &client, &base, &device_id).await {
                    Ok(Some((task, fleet))) => {
                        handle_task(&app, &client, &base, &device_id, &task, fleet, &shutdown).await;
                        // Re-poll immediately: finishing one task is the moment
                        // the queue most likely holds the next.
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[tasks] /tasks/next failed: {e}"),
                }
            }
        }
        let state = app.state::<ChannelState>();
        state.doorbell.wait(TASK_POLL_INTERVAL, &shutdown).await;
    }
}

/// Walk one task through its device-legal lifecycle. Every early return leaves
/// the task in a state the OPERATOR resolves — killing and abandoning are not
/// this device's transitions to make (the backend's `_DEVICE_LEGAL` would
/// refuse them anyway), so on any failure the honest move is a `status` message
/// saying what happened and a task left standing where it stood.
async fn handle_task(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    device_id: &str,
    task: &Value,
    fleet_from_pickup: (Option<String>, Option<String>),
    cancel: &CancellationToken,
) {
    let task_id = task.get("task_id").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let title = task.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let spec = task.get("spec").and_then(|s| s.as_str()).unwrap_or("").to_string();
    if task_id.is_empty() || spec.is_empty() {
        eprintln!("[tasks] task missing task_id/spec; skipping");
        return;
    }
    eprintln!("[tasks] picked up task {task_id} ({title:?})");

    // queued -> readback. A failure here (e.g. the operator killed the task
    // between our GET and now — a 409) means the task is no longer ours to
    // move; drop it and let the next poll see the world as it is.
    if let Err(e) = patch_task_status(app, client, base, &task_id, "readback", cancel).await {
        eprintln!("[tasks] {task_id}: queued->readback failed: {e}");
        return;
    }

    // The task's definition-of-done and send-back note. Absent fields read as
    // "none" — a task minted before the checklist layer behaves exactly as it
    // did before it.
    let checklist = task.get("checklist").cloned().unwrap_or(Value::Null);
    let last_directive = task
        .get("last_directive")
        .and_then(|d| d.as_str())
        .map(str::to_string);

    // The readback question. Deterministic msg_id: a worker that crashes
    // mid-wait re-posts the same question and inherits its original seq — and
    // any verdict already given. A SENT-BACK task gets a generation-suffixed
    // id so the re-confirm is a FRESH question rather than an idempotent
    // replay of the already-approved one (see `generation_suffix`).
    // What this run WILL drive, promised honestly: the pickup's fleet values
    // win; the display resolver (last dispatch frame > env > default) is only
    // the fallback. The same pair is handed to start_run_internal below, so the
    // readback can never promise one endpoint and the run drive another.
    let ep = crate::agent::endpoint_for_display();
    let (fleet_base, fleet_model) = fleet_from_pickup;
    let promised_base = fleet_base.clone().unwrap_or_else(|| ep.base.clone());
    let promised_model = fleet_model.clone().unwrap_or_else(|| ep.model.clone());
    let ws = task.get("workspace").map(workspace_summary).unwrap_or(None);
    let cl_lines = checklist_readback_lines(&checklist);
    let text = readback_text(
        &title,
        &spec,
        ws.as_deref(),
        cl_lines.as_deref(),
        last_directive.as_deref(),
        &promised_model,
        &promised_base,
    );
    let sent_back_count = task
        .get("sent_back_count")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    let question_id = readback_msg_id(&task_id, sent_back_count);
    let Some(verdict) = ask_operator(
        app,
        client,
        base,
        device_id,
        &question_id,
        &task_id,
        json!({ "text": text }),
        cancel,
    )
    .await
    else {
        return; // shutting down; the question stands for the next start
    };

    let payload = verdict.get("payload").cloned().unwrap_or_else(|| json!({}));
    if !verdict_approves(&payload) {
        // NOT ours to abandon: `abandoned`/`killed` are operator-only
        // transitions (the whole point of the verdict layer is that judgments
        // about the work belong to the human). Record the rejection in the log
        // and leave the task in `readback`, where the operator's console shows
        // it and the operator decides its fate.
        eprintln!("[tasks] {task_id}: readback rejected; leaving task in readback");
        // Same generation suffix as the question it reports on: rejecting a
        // RE-readback must not idempotently replay the first rejection row.
        post_status(
            app, client, base, device_id,
            &format!("rejected-{task_id}{}", generation_suffix(sent_back_count)),
            &task_id,
            json!({
                "text": "readback rejected by operator; task left in readback for the operator to resolve",
                "verdict_msg_id": verdict.get("msg_id").and_then(|m| m.as_str()).unwrap_or(""),
            }),
            cancel,
        )
        .await;
        return;
    }

    // Approved: readback -> running, then start the work through the SAME path
    // a dispatched run takes.
    if let Err(e) = patch_task_status(app, client, base, &task_id, "running", cancel).await {
        eprintln!("[tasks] {task_id}: readback->running failed: {e}");
        return;
    }
    let run_id = match create_run_row(app, client, base, &title, &promised_model, cancel).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[tasks] {task_id}: POST /runs failed: {e}");
            post_status(
                app, client, base, device_id,
                &format!("runfail-{task_id}"),
                &task_id,
                json!({ "text": format!("could not create a run for this task: {e}; task left in running") }),
                cancel,
            )
            .await;
            return;
        }
    };

    // The fleet endpoint resolution is UNCHANGED from a dispatched run: pass
    // fleet values in the frame position and let `resolve_endpoint`'s
    // frame > env > default ladder decide. The values come from the PICKUP
    // response (resolved server-side at claim time), falling back to the last
    // dispatch frame's memory — so a task run drives exactly what the readback
    // promised.
    let fleet = crate::agent::last_fleet_endpoint();
    let frame_base = fleet_base.or_else(|| fleet.as_ref().map(|(b, _)| b.clone()));
    let frame_model = fleet_model.or_else(|| fleet.as_ref().and_then(|(_, m)| m.clone()));
    // The spec the model runs on carries the definition of done and the
    // send-back note — the readback promised them, the run must honor them.
    let run_spec = task_run_spec(&spec, &checklist, last_directive.as_deref());
    let agent_state = app.state::<crate::agent::AgentState>();
    let started = crate::agent::start_run_internal(
        app,
        &agent_state,
        run_spec,
        String::new(), // no session token; the device credential rides via with_bearer
        Vec::new(),
        run_id.clone(),
        frame_model,
        frame_base,
        base.to_string(),
    );
    let run_token = match started {
        Ok(()) => app
            .state::<crate::agent::AgentState>()
            .0
            .lock()
            .ok()
            .and_then(|g| g.clone()),
        Err(e) => {
            // Lost a race with a dispatched run that started between our idle
            // check and here. The task stays `running` with no run attached;
            // say so rather than fight over the machine.
            eprintln!("[tasks] {task_id}: start_run_internal refused: {e}");
            post_status(
                app, client, base, device_id,
                &format!("runfail-{task_id}"),
                &task_id,
                json!({ "text": format!("could not start the run: {e}; task left in running") }),
                cancel,
            )
            .await;
            return;
        }
    };

    // Wait for the run to end, however it ends: RunLease cancels the token on
    // EVERY exit path (completion, failure, even panic), so this wait cannot
    // hang on a run that died badly.
    if let Some(token) = run_token {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = token.cancelled() => {}
        }
    }

    // `note_run_outcome` ran inside runs_finalize, strictly before the lease
    // drop that woke us — so a missing outcome means the run died WITHOUT
    // finalizing (a panic), which is a failure by any honest reading.
    let outcome = take_outcome_for(app, &run_id);
    let (status, error) = match &outcome {
        Some(o) => (o.status.as_str(), o.error.clone()),
        None => ("failed", Some("run ended without reporting an outcome".to_string())),
    };

    if status == "completed" {
        // running -> awaiting_verdict: the device's last legal move. `done` is
        // the operator's judgment of the WORK; this only reports that the
        // worker stopped working.
        if let Err(e) =
            patch_task_status(app, client, base, &task_id, "awaiting_verdict", cancel).await
        {
            eprintln!("[tasks] {task_id}: running->awaiting_verdict failed: {e}");
        }
        post_status(
            app, client, base, device_id,
            &format!("outcome-{run_id}"),
            &task_id,
            json!({ "run_id": run_id, "outcome": "completed" }),
            cancel,
        )
        .await;
    } else {
        // Failed or cancelled: the task is NOT ours to kill. Leave it in
        // `running` — visibly wrong on the console, which is the point — and
        // put the error in the log so the operator can kill or re-run it.
        post_status(
            app, client, base, device_id,
            &format!("outcome-{run_id}"),
            &task_id,
            json!({
                "run_id": run_id,
                "outcome": status,
                "error": error.unwrap_or_default(),
                "text": "run did not complete; task left in running for the operator",
            }),
            cancel,
        )
        .await;
    }
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the approval rule -------------------------------------------------

    /// The liberal default: anything that is not an explicit rejection — an
    /// empty payload included — approves. An unattended fleet stalls on
    /// false-rejects; false-approves start work the operator is watching.
    #[test]
    fn verdict_approves_unless_explicitly_rejected() {
        for payload in [
            json!({}),
            json!({"decision": "approved"}),
            json!({"decision": "yes"}),
            json!({"text": "looks good, go ahead"}),
            json!({"text": "sure"}),
            json!({"answer": "LGTM"}),
            json!({"approved": true}),
            // "no" only rejects ALONE — leading "no" in a longer approval
            // ("no problem...") must not stall the machine.
            json!({"text": "no problem, go ahead"}),
            // Rejection words NOT in first position don't reject: "I won't
            // reject this" is an approval that mentions the word.
            json!({"text": "nothing to reject here, proceed"}),
        ] {
            assert!(verdict_approves(&payload), "should approve: {payload}");
        }
    }

    #[test]
    fn verdict_rejects_on_explicit_markers() {
        for payload in [
            json!({"decision": "rejected"}),
            json!({"decision": "REJECT"}),
            json!({"text": "no"}),
            json!({"text": "  No.  "}),
            json!({"text": "denied, wrong machine"}),
            json!({"text": "stop! that spec is stale"}),
            json!({"text": "abort"}),
            json!({"approved": false}),
        ] {
            assert!(!verdict_approves(&payload), "should reject: {payload}");
        }
    }

    /// The boolean, when present, is the most explicit signal and wins over
    /// any free text beside it.
    #[test]
    fn verdict_boolean_beats_text() {
        assert!(!verdict_approves(&json!({"approved": false, "text": "yes"})));
        assert!(verdict_approves(&json!({"approved": true, "text": "no"})));
    }

    // ---- readback assembly -------------------------------------------------

    #[test]
    fn readback_echoes_task_and_endpoint() {
        let text = readback_text(
            "Migrate CI to uv",
            "Move the pipeline off pip.",
            Some("github.com/x/y (scratch, branch main)"),
            None,
            None,
            "qwen-vl",
            "http://self-hosted:8080",
        );
        assert!(text.contains("Migrate CI to uv"));
        assert!(text.contains("Move the pipeline off pip."));
        assert!(text.contains("github.com/x/y (scratch, branch main)"));
        assert!(text.contains("qwen-vl"));
        assert!(text.contains("http://self-hosted:8080"));
    }

    /// The spec is truncated on a CHAR boundary (byte slicing panics mid-UTF-8)
    /// and stays under the readback budget — the channel payload cap is 16KB
    /// and the spec alone can be 20KB.
    #[test]
    fn readback_truncates_a_long_spec() {
        let spec = "é".repeat(READBACK_SPEC_CHARS + 500);
        let text = readback_text("t", &spec, None, None, None, "m", "http://e");
        assert!(text.contains('…'), "truncation is marked");
        assert!(text.chars().count() < READBACK_SPEC_CHARS + 300);
    }

    #[test]
    fn readback_omits_a_missing_workspace() {
        let text = readback_text("t", "s", None, None, None, "m", "http://e");
        assert!(!text.contains("Workspace:"));
        assert!(!text.contains("Definition of done"));
        assert!(!text.contains("sending this task back"));
    }

    /// The readback echoes the checklist with approvals marked and the
    /// send-back note labeled as the operator's own words — the two things a
    /// re-confirm exists to restate.
    #[test]
    fn readback_carries_checklist_and_sendback_note() {
        let checklist = json!([
            {"item_id": "a", "text": "tests pass", "approved": true, "added_at": "2026-09-01T00:00:00Z"},
            {"item_id": "b", "text": "docs updated", "approved": false, "added_at": "2026-09-01T00:00:00Z"},
        ]);
        let lines = checklist_readback_lines(&checklist).unwrap();
        let text = readback_text(
            "t", "s", None,
            Some(&lines),
            Some("the docs section is still missing"),
            "m", "http://e",
        );
        assert!(text.contains("[done] tests pass"));
        assert!(text.contains("[ ] docs updated"));
        assert!(text.contains(
            "Operator's note from sending this task back: the docs section is still missing"
        ));
    }

    #[test]
    fn checklist_lines_mark_approvals_and_stay_bounded() {
        assert_eq!(checklist_readback_lines(&Value::Null), None);
        assert_eq!(checklist_readback_lines(&json!([])), None);
        // The {items: []} envelope degrades to its items rather than to
        // nothing.
        let wrapped = json!({"items": [{"item_id": "a", "text": "x", "approved": false}]});
        assert_eq!(checklist_readback_lines(&wrapped).as_deref(), Some("[ ] x"));
        // Per-item and total truncation both hold on a char boundary.
        let big: Vec<Value> = (0..20)
            .map(|i| json!({"item_id": format!("i{i}"), "text": "é".repeat(400), "approved": false}))
            .collect();
        let lines = checklist_readback_lines(&json!(big)).unwrap();
        assert!(lines.chars().count() <= READBACK_CHECKLIST_CHARS + 1);
        assert!(lines.contains('…'));
    }

    // ---- the run prompt ----------------------------------------------------

    /// A task with no checklist and no directive runs on the spec VERBATIM —
    /// byte-identical, so pre-checklist tasks behave exactly as before.
    #[test]
    fn run_spec_is_verbatim_without_operator_context() {
        assert_eq!(task_run_spec("do the thing", &Value::Null, None), "do the thing");
        assert_eq!(task_run_spec("do the thing", &json!([]), Some("  ")), "do the thing");
    }

    /// The appended section splits the checklist by approval — unapproved
    /// items are the goals still owed, approved ones are named as accepted and
    /// not-to-redo — and carries the send-back note as the priority word.
    #[test]
    fn run_spec_appends_definition_of_done_and_note() {
        let checklist = json!([
            {"item_id": "a", "text": "tests pass", "approved": true},
            {"item_id": "b", "text": "docs updated", "approved": false},
        ]);
        let out = task_run_spec("the spec", &checklist, Some("focus on the docs"));
        assert!(out.starts_with("the spec\n\n"), "the spec leads, untouched");
        assert!(out.contains("==== OPERATOR CONTEXT"));
        assert!(out.contains("send-back note — the PRIORITY instruction"));
        assert!(out.contains("focus on the docs"));
        assert!(out.contains("items still owed:\n- docs updated"));
        assert!(out.contains("do NOT redo or rework these:\n- [done] tests pass"));
    }

    /// All items approved: say so explicitly rather than print an empty owed
    /// list the model could misread as "nothing was defined".
    #[test]
    fn run_spec_names_a_fully_approved_checklist() {
        let checklist = json!([{"item_id": "a", "text": "tests pass", "approved": true}]);
        let out = task_run_spec("s", &checklist, None);
        assert!(out.contains("(none — every checklist item is already accepted)"));
        assert!(out.contains("- [done] tests pass"));
        assert!(!out.contains("PRIORITY"), "no note, no note section");
    }

    // ---- the re-readback msg_id --------------------------------------------

    /// First pickup keeps the v1 id (crash-replay still finds the original
    /// question); a send-back mints a NEW deterministic id — otherwise the
    /// backend's idempotent replay hands back the pre-send-back verdict and
    /// the re-confirm silently never happens. The generation number is the
    /// server's sent_back_count, bumped atomically with the send-back edge, so
    /// even two send-backs with byte-identical notes and unchanged approvals
    /// are distinct questions — the case a content hash could not tell apart.
    #[test]
    fn readback_msg_id_regenerates_on_a_send_back() {
        let first = readback_msg_id("t1", 0);
        assert_eq!(first, "readback-t1");

        let sent_back = readback_msg_id("t1", 1);
        assert_ne!(sent_back, first);
        assert_eq!(sent_back, "readback-t1-g1");
        // Deterministic: a crash mid-wait re-reads the same counter off the
        // task row and replays its own question instead of minting a duplicate.
        assert_eq!(sent_back, readback_msg_id("t1", 1));
        // Every further send-back is a new question, even with an identical
        // note and identical approvals.
        assert_ne!(sent_back, readback_msg_id("t1", 2));
    }

    #[test]
    fn workspace_summary_reads_the_task_shape() {
        let ws = json!({"repo": "github.com/x/y", "mode": "existing", "branch": "main"});
        assert_eq!(
            workspace_summary(&ws).as_deref(),
            Some("github.com/x/y (existing, branch main)")
        );
        assert_eq!(workspace_summary(&json!({})), None);
    }

    // ---- the drain's cursor rule -------------------------------------------

    /// The load-bearing timing: a fetched batch is NOT acknowledgeable until
    /// the turn that consumed it completes. Between note_batch and
    /// take_advance a crash redelivers — msg_id idempotency makes that safe;
    /// advancing early would make it lossy.
    #[test]
    fn drain_cursor_holds_the_advance_until_the_turn_completes() {
        let mut c = DrainCursor::starting_at(10);
        assert_eq!(c.since_seq(), 10);
        assert_eq!(c.take_advance(), None, "nothing consumed, nothing to ack");

        c.note_batch(14);
        // The next GET must not refetch what this turn already holds...
        assert_eq!(c.since_seq(), 14);
        // ...and after the turn completes, exactly that batch is acknowledged,
        // exactly once.
        assert_eq!(c.take_advance(), Some(14));
        assert_eq!(c.take_advance(), None, "an ack is not repeated");
    }

    /// A failed PUT keeps the batch pending; a batch fetched meanwhile folds
    /// into one forward-only acknowledgement.
    #[test]
    fn drain_cursor_retries_a_failed_advance_and_batches_forward() {
        let mut c = DrainCursor::starting_at(0);
        c.note_batch(5);
        let n = c.take_advance().unwrap();
        c.advance_failed(n); // the PUT for 5 failed
        c.note_batch(9); // next boundary fetched more
        assert_eq!(c.take_advance(), Some(9), "one ack covers both batches");
        assert_eq!(c.since_seq(), 9);
    }

    /// An out-of-order (stale) batch max can never move anything backwards —
    /// seq is the backend's only ordering authority and the cursor mirrors it.
    #[test]
    fn drain_cursor_never_rewinds() {
        let mut c = DrainCursor::starting_at(20);
        c.note_batch(7);
        assert_eq!(c.since_seq(), 20, "fetch frontier holds");
        // The pending mark exists (the batch WAS handed to a turn) but the
        // eventual PUT of 7 is one the server's forward-only rule absorbs.
        assert_eq!(c.take_advance(), Some(7));
    }

    // ---- doorbell debounce -------------------------------------------------

    /// Any number of rings between two waits collapse into ONE pending wake:
    /// the doorbell says "you have mail", never how much.
    #[test]
    fn doorbell_debounces_a_burst_of_rings() {
        let bell = Doorbell::new();
        bell.ring();
        bell.ring();
        bell.ring();
        assert!(bell.take_pending(), "one wake for the burst");
        assert!(!bell.take_pending(), "and only one");
    }

    /// A ring while the poller is busy latches: the NEXT wait returns
    /// immediately instead of losing the wake.
    #[test]
    fn doorbell_latches_while_nobody_waits() {
        let bell = Doorbell::new();
        assert!(!bell.take_pending(), "quiet bell has nothing pending");
        bell.ring();
        assert!(bell.take_pending(), "a ring during busy-time survives");
    }

    // ---- message assembly --------------------------------------------------

    /// A question must carry requires_reply (the backend 422s otherwise); no
    /// other type may.
    #[test]
    fn message_body_derives_requires_reply_from_the_type() {
        let q = message_body("m1", "question", Some("t1"), None, json!({}));
        assert_eq!(q["requires_reply"], json!(true));
        assert_eq!(q["task_id"], json!("t1"));
        let s = message_body("m2", "status", None, None, json!({}));
        assert_eq!(s["requires_reply"], json!(false));
        assert!(s.get("task_id").is_none());
        assert!(s.get("in_reply_to").is_none());
    }

    #[test]
    fn receipt_payloads_say_what_happened() {
        assert_eq!(receipt_payload("nudge", false)["disposition"], json!("injected"));
        assert_eq!(receipt_payload("verdict", true)["disposition"], json!("handled"));
        assert_eq!(receipt_payload("verdict", false)["disposition"], json!("superseded"));
        assert_eq!(receipt_payload("goal", false)["disposition"], json!("superseded"));
        assert_eq!(receipt_payload("status", false)["disposition"], json!("noted"));
    }

    /// Payload text extraction is liberal: known keys first, raw JSON as the
    /// floor — a nudge we cannot parse still reaches the model.
    #[test]
    fn payload_text_falls_back_to_raw_json() {
        assert_eq!(payload_text(&json!({"text": "focus on the form"})), "focus on the form");
        assert_eq!(payload_text(&json!({"message": "hi"})), "hi");
        let odd = json!({"steer": {"x": 1}});
        assert_eq!(payload_text(&odd), odd.to_string());
    }

    #[test]
    fn nudge_wrapper_names_the_human() {
        let s = nudge_user_text("use the staging site");
        assert!(s.contains("human supervising this run"));
        assert!(s.ends_with("use the staging site"));
    }
}
