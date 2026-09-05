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

/// Bounds for the done-claim status message, same 16KB payload cap. The claim
/// is the ONE message the operator reads before judging, so it is bounded per
/// field rather than truncated as a blob: a claim whose last item vanished into
/// an ellipsis is worse than one whose notes are clipped. Worst case at the
/// item ceiling is 25 × (160 + 200 + ~120) ≈ 12KB plus the summary — inside the
/// cap with room for the envelope.
const CLAIM_MAX_ITEMS: usize = 25;
const CLAIM_ITEM_TEXT_CHARS: usize = 160;
const CLAIM_NOTE_CHARS: usize = 200;
const CLAIM_SUMMARY_CHARS: usize = 600;

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
    /// The checklist the run ABOUT TO START is answerable for, parked here by
    /// `handle_task` and CONSUMED by `run_agent` (`take_run_checklist`). A
    /// hand-off slot rather than a live field on purpose: taking it means a
    /// local or dispatched run that starts later can never inherit a previous
    /// task's items and offer the model a `claim_done` about work it was never
    /// given. Empty for every run that is not a checklisted task's run.
    pending_checklist: Mutex<Option<Vec<ChecklistItem>>>,
    /// The done-claim the model made during the current run, recorded by
    /// agent.rs's dispatch (which cannot reach the network from inside the
    /// Computer-state scope) and drained by `handle_task` when the run ends.
    /// One slot for the same reason `outcome` is one slot; a second
    /// `claim_done` call in the same run overwrites — the last claim the model
    /// stood behind is the one the operator should judge.
    claim: Mutex<Option<(String, DoneClaim)>>,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self {
            doorbell: Doorbell::new(),
            shutdown: CancellationToken::new(),
            outcome: Mutex::new(None),
            handled_verdicts: Mutex::new(HashSet::new()),
            pending_checklist: Mutex::new(None),
            claim: Mutex::new(None),
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

/// Park the checklist the run about to start is answerable for. Called by
/// `handle_task` immediately before `start_run_internal`; see
/// `ChannelState::pending_checklist` for why it is a hand-off and not a field.
fn set_pending_checklist(app: &AppHandle, items: Vec<ChecklistItem>) {
    if let Some(state) = app.try_state::<ChannelState>() {
        if let Ok(mut g) = state.pending_checklist.lock() {
            *g = Some(items);
        }
    }
}

/// Take the parked checklist, if any. `run_agent` calls this ONCE at run start:
/// a non-empty result means this run belongs to a checklisted task and the
/// `claim_done` tool applies to it; an empty one means the model gets no such
/// tool and nothing in its prompt about claiming (a local run, a dispatched
/// run, or a task whose every item the operator already accepted).
pub(crate) fn take_run_checklist(app: &AppHandle) -> Vec<ChecklistItem> {
    app.try_state::<ChannelState>()
        .and_then(|s| s.pending_checklist.lock().ok().and_then(|mut g| g.take()))
        .unwrap_or_default()
}

/// Record the model's done-claim for `run_id`. Called from agent.rs's tool
/// dispatch, which is synchronous by construction (it holds the Computer state
/// mutex and may not await), so the claim is stashed here and posted to the
/// diary later by `handle_task` — in the same breath as the move to
/// `awaiting_verdict`, which is the moment the operator starts judging.
pub(crate) fn note_run_claim(app: &AppHandle, run_id: &str, claim: DoneClaim) {
    if let Some(state) = app.try_state::<ChannelState>() {
        if let Ok(mut g) = state.claim.lock() {
            *g = Some((run_id.to_string(), claim));
        }
    }
}

/// The claim made during `run_id`, or None. Run-id-matched for the same reason
/// `take_outcome_for` is: a claim left behind by an earlier run must never be
/// attributed to this one.
fn take_claim_for(app: &AppHandle, run_id: &str) -> Option<DoneClaim> {
    let state = app.try_state::<ChannelState>()?;
    let mut g = state.claim.lock().ok()?;
    match g.as_ref() {
        Some((id, _)) if id == run_id => g.take().map(|(_, c)| c),
        _ => None,
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
#[derive(Clone, Debug, PartialEq)]
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

// ---- the done claim --------------------------------------------------------
//
// The gap this closes, observed on the first real two-item run: the model
// finished by narrating "Both windows are now visible side by side: Notepad
// shows … and Calculator shows 42" — one prose blob, for a task that carried
// two SEPARATE, typed checklist items and fourteen uploaded frames. Nothing in
// that sentence said which item the model believed it had satisfied, or which
// frame proved it, so the operator had to read every thumbnail and re-derive
// the mapping by hand. Having a structured checklist and then throwing the
// structure away at the one moment it is worth something is the whole bug.
//
// What follows is deliberately an ASSERTION channel, not an approval one. The
// backend already 403s a device on `done` and on item approval; nothing here
// tries to route around that, and the wording — in the tool description, in the
// run prompt, and in the posted payload's own `text` — keeps saying "claims"
// so a false claim reads as a false claim rather than as a fact.

/// One item's claim as the worker records it: the model's assertion about one
/// checklist item, plus the checklist text it refers to (denormalized so the
/// operator's console can render the claim without joining back to the task)
/// and the frame the model says proves it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedItem {
    pub item_id: String,
    pub text: String,
    pub satisfied: bool,
    pub evidence_note: String,
    /// The `seq` of a screenshot event in THIS run, or None. Optional on
    /// purpose: "the file is on disk in the folder I opened" is a legitimate
    /// item with no single frame behind it, and forcing a number would only
    /// teach the model to invent one.
    pub frame_seq: Option<i64>,
}

/// A whole `claim_done` call, validated against the task's real checklist.
#[derive(Clone, Debug, PartialEq)]
pub struct DoneClaim {
    pub items: Vec<ClaimedItem>,
    pub summary: String,
}

impl DoneClaim {
    fn satisfied_count(&self) -> usize {
        self.items.iter().filter(|c| c.satisfied).count()
    }
}

/// Items the run still owes — the ones a claim is actually about. Approvals
/// persist across send-backs, so an item the operator already accepted is not
/// this run's to claim (and redoing it is explicitly discouraged elsewhere in
/// the run spec).
fn owed_items(items: &[ChecklistItem]) -> Vec<&ChecklistItem> {
    items.iter().filter(|it| !it.approved).collect()
}

/// Whether a run with this checklist gets the `claim_done` tool at all: only
/// when something is still owed. One predicate, used by BOTH the tool
/// declaration and the run-prompt contract, so the model can never be told to
/// call a tool it was not given (or given one nothing in its prompt explains).
pub fn claim_applies(items: &[ChecklistItem]) -> bool {
    items.iter().any(|it| !it.approved)
}

/// Resolve a model-supplied `item_id` against the task's real checklist.
///
/// Exact match first. Failing that, a case-insensitive PREFIX of at least 8
/// characters that matches exactly one item — because the real ids are UUIDs
/// (`e4777019-5f63-4776-a800-577c965bb88c`), and asking a 8B-class self-hosted
/// model to transcribe 36 characters without a slip is asking for a rejected
/// claim at the one moment in the run where a rejection costs the most. A
/// prefix that matches two items is NOT resolved: guessing between them would
/// attach the model's evidence to the wrong criterion, which is worse than the
/// error message.
fn resolve_item_id<'a>(raw: &str, items: &'a [ChecklistItem]) -> Option<&'a ChecklistItem> {
    let raw = raw.trim();
    if let Some(hit) = items.iter().find(|it| it.item_id == raw) {
        return Some(hit);
    }
    if raw.len() < 8 {
        return None;
    }
    let lower = raw.to_lowercase();
    let mut matches = items
        .iter()
        .filter(|it| it.item_id.to_lowercase().starts_with(&lower));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// The valid item ids, for an error message that tells the model what to do
/// next instead of only what it did wrong.
fn valid_ids(items: &[ChecklistItem]) -> String {
    items
        .iter()
        .map(|it| it.item_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse and validate one `claim_done` tool input against the task's checklist
/// and the screenshot seqs this run has actually posted.
///
/// Every rejection returns text meant to be READ BY THE MODEL and acted on: it
/// names what was wrong and what the legal values are, because the model can
/// call `claim_done` again and a claim that is merely re-asked is worth far
/// more than one silently coerced into shape. Nothing here is lenient about
/// identity — an unknown `item_id` or a `frame_seq` from no frame we posted is
/// refused outright, since the entire value of the claim to the operator is
/// that its references resolve.
pub fn parse_done_claim(
    input: &Value,
    checklist: &[ChecklistItem],
    frame_seqs: &[i64],
) -> Result<DoneClaim, String> {
    if checklist.is_empty() {
        return Err("claim_done does not apply to this task: it carries no checklist.".to_string());
    }
    let Some(arr) = input.get("items").and_then(|i| i.as_array()) else {
        return Err(format!(
            "claim_done requires `items`: an array with one entry per checklist item. \
             Valid item_id values: {}",
            valid_ids(checklist)
        ));
    };
    if arr.is_empty() {
        return Err(format!(
            "claim_done requires at least one entry in `items`. Valid item_id values: {}",
            valid_ids(checklist)
        ));
    }
    if arr.len() > CLAIM_MAX_ITEMS {
        return Err(format!(
            "claim_done accepts at most {CLAIM_MAX_ITEMS} entries; you sent {}. \
             Send one entry per checklist item and no duplicates.",
            arr.len()
        ));
    }

    let mut items: Vec<ClaimedItem> = Vec::with_capacity(arr.len());
    for entry in arr {
        let raw_id = entry.get("item_id").and_then(|i| i.as_str()).unwrap_or("");
        if raw_id.trim().is_empty() {
            return Err(format!(
                "every entry in claim_done needs an `item_id`. Valid item_id values: {}",
                valid_ids(checklist)
            ));
        }
        let Some(item) = resolve_item_id(raw_id, checklist) else {
            return Err(format!(
                "claim_done: '{raw_id}' is not an item_id on this task. \
                 Call claim_done again using only these item_id values: {}",
                valid_ids(checklist)
            ));
        };
        // No default. `satisfied` is the single bit the operator's eye goes to,
        // and a missing one silently read as `false` would report a finished
        // item as unfinished (or, read as `true`, invent a claim the model
        // never made). Both are worse than one more turn.
        let Some(satisfied) = entry.get("satisfied").and_then(|s| s.as_bool()) else {
            return Err(format!(
                "claim_done: entry for item_id '{}' is missing `satisfied` (must be true or false).",
                item.item_id
            ));
        };
        let note = entry
            .get("evidence_note")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .unwrap_or("");
        if note.is_empty() {
            return Err(format!(
                "claim_done: entry for item_id '{}' is missing `evidence_note` — say what on \
                 screen shows this item is or is not satisfied.",
                item.item_id
            ));
        }
        let frame_seq = match entry.get("frame_seq") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let Some(n) = v.as_i64() else {
                    return Err(format!(
                        "claim_done: `frame_seq` for item_id '{}' must be a number \
                         (or omitted if you have no single frame for it).",
                        item.item_id
                    ));
                };
                if !frame_seqs.contains(&n) {
                    return Err(format!(
                        "claim_done: frame_seq {n} is not a screenshot from this run. {} \
                         Omit frame_seq if you have no single frame for this item.",
                        available_frames_sentence(frame_seqs)
                    ));
                }
                Some(n)
            }
        };
        if items.iter().any(|c| c.item_id == item.item_id) {
            return Err(format!(
                "claim_done: item_id '{}' appears twice. Send one entry per checklist item.",
                item.item_id
            ));
        }
        items.push(ClaimedItem {
            item_id: item.item_id.clone(),
            text: truncate_chars(&item.text, CLAIM_ITEM_TEXT_CHARS),
            satisfied,
            evidence_note: truncate_chars(note, CLAIM_NOTE_CHARS),
            frame_seq,
        });
    }

    let summary = input
        .get("summary")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .unwrap_or("");
    if summary.is_empty() {
        return Err(
            "claim_done requires a one-line `summary` of what you did in this run.".to_string()
        );
    }
    Ok(DoneClaim { items, summary: truncate_chars(summary, CLAIM_SUMMARY_CHARS) })
}

/// The tail of the "bad frame_seq" error: which numbers WOULD have worked. The
/// last few only — a long run posts dozens of frames and the model's evidence
/// is almost always recent, so the full list would spend context to make the
/// correction harder to read.
fn available_frames_sentence(frame_seqs: &[i64]) -> String {
    if frame_seqs.is_empty() {
        return "This run has posted no screenshots yet.".to_string();
    }
    let tail: Vec<String> = frame_seqs
        .iter()
        .rev()
        .take(10)
        .rev()
        .map(|s| s.to_string())
        .collect();
    format!("Most recent frame_seq values: {}.", tail.join(", "))
}

/// The one-line note appended to a turn's tool results naming the frame numbers
/// that turn's screenshots were filed under.
///
/// Without it `frame_seq` is unusable: the seqs are allocated by the run loop
/// AFTER a turn's actions are dispatched, so the model has no way to learn the
/// number of the picture it is looking at, and a claim could only ever cite
/// frames by guess. Emitted only on runs where `claim_done` applies — every
/// other run would be paying context for a number nothing consumes.
pub fn frame_seq_note(seqs: &[i64]) -> Option<String> {
    if seqs.is_empty() {
        return None;
    }
    let list = seqs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
    Some(format!(
        "[worker] The screenshot(s) above were filed as frame_seq {list}. \
         Cite these numbers as `frame_seq` in claim_done."
    ))
}

/// The `claim_done` tool schema, or None when this run's task has nothing left
/// to claim (see `claim_applies`) — which is also how a run with NO checklist
/// gets no tool: `items` is empty, so nothing is owed. A model that invents the
/// call anyway lands in agent.rs's `unknown tool` arm, exactly like any other
/// hallucinated name.
///
/// # Why a separate tool
///
/// Same reason as `click_element` and `launch_browser`: `computer_20251124` is
/// the server-defined, schema-LESS tool, so there is no `input_schema` in which
/// to declare an extra action and no chance a model emits one it was never
/// trained on. A custom tool beside it is the only mechanism the API offers.
///
/// # Why the ids are inlined into the description and the enum
///
/// The valid ids are the single thing the model cannot get wrong and still be
/// useful, and a small self-hosted model does far better copying from an
/// adjacent list than from a prompt section several thousand tokens back. The
/// `enum` also constrains decoding on endpoints that use the schema for that.
pub fn claim_done_tool(items: &[ChecklistItem]) -> Option<Value> {
    if !claim_applies(items) {
        return None;
    }
    let owed = owed_items(items);
    let ids: Vec<&str> = items.iter().map(|it| it.item_id.as_str()).collect();
    let owed_list = owed
        .iter()
        .map(|it| format!("{} = {}", it.item_id, it.text))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(json!({
        "name": "claim_done",
        "description": format!(
            "Report, item by item, which of this task's checklist items you believe you \
satisfied. Call this ONCE, as the last thing you do, before you stop. Send one entry for every \
item still owed: {owed_list}. This does NOT mark anything done — a human reads your claim and \
decides; you cannot approve your own work. Say satisfied:false for anything you did not finish. \
A false is free; a claim that turns out to be wrong wastes the operator's review and the run."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "One entry per checklist item still owed.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "item_id": {
                                "type": "string",
                                "enum": ids,
                                "description": "The checklist item's id, copied exactly."
                            },
                            "satisfied": {
                                "type": "boolean",
                                "description": "True only if you believe this item is now satisfied."
                            },
                            "evidence_note": {
                                "type": "string",
                                "description": "What on screen shows it — the window, the text, \
the value you read. One sentence."
                            },
                            "frame_seq": {
                                "type": "integer",
                                "description": "The frame_seq of the screenshot that shows it, as \
printed under the screenshot. Omit if no single frame shows this item."
                            }
                        },
                        "required": ["item_id", "satisfied", "evidence_note"]
                    }
                },
                "summary": {
                    "type": "string",
                    "description": "One line: what you did in this run."
                }
            },
            "required": ["items", "summary"]
        }
    }))
}

