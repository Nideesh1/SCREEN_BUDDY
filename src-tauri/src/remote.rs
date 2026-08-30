//! Always-on remote control channel.
//!
//! Opens ONE persistent WebSocket to the backend so it can push run commands to
//! this desktop and have a computer-use run start automatically — the same path
//! a locally-launched run takes. The wire is deliberately tiny:
//!
//!   backend → desktop  {"type":"run","run_id":"uuid","task":"…","model":"…",
//!                       "model_endpoint":"https://…",
//!                       "pinned_set_names":"[\"Set name\"]"}
//!                      {"type":"snapshot"}
//!                      {"type":"ping"}
//!   desktop → backend  {"type":"ack","run_id":"uuid"}
//!                      {"type":"ack","kind":"snapshot"}
//!                      {"type":"pong"}
//!
//! A `snapshot` frame asks "what is this machine looking at right now" — the
//! console's way of pulling a fresh frame from a machine that is idle, and so it
//! carries no `run_id` and the uploaded object belongs to no run. Its ack means
//! CAPTURE ACCEPTED, not uploaded: the frame reaches the backend out of band via
//! `POST /screenshots/commit`, and the ack goes out long before that lands. The
//! console learns the upload happened from the commit, never from this socket.
//!
//! A `run` frame is ack'd immediately, then funneled through
//! `agent::start_run_internal` (the exact lock/RunLease/persistence path that
//! `start_agent_task` uses). The session token doubles as the WS auth (query
//! param) AND the run's `auth` bearer, so a remotely-started run persists
//! identically to a normal one. If a run is already in flight we still ack and
//! skip — the backend learns the desktop is busy via the absence of progress,
//! not a dropped frame.
//!
//! `model_endpoint` is the operator's fleet setting, travelling with the work so
//! the machine no longer has to be told separately what to drive. It is OPTIONAL
//! on the wire: an older backend omits it and this desktop then falls back to
//! its own `CU_ANTHROPIC_BASE` exactly as before (see `agent::resolve_endpoint`).
//!
//! Resilience: the task reconnects with exponential backoff (1s → 30s) on any
//! close/error and loops forever until the managed `RemoteState` token is
//! cancelled (`stop_remote_listener`, or a second `start_remote_listener` which
//! cancels the prior task first). A `remote://status` event with `{connected}`
//! is emitted on every connect/disconnect so the UI can show an indicator.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::time::interval;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentState;

/// Frontend event carrying the live connection state of the remote channel.
pub const EV_REMOTE_STATUS: &str = "remote://status";

/// Backoff bounds for reconnect.
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often we send a WebSocket-level ping to keep the socket warm and notice a
/// dead peer promptly (independent of the app-level ping/pong frames).
const WS_PING_EVERY: Duration = Duration::from_secs(20);

/// Holds the cancellation token for the single in-flight listener task (if any),
/// so a second `start_remote_listener` cancels the prior one instead of opening
/// a second socket. Managed as Tauri state.
#[derive(Default)]
pub struct RemoteState(pub Mutex<Option<CancellationToken>>);

/// Last value pushed to `remote://status`, so a view that mounts between
/// connect/disconnect events can ask instead of waiting for the next one.
///
/// The event alone is enough for an indicator that lives for the life of the
/// app, but the worker machine panel is opened cold and the link is the one
/// thing on it that says whether the machine can receive work at all — leaving
/// that reading "checking..." until the socket next changes state is the wrong
/// answer for the longest, and a machine that has been happily connected for an
/// hour is exactly the case that never fires an event.
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Derive the WebSocket URL from the backend HTTP(S) base: http→ws, https→wss,
/// and append the listen path with the session token as a query param.
///
/// `device_id` rides along so the backend can stamp liveness against the right
/// machine: the token identifies the *user*, and one user can have several
/// desktops connected at once. It stays SECOND in the query string on purpose —
/// `ws_url_redacted` truncates at `?token=`, so anything after the token is
/// hidden along with it and the redaction keeps working unchanged.
fn ws_url(backend: &str, token: &str, device_id: &str) -> String {
    let base = backend.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a ws(s) scheme or bare host — pass through untouched.
        base.to_string()
    };
    format!(
        "{ws_base}/agent/listen?token={}&device_id={}",
        urlencoding::encode(token),
        urlencoding::encode(device_id)
    )
}

