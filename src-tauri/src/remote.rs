//! Always-on remote control channel.
//!
//! Opens ONE persistent WebSocket to the backend so it can push run commands to
//! this desktop and have a computer-use run start automatically — the same path
//! a locally-launched run takes. The wire is deliberately tiny:
//!
//!   backend → desktop  {"type":"run","run_id":"uuid","task":"…","model":"…",
//!                       "pinned_set_names":"[\"Set name\"]"}
//!                      {"type":"ping"}
//!   desktop → backend  {"type":"ack","run_id":"uuid"}
//!                      {"type":"pong"}
//!
//! A `run` frame is ack'd immediately, then funneled through
//! `agent::start_run_internal` (the exact lock/RunLease/persistence path that
//! `start_agent_task` uses). The session token doubles as the WS auth (query
//! param) AND the run's `auth` bearer, so a remotely-started run persists
//! identically to a normal one. If a run is already in flight we still ack and
//! skip — the backend learns the desktop is busy via the absence of progress,
//! not a dropped frame.
//!
//! Resilience: the task reconnects with exponential backoff (1s → 30s) on any
//! close/error and loops forever until the managed `RemoteState` token is
//! cancelled (`stop_remote_listener`, or a second `start_remote_listener` which
//! cancels the prior task first). A `remote://status` event with `{connected}`
//! is emitted on every connect/disconnect so the UI can show an indicator.

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

/// Derive the WebSocket URL from the backend HTTP(S) base: http→ws, https→wss,
/// and append the listen path with the session token as a query param.
fn ws_url(backend: &str, token: &str) -> String {
    let base = backend.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a ws(s) scheme or bare host — pass through untouched.
        base.to_string()
    };
    format!("{ws_base}/agent/listen?token={}", urlencoding::encode(token))
}

/// Emit the `remote://status` event so the UI indicator can reflect the link.
fn emit_status(app: &AppHandle, connected: bool) {
    let _ = app.emit(EV_REMOTE_STATUS, json!({ "connected": connected }));
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
        Some("run") => {
            let run_id = v.get("run_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
            let task = v.get("task").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let model = v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string());
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
                return false;
            }
        },
    };

    emit_status(app, true);
    eprintln!("[remote] connected");
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
/// sockets. `token` is the backend session token (WS auth + run bearer);
/// `backend` is the HTTP(S) base (http→ws / https→wss).
#[tauri::command]
pub fn start_remote_listener(
    app: AppHandle,
    state: tauri::State<'_, RemoteState>,
    token: String,
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

    let url = ws_url(&backend, &token);
    eprintln!("[remote] listener starting → {url}");
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
            ws_url("https://api.example.com", "abc"),
            "wss://api.example.com/agent/listen?token=abc"
        );
        assert_eq!(
            ws_url("http://localhost:8000/", "a b"),
            "ws://localhost:8000/agent/listen?token=a%20b"
        );
    }
}