/// What the model is told after a claim is accepted. Reads back the counts so a
/// model that mangled an entry can see it, and repeats the invariant — a model
/// that believes `claim_done` finished the task will happily go on to claim
/// authority it does not have.
pub fn claim_ack_text(claim: &DoneClaim) -> String {
    format!(
        "Claim recorded: {} of {} items claimed satisfied. This is a claim, not an approval — \
         the operator decides. Nothing further is needed; stop now.",
        claim.satisfied_count(),
        claim.items.len()
    )
}

/// The diary payload for a done claim. A `status` message, because that is what
/// the worker→admin types allow (`goal`/`nudge`/`verdict` are the admin's,
/// `question` would block a run that has already ended, and `receipt` answers a
/// specific inbound message) — the console dispatches on `kind`, which costs
/// the backend nothing and needs no new message type.
///
/// `text` carries the whole thing in prose as well, and says "claims" in its
/// first clause. That redundancy is the point: this row is read by consumers we
/// do not control (a phone notification, a future model summarizing the log),
/// and every one of them must land on "the worker asserts" rather than "done".
pub fn claim_status_payload(run_id: &str, claim: &DoneClaim) -> Value {
    let sat = claim.satisfied_count();
    let total = claim.items.len();
    let mut text = format!(
        "The worker CLAIMS {sat} of {total} checklist item(s) satisfied — an assertion by the \
         agent, NOT an approval. Verify each one before you accept it.\nSummary: {}",
        claim.summary
    );
    for c in &claim.items {
        let mark = if c.satisfied { "claims YES" } else { "claims NO" };
        let frame = match c.frame_seq {
            Some(s) => format!(" (frame {s})"),
            None => " (no frame cited)".to_string(),
        };
        text.push_str(&format!("\n- {} — {}{}: {}", c.text, mark, frame, c.evidence_note));
    }
    json!({
        "kind": "done_claim",
        "run_id": run_id,
        "text": text,
        "summary": claim.summary,
        "claimed_satisfied": sat,
        "claimed_total": total,
        "claims": claim
            .items
            .iter()
            .map(|c| json!({
                "item_id": c.item_id,
                "text": c.text,
                "satisfied": c.satisfied,
                "evidence_note": c.evidence_note,
                "frame_seq": c.frame_seq,
            }))
            .collect::<Vec<_>>(),
    })
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
            // Each item is printed as its own two-line block with the id on the
            // first line. The ids are UUIDs and the model has to reproduce one
            // per `claim_done` entry; a prose bullet with the id buried in it is
            // measurably harder for a small model to copy than a labelled line
            // it can read straight off.
            for it in &owed {
                out.push_str(&format!("\n- item_id: {}\n  {}", it.item_id, it.text));
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
        if !owed.is_empty() {
            out.push_str(CLAIM_CONTRACT);
        }
    }
    out
}