/// The same URL with the token query stripped, for logging.
///
/// The session token is a live bearer credential — it authenticates the WS AND
/// doubles as the run's auth. Logging the full URL wrote it to stdout on every
/// connect, so it landed in terminal scrollback, any redirected log file, and CI
/// output. Log the endpoint, never the credential.
fn ws_url_redacted(url: &str) -> &str {
    match url.find("?token=") {
        Some(i) => &url[..i],
        None => url,
    }
}

/// Emit the `remote://status` event so the UI indicator can reflect the link.
/// Also records it for `remote_status`, so the push and the pull can never
/// disagree — every state change goes through here.
fn emit_status(app: &AppHandle, connected: bool) {
    CONNECTED.store(connected, Ordering::Relaxed);
    let _ = app.emit(EV_REMOTE_STATUS, json!({ "connected": connected }));
}

/// Whether the command channel is up right now. Pull counterpart to the
/// `remote://status` event; a view should read this on mount and subscribe for
/// changes.
#[tauri::command]
pub fn remote_status() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

/// Handle one decoded text frame. Returns the reply string to send back (if
/// any). `run` frames start a run via the shared internal path.
fn handle_text(app: &AppHandle, backend: &str, auth: &str, text: &str) -> Option<String> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[remote] ignoring non-JSON frame: {e}");
            return None;
        }
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("ping") => Some(json!({ "type": "pong" }).to_string()),
        Some("snapshot") => {
            // Detached: the capture and its three-legged upload must not hold up
            // the read loop, which is also how run frames arrive. `auth` is this
            // socket's credential — the device token on an enrolled worker — so
            // the presign/commit legs authenticate exactly as run persistence
            // does.
            //
            // Deliberately independent of any run in flight: no `run_id`, no
            // `AgentState`, and no `ComputerState` (see
            // `screenshots::spawn_snapshot`), so a live run's timeline and its
            // click-coordinate scaling are both untouched.
            crate::screenshots::spawn_snapshot(app, backend, auth);
            Some(json!({ "type": "ack", "kind": "snapshot" }).to_string())
        }
        Some("run") => {
            let run_id = v.get("run_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
            let task = v.get("task").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let model = v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string());
            // `model_endpoint` (optional): where the operator wants this run's
            // model calls to go. Accepted under either casing because the wire is
            // snake_case but the account-settings field it is sourced from is
            // camelCase, and a backend that forwards the settings value verbatim
            // is a plausible mistake we would rather absorb than fail a fleet on.
            // Absent → the desktop's own env var wins; see `agent::resolve_endpoint`.
            let model_endpoint = v
                .get("model_endpoint")
                .or_else(|| v.get("modelEndpoint"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string());
            // `pinned_set_ids` (optional) is a JSON-encoded STRING holding a list
            // of LOCAL set UUIDs, e.g. "[\"a1b2…\"]". These are already local set
            // ids (the backend registry stores the desktop's own uuids), so when
            // present we use them DIRECTLY — no name lookup. Parse leniently: any
            // absence/parse failure yields an empty list (never crash).
            let direct_ids: Vec<String> = v
                .get("pinned_set_ids")
                .and_then(|p| p.as_str())
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_default();
            // `pinned_set_names` (optional) is a JSON-encoded STRING holding a
            // list of set NAMES, e.g. "[\"Weekly groceries\"]". Parse it
            // leniently: any absence/parse failure yields an empty list — the
            // listener must never crash on a malformed field.
            let pinned_set_names: Vec<String> = v
                .get("pinned_set_names")
                .and_then(|p| p.as_str())
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_default();
            // Prefer the direct uuids when present; only fall back to resolving
            // NAMES via the local pinned index when no direct ids were given.
            // Names with no local match are skipped with a warning (never fatal).
            let pinned_set_ids: Vec<String> = if !direct_ids.is_empty() {
                direct_ids
            } else if pinned_set_names.is_empty() {
                Vec::new()
            } else {
                let sets = crate::pinned::pinned_list(app.clone()).unwrap_or_default();
                pinned_set_names
                    .iter()
                    .filter_map(|name| {
                        match sets.iter().find(|s| &s.name == name) {
                            Some(s) => Some(s.id.clone()),
                            None => {
                                eprintln!("[remote] no local pinned set named {name:?}; skipping");
                                None
                            }
                        }
                    })
                    .collect()
            };
            if run_id.is_empty() || task.is_empty() {
                eprintln!("[remote] run frame missing run_id/task; skipping");
                // Still ack what we can so the backend isn't left hanging.
                return Some(json!({ "type": "ack", "run_id": run_id }).to_string());
            }
            // Ack first (built before we move run_id into the run), then start.
            let ack = json!({ "type": "ack", "run_id": run_id }).to_string();
            // Start the run through the SAME path as a local launch. The session
            // token is both the WS auth and the run's bearer, so it persists
            // exactly like a normal run.
            if let Some(state) = app.try_state::<AgentState>() {
                match crate::agent::start_run_internal(
                    app,
                    &state,
                    task,
                    auth.to_string(),
                    pinned_set_ids,
                    run_id.clone(),
                    model,
                    model_endpoint,
                    backend.to_string(),
                ) {
                    Ok(()) => eprintln!("[remote] started run {run_id}"),
                    Err(e) => eprintln!("[remote] run {run_id} not started: {e}"),
                }
            } else {
                eprintln!("[remote] AgentState unavailable; cannot start run {run_id}");
            }
            Some(ack)
        }
        other => {
            eprintln!("[remote] ignoring frame type {other:?}");
            None
        }
    }
}

/// One connect → serve → disconnect cycle. Returns when the socket closes/errors
/// (so the caller can back off and reconnect) or when `token` is cancelled
/// (signalled via the returned bool: `true` == shut down for good).
async fn run_connection(app: &AppHandle, url: &str, backend: &str, auth: &str, token: &CancellationToken) -> bool {
    let stream = tokio::select! {
        _ = token.cancelled() => return true,
        r = tokio_tungstenite::connect_async(url) => match r {
            Ok((s, _resp)) => s,
            Err(e) => {
                eprintln!("[remote] connect failed: {e}");
                // A refused HANDSHAKE is how a worker finds out its enrollment is
                // dead — there is no other authenticated call it makes on its own.
                // Surface it and keep backing off; we do not clear the token and
                // we do NOT reopen the socket with anything else.
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                    crate::device::note_rejection(app, resp.status().as_u16(), "agent/listen");
                }
                return false;
            }
        },
    };

    emit_status(app, true);
    eprintln!("[remote] connected");

    // Announce this machine to the fleet. Fired on every successful connect
    // rather than once at listener start, because that is the moment we know the
    // backend is actually reachable — and it gives the retry for free: a
    // registration lost to a server restart is re-sent by the next reconnect,
    // with no retry loop of its own. Detached and never awaited, so a slow or
    // dead `/devices` cannot delay serving run frames.
    tauri::async_runtime::spawn(crate::device::register(
        app.clone(),
        backend.to_string(),
        auth.to_string(),
    ));

    let (mut write, mut read) = stream.split();
    let mut ping = interval(WS_PING_EVERY);
    ping.tick().await; // consume the immediate first tick

    let shutting_down = loop {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = write.send(Message::Close(None)).await;
                break true;
            }
            _ = ping.tick() => {
                if write.send(Message::Ping(Vec::new())).await.is_err() {
                    break false;
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(reply) = handle_text(app, backend, auth, &text) {
                            if write.send(Message::Text(reply)).await.is_err() {
                                break false;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = write.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break false,
                    Some(Ok(_)) => {} // binary/pong/frame — ignore
                    Some(Err(e)) => {
                        eprintln!("[remote] socket error: {e}");
                        break false;
                    }
                }
            }
        }
    };

    emit_status(app, false);
    eprintln!("[remote] disconnected");
    shutting_down
}