/// The last paragraph of a checklisted run's prompt: the per-item reporting
/// contract, appended verbatim whenever `claim_done` is offered (the two are
/// gated on the same predicate, so the model is never told about a tool it does
/// not have).
///
/// Written for the weakest model in the fleet — a qwen3-8B or a 27B — which is
/// why it is short, gives ONE literal call shape rather than describing a
/// schema, and states the honest-false incentive in a single clause. The
/// closing sentence is not decoration: the failure this whole feature exists to
/// avoid is a confident final narration, and a model that thinks `claim_done`
/// settles the matter produces exactly that in a different wrapper.
const CLAIM_CONTRACT: &str = "\n\nBefore you stop you MUST call the `claim_done` tool, once, \
with one entry for EVERY item_id listed above:\n\
{\"items\": [{\"item_id\": \"<the id above>\", \"satisfied\": true, \"evidence_note\": \"what on \
screen shows this\", \"frame_seq\": <the number printed under the screenshot that shows it>}], \
\"summary\": \"<one line on what you did>\"}\n\
Set satisfied to false for anything you did not finish — a false is free, and a claim that turns \
out to be wrong wastes the run. Omit frame_seq when no single screenshot shows the item. \
claim_done does NOT complete the task: you are telling a human what you believe, and the human \
decides. Do not claim an item is satisfied because it should be — claim it because you saw it.";

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
        // The original text plus the checklist as STRUCTURED rows. The text
        // stays complete on its own (a degraded consumer loses nothing); the
        // array is what renderers and models should read — parsing "[ ]" lines
        // back out of prose is exactly the confusion the operator flagged.
        json!({
            "text": text,
            "checklist": checklist_items(&checklist)
                .iter()
                .map(|it| json!({
                    "item_id": it.item_id,
                    "text": it.text,
                    "approved": it.approved,
                }))
                .collect::<Vec<_>>(),
        }),
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
    // Park the checklist for the run we are about to start: the run prompt just
    // told the model to call `claim_done`, and this is what makes the tool exist
    // and gives its validation the only list of ids that counts. `run_agent`
    // TAKES it, so the failure path below has to put it back down (see
    // `set_pending_checklist`) or the next local run would inherit it.
    set_pending_checklist(app, checklist_items(&checklist));
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
            // No run will take the parked checklist; drop it here so it cannot
            // leak into whatever run does start next.
            let _ = take_run_checklist(app);
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

    // The done claim, if the model made one, BEFORE the outcome row and before
    // the move to `awaiting_verdict` — so by the time the task shows up on the
    // verdict screen the operator already has, in the same log, which item the
    // worker thinks it satisfied and which frame it says proves it. Posted on
    // the failure path too: a claim made at minute 40 of a run that died at
    // minute 41 is still the best account of what got done.
    if let Some(claim) = take_claim_for(app, &run_id) {
        post_status(
            app, client, base, device_id,
            &format!("claim-{run_id}"),
            &task_id,
            claim_status_payload(&run_id, &claim),
            cancel,
        )
        .await;
    }

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
        // Owed items carry their item_id on a labelled line of its own — the
        // model has to reproduce it verbatim in `claim_done`.
        assert!(out.contains("items still owed:\n- item_id: b\n  docs updated"));
        assert!(out.contains("do NOT redo or rework these:\n- [done] tests pass"));
        // Approved items are NOT claimable, so their ids stay out of the prompt.
        assert!(!out.contains("item_id: a"));
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

    // ---- the done claim ----------------------------------------------------

    /// Two owed items, the shape a real task has.
    fn two_items() -> Vec<ChecklistItem> {
        checklist_items(&json!([
            {"item_id": "e4777019-5f63-4776-a800-577c965bb88c",
             "text": "Notepad contains the text \"checklist item one\"", "approved": false},
            {"item_id": "d6d7e296-e8b2-4535-92cc-d74ec0b3fba0",
             "text": "Calculator displays the result 42", "approved": false},
        ]))
    }

    fn full_claim() -> Value {
        json!({
            "items": [
                {"item_id": "e4777019-5f63-4776-a800-577c965bb88c", "satisfied": true,
                 "evidence_note": "Notepad shows the line", "frame_seq": 12},
                {"item_id": "d6d7e296-e8b2-4535-92cc-d74ec0b3fba0", "satisfied": false,
                 "evidence_note": "Calculator still shows 0"},
            ],
            "summary": "typed the line, calculator not done",
        })
    }

    /// The run prompt asks for the claim only when the tool exists — the two are
    /// gated on the same predicate, so the model is never told to call something
    /// it was not given, nor given a tool nothing explains.
    #[test]
    fn claim_contract_and_tool_appear_together() {
        let owed = json!([{"item_id": "b", "text": "docs updated", "approved": false}]);
        let all_done = json!([{"item_id": "a", "text": "tests pass", "approved": true}]);

        let out = task_run_spec("s", &owed, None);
        assert!(out.contains("call the `claim_done` tool"));
        assert!(out.contains("\"frame_seq\""), "one literal call shape, not a schema");
        assert!(out.contains("does NOT complete the task"), "the claim is never an approval");
        assert!(claim_done_tool(&checklist_items(&owed)).is_some());

        let out = task_run_spec("s", &all_done, None);
        assert!(!out.contains("claim_done"), "nothing owed, nothing to claim");
        assert!(claim_done_tool(&checklist_items(&all_done)).is_none());

        // No checklist at all: unchanged from before this layer existed.
        assert_eq!(task_run_spec("s", &Value::Null, None), "s");
        assert!(claim_done_tool(&[]).is_none());
    }

    /// The tool contract: every valid id is in the `item_id` enum (constrained
    /// decoding leans on it), the owed items are spelled out in the description
    /// where a small model reads them, and nothing in it promises approval.
    #[test]
    fn claim_tool_schema_carries_the_ids_and_the_invariant() {
        let items = two_items();
        let schema = claim_done_tool(&items).expect("items are owed");
        assert_eq!(schema["name"], "claim_done");

        let entry = &schema["input_schema"]["properties"]["items"]["items"];
        let ids: Vec<&str> =
            entry["properties"]["item_id"]["enum"].as_array().unwrap()
                .iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(ids, vec![items[0].item_id.as_str(), items[1].item_id.as_str()]);
        assert_eq!(
            entry["required"].as_array().unwrap().len(), 3,
            "frame_seq is the only optional field"
        );
        assert_eq!(
            schema["input_schema"]["required"],
            json!(["items", "summary"])
        );

        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains(&items[0].item_id), "ids inlined next to the call");
        assert!(desc.contains("Calculator displays the result 42"));
        assert!(desc.contains("does NOT mark anything done"));
    }

    /// The happy path: ids resolve, the checklist text is denormalized onto each
    /// claim so the console needs no join, and an omitted frame_seq stays None.
    #[test]
    fn claim_parses_and_denormalizes_the_item_text() {
        let claim = parse_done_claim(&full_claim(), &two_items(), &[9, 12]).unwrap();
        assert_eq!(claim.items.len(), 2);
        assert!(claim.items[0].satisfied);
        assert_eq!(claim.items[0].frame_seq, Some(12));
        assert_eq!(claim.items[0].text, "Notepad contains the text \"checklist item one\"");
        assert!(!claim.items[1].satisfied);
        assert_eq!(claim.items[1].frame_seq, None, "no frame is legitimate");
        assert_eq!(claim.summary, "typed the line, calculator not done");
    }

    /// An id the task does not carry is refused, and the refusal NAMES the legal
    /// ids — the model can call again, which is the only reason to reject rather
    /// than coerce.
    #[test]
    fn claim_rejects_an_unknown_item_id() {
        let mut input = full_claim();
        input["items"][0]["item_id"] = json!("made-up-id");
        let err = parse_done_claim(&input, &two_items(), &[12]).unwrap_err();
        assert!(err.contains("'made-up-id' is not an item_id on this task"));
        assert!(err.contains("e4777019-5f63-4776-a800-577c965bb88c"));
        assert!(err.contains("d6d7e296-e8b2-4535-92cc-d74ec0b3fba0"));
    }

    /// A UUID is 36 characters for a model to transcribe; an unambiguous prefix
    /// resolves to the real id (and is STORED as the real id), while a prefix
    /// that could mean either item is refused rather than guessed.
    #[test]
    fn claim_resolves_a_unique_id_prefix_but_never_an_ambiguous_one() {
        let items = checklist_items(&json!([
            {"item_id": "aaaa1111-2222", "text": "one", "approved": false},
            {"item_id": "aaaa1111-3333", "text": "two", "approved": false},
        ]));
        let ok = json!({
            "items": [{"item_id": "aaaa1111-2", "satisfied": true, "evidence_note": "n"}],
            "summary": "s",
        });
        let claim = parse_done_claim(&ok, &items, &[]).unwrap();
        assert_eq!(claim.items[0].item_id, "aaaa1111-2222", "stored canonical");

        let ambiguous = json!({
            "items": [{"item_id": "aaaa1111", "satisfied": true, "evidence_note": "n"}],
            "summary": "s",
        });
        assert!(parse_done_claim(&ambiguous, &items, &[]).is_err());
        // Too short to be a prefix at all — an id that happens to head both.
        let short = json!({
            "items": [{"item_id": "aaaa", "satisfied": true, "evidence_note": "n"}],
            "summary": "s",
        });
        assert!(parse_done_claim(&short, &items, &[]).is_err());
    }

    /// The fields with no safe default. `satisfied` read as false would report
    /// finished work as unfinished; read as true would invent a claim. An empty
    /// `evidence_note` or `summary` is the prose blob this feature exists to
    /// replace.
    #[test]
    fn claim_rejects_missing_fields() {
        let items = two_items();
        let strip = |key: &str| {
            let mut input = full_claim();
            input["items"][0].as_object_mut().unwrap().remove(key);
            parse_done_claim(&input, &items, &[12]).unwrap_err()
        };
        assert!(strip("satisfied").contains("missing `satisfied`"));
        assert!(strip("evidence_note").contains("missing `evidence_note`"));
        assert!(strip("item_id").contains("needs an `item_id`"));

        let mut no_summary = full_claim();
        no_summary.as_object_mut().unwrap().remove("summary");
        assert!(parse_done_claim(&no_summary, &items, &[12])
            .unwrap_err()
            .contains("one-line `summary`"));

        assert!(parse_done_claim(&json!({"summary": "s"}), &items, &[]).is_err());
        assert!(parse_done_claim(&json!({"items": [], "summary": "s"}), &items, &[]).is_err());
        // Whitespace is not evidence.
        let mut blank = full_claim();
        blank["items"][0]["evidence_note"] = json!("   ");
        assert!(parse_done_claim(&blank, &items, &[12]).is_err());
    }

    /// A frame_seq must name a screenshot THIS run actually posted — the whole
    /// value of the citation to the operator is that it resolves to a thumbnail.
    #[test]
    fn claim_rejects_a_frame_seq_from_no_frame_we_posted() {
        let items = two_items();
        let err = parse_done_claim(&full_claim(), &items, &[3, 5]).unwrap_err();
        assert!(err.contains("frame_seq 12 is not a screenshot from this run"));
        assert!(err.contains("3, 5"), "the correction names what would work");
        assert!(err.contains("Omit frame_seq"));

        // No frames posted at all: still refused, with an honest reason.
        assert!(parse_done_claim(&full_claim(), &items, &[])
            .unwrap_err()
            .contains("posted no screenshots yet"));

        // Not a number.
        let mut bad = full_claim();
        bad["items"][0]["frame_seq"] = json!("twelve");
        assert!(parse_done_claim(&bad, &items, &[12]).unwrap_err().contains("must be a number"));

        // Explicit null is "I have no frame", not an error.
        let mut null_frame = full_claim();
        null_frame["items"][0]["frame_seq"] = Value::Null;
        assert_eq!(
            parse_done_claim(&null_frame, &items, &[]).unwrap().items[0].frame_seq,
            None
        );
    }

    /// Two entries for one item would leave the operator with two verdicts to
    /// give on one criterion; and no checklist means no claim at all.
    #[test]
    fn claim_rejects_duplicates_and_a_checklistless_task() {
        let items = two_items();
        let mut dup = full_claim();
        dup["items"][1]["item_id"] = dup["items"][0]["item_id"].clone();
        assert!(parse_done_claim(&dup, &items, &[12]).unwrap_err().contains("appears twice"));

        assert!(parse_done_claim(&full_claim(), &[], &[12])
            .unwrap_err()
            .contains("carries no checklist"));
    }

    /// The payload cap is 16KB serialized; the claim is bounded per FIELD so a
    /// long-winded note clips instead of the last item vanishing.
    #[test]
    fn claim_is_bounded_and_capped_in_item_count() {
        let items = two_items();
        let mut long = full_claim();
        long["items"][0]["evidence_note"] = json!("é".repeat(CLAIM_NOTE_CHARS + 400));
        long["summary"] = json!("x".repeat(CLAIM_SUMMARY_CHARS + 400));
        let claim = parse_done_claim(&long, &items, &[12]).unwrap();
        assert!(claim.items[0].evidence_note.ends_with('…'));
        assert!(claim.items[0].evidence_note.chars().count() <= CLAIM_NOTE_CHARS + 1);
        assert!(claim.summary.chars().count() <= CLAIM_SUMMARY_CHARS + 1);

        let many: Vec<Value> = (0..CLAIM_MAX_ITEMS + 1)
            .map(|_| json!({"item_id": items[0].item_id, "satisfied": true, "evidence_note": "n"}))
            .collect();
        assert!(parse_done_claim(&json!({"items": many, "summary": "s"}), &items, &[])
            .unwrap_err()
            .contains("at most"));
    }

    /// The payload the console renders. It is a `status` message's body (the
    /// only worker→admin type that fits), it carries structured rows AND prose,
    /// and every rendering of it says "claims" — a false claim must read as a
    /// false claim, never as a fact.
    #[test]
    fn claim_payload_is_structured_and_never_says_done() {
        let claim = parse_done_claim(&full_claim(), &two_items(), &[12]).unwrap();
        let p = claim_status_payload("run-1", &claim);
        assert_eq!(p["kind"], "done_claim");
        assert_eq!(p["run_id"], "run-1");
        assert_eq!(p["claimed_satisfied"], 1);
        assert_eq!(p["claimed_total"], 2);
        assert_eq!(p["claims"][0]["frame_seq"], 12);
        assert_eq!(p["claims"][0]["item_id"], "e4777019-5f63-4776-a800-577c965bb88c");
        assert_eq!(p["claims"][1]["frame_seq"], Value::Null);

        let text = p["text"].as_str().unwrap();
        assert!(text.starts_with("The worker CLAIMS 1 of 2"));
        assert!(text.contains("NOT an approval"));
        assert!(text.contains("claims YES (frame 12)"));
        assert!(text.contains("claims NO (no frame cited)"));

        // Comfortably inside the 16KB serialized payload cap.
        assert!(p.to_string().len() < PAYLOAD_SANITY_BYTES);

        // And the model is told it claimed, not that it finished.
        let ack = claim_ack_text(&claim);
        assert!(ack.contains("1 of 2"));
        assert!(ack.contains("not an approval"));
    }

    /// Local slack under the backend's 16KB PAYLOAD_MAX_BYTES.
    const PAYLOAD_SANITY_BYTES: usize = 16 * 1024;

    /// `frame_seq` is only citable because the worker feeds the numbers back —
    /// they are allocated after the turn that produced the frame, so the model
    /// has no other way to learn them.
    #[test]
    fn frame_seq_note_names_this_turns_frames() {
        assert_eq!(frame_seq_note(&[]), None, "a turn with no frames says nothing");
        let note = frame_seq_note(&[7, 9]).unwrap();
        assert!(note.contains("frame_seq 7, 9"));
        assert!(note.contains("claim_done"));
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