/// The forever-loop: connect, serve, and on any drop back off (1s→30s, reset on
/// a clean connect) and retry — until the listener token is cancelled.
async fn listen_loop(app: AppHandle, url: String, backend: String, auth: String, token: CancellationToken) {
    let mut backoff = BACKOFF_START;
    loop {
        if token.is_cancelled() {
            return;
        }
        let connected_at = std::time::Instant::now();
        if run_connection(&app, &url, &backend, &auth, &token).await {
            return; // cancelled — shut down for good
        }
        // A connection that survived a while resets the backoff; a fast failure
        // (e.g. immediate refused) keeps escalating it.
        if connected_at.elapsed() >= Duration::from_secs(5) {
            backoff = BACKOFF_START;
        }
        let wait = backoff;
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

// ---- Tauri commands -------------------------------------------------------

/// Start (or restart) the always-on remote listener. Idempotent: cancels any
/// prior task before spawning a fresh one, so calling twice never opens two
/// sockets. `token` is the session token the frontend holds, if any; `backend` is
/// the HTTP(S) base (http→ws / https→wss).
///
/// `token` is OPTIONAL because an enrolled worker genuinely has none — its
/// credential lives in the Rust store and never reaches the webview, so the
/// frontend calls this with `backend` alone. Requiring it made Tauri reject the
/// invocation before the command ever ran, and since the call site is
/// best-effort the socket simply never opened: the machine sat there enrolled,
/// idle and unreachable, with nothing anywhere saying why.
///
/// What actually goes on the wire is whatever `credentials::backend_credential`
/// returns — the stored device token on an enrolled worker, the session token
/// otherwise. The URL is fixed for the life of the task, so a machine that
/// enrols mid-session reconnects with the new credential when the frontend
/// restarts the listener (which it does on the credential class changing).
#[tauri::command]
pub fn start_remote_listener(
    app: AppHandle,
    state: tauri::State<'_, RemoteState>,
    token: Option<String>,
    backend: String,
) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| format!("remote state poisoned: {e}"))?;
    // Cancel any existing listener so we don't double-connect.
    if let Some(prev) = guard.take() {
        prev.cancel();
    }
    let cancel = CancellationToken::new();
    *guard = Some(cancel.clone());
    drop(guard);

    // A device id we cannot read is not worth refusing to connect over: the
    // socket still works without it, the backend just can't attribute liveness.
    let device_id = crate::device::device_id(&app).unwrap_or_else(|e| {
        eprintln!("[remote] no device id ({e}); connecting without one");
        String::new()
    });
    // One credential choice, made here and carried through both the socket and
    // any run this socket starts.
    let token = crate::credentials::backend_credential(&app, &token.unwrap_or_default())
        .unwrap_or_default();
    let url = ws_url(&backend, &token, &device_id);
    eprintln!(
        "[remote] listener starting → {} (device {device_id})",
        ws_url_redacted(&url)
    );
    tauri::async_runtime::spawn(listen_loop(app, url, backend, token, cancel));
    Ok(())
}

/// Stop the remote listener (if running). Safe to call when nothing runs.
#[tauri::command]
pub fn stop_remote_listener(state: tauri::State<'_, RemoteState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| format!("remote state poisoned: {e}"))?;
    if let Some(token) = guard.take() {
        token.cancel();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ws_scheme_and_path() {
        assert_eq!(
            ws_url("https://api.example.com", "abc", "dev-1"),
            "wss://api.example.com/agent/listen?token=abc&device_id=dev-1"
        );
        assert_eq!(
            ws_url("http://localhost:8000/", "a b", "dev 1"),
            "ws://localhost:8000/agent/listen?token=a%20b&device_id=dev%201"
        );
    }

    /// The redaction predates the `device_id` param; this pins the fact that a
    /// second query param did not sneak the credential back into the logs.
    #[test]
    fn redaction_still_hides_the_token_with_a_second_param() {
        let url = ws_url("https://api.example.com", "s3cret", "dev-1");
        let shown = ws_url_redacted(&url);
        assert_eq!(shown, "wss://api.example.com/agent/listen");
        assert!(!shown.contains("s3cret"));
    }
}
