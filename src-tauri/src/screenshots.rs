//! Off-machine screenshot delivery.
//!
//! WHY this exists: until now a run's frames were written only to that machine's
//! own disk (`<app_data_dir>/runs/<run_id>/<n>.jpg`, see
//! `agent::runs_save_screenshot_local`). That is the right offline record and it
//! stays — but it means the admin console can never see what a fleet machine is
//! looking at, and diagnosing a run costs a remote-desktop session. Mirroring
//! each frame into object storage turns the console into the place you look.
//!
//! The upload is a THREE-legged handshake, and the middle leg is the unusual one:
//!
//!   1. `POST {backend}/screenshots/presign`  — device/session bearer
//!   2. `PUT  {upload_url}`                   — **no Authorization header**
//!   3. `POST {backend}/screenshots/commit`   — device/session bearer
//!
//! Leg 2 carries no bearer ON PURPOSE. The presigned signature IS the
//! authorization, and object stores reject a request that presents both (the
//! `Authorization` header is not part of the signed canonical request, so it
//! either conflicts with the query-string signature or is treated as a competing
//! auth attempt). Sending our device token to a storage host we do not control
//! would also leak a fleet credential outside the backend's origin. So
//! `put_object` builds its request from the bare client and never goes through
//! `agent::with_bearer`.
//!
//! ## Best-effort, always
//!
//! Every failure here is logged and swallowed. Driving the desktop is the
//! product; this is telemetry. Nothing in this module returns an error that can
//! reach the agent loop, and no call blocks it — `enqueue_run_shot` and
//! `spawn_snapshot` both hand off to a detached task and return immediately.
//!
//! ## Backpressure
//!
//! Uploads can be slower than turns (a fast local run against a self-hosted
//! endpoint, a machine on hotel wifi). "Just spawn a task per frame" then grows
//! an unbounded queue of in-flight requests holding a JPEG each, which is its own
//! failure — memory and socket exhaustion on the machine we were trying to keep
//! healthy. Instead there is a hard ceiling on concurrent uploads
//! (`MAX_RUN_UPLOADS`) and frames arriving over it are DROPPED, newest first,
//! with a count logged.
//!
//! Drop-newest rather than drop-oldest because there is no queue to evict from:
//! an over-limit frame is refused before its bytes are ever decoded, so the
//! policy costs one atomic read. And the two are equivalent for the consumer —
//! the console renders a timeline keyed by `seq`, so a gap is a gap wherever it
//! falls, and the local disk still holds every frame for the run that actually
//! needs forensics. What drop-newest additionally guarantees is that a machine
//! whose uploads are permanently failing does no work per frame beyond that
//! atomic, instead of degrading further the worse the network gets.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde_json::{json, Value};
use tauri::AppHandle;

/// Everything we send is a JPEG from `capture`, at the vision budget.
const CONTENT_TYPE: &str = "image/jpeg";

/// Concurrent run-screenshot uploads allowed at once. Two, not one: a single
/// slot serialises against latency, so one slow round trip on a multi-second
/// turn would drop the next frame even on a healthy link. Two absorbs that while
/// still capping the in-flight bytes at a couple of JPEGs.
const MAX_RUN_UPLOADS: usize = 2;

/// Run-screenshot uploads in flight. The whole backpressure policy.
static RUN_UPLOADS: AtomicUsize = AtomicUsize::new(0);
/// Frames refused by the ceiling, cumulative for the life of the process. Logged
/// on each drop so a machine that is quietly shedding telemetry says so.
static RUN_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Operator snapshots in flight. A SEPARATE counter from `RUN_UPLOADS` so a run
/// saturating its ceiling can never make the console's "show me this machine"
/// button do nothing — the two flows compete for bandwidth, never for slots.
/// One at a time is plenty: a snapshot is a human pressing a button.
static SNAPSHOT_UPLOADS: AtomicUsize = AtomicUsize::new(0);

/// An in-flight permit, released on drop.
///
/// RAII rather than a decrement at the end of the upload because the upload has
/// half a dozen early returns; a leaked permit is permanent (the counter never
/// falls back below the ceiling again) and would silently stop all further
/// uploads for the life of the process.
struct Slot(&'static AtomicUsize);

impl Slot {
    /// Claim a permit if the counter is below `max`, else `None`.
    ///
    /// A CAS loop, not `fetch_add` + compare: `fetch_add` would briefly push the
    /// counter over the ceiling, and two callers racing at the limit could then
    /// both see a value that is already too high and both back off, or both
    /// proceed depending on ordering. The compare-exchange makes "claim" atomic
    /// with the check.
    fn claim(counter: &'static AtomicUsize, max: usize) -> Option<Slot> {
        let mut cur = counter.load(Ordering::Acquire);
        loop {
            if cur >= max {
                return None;
            }
            match counter.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Slot(counter)),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Which leg of the handshake failed, and how.
///
/// The distinction that earns its keep is `Http` vs `Transport`: an HTTP status
/// is the backend refusing us (and 401/403 specifically means this worker's
/// enrollment may be dead, which `device::note_rejection` surfaces to the UI),
/// whereas a transport error is the network and says nothing about credentials.
/// Conflating them would make every flaky connection look like a rejected
/// enrollment in the logs.
#[derive(Debug, PartialEq, Eq)]
enum UploadError {
    Http { stage: &'static str, status: u16 },
    Transport { stage: &'static str, msg: String },
    /// A 2xx whose body was not the shape the contract promises.
    Malformed { stage: &'static str, msg: String },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Http { stage, status } => write!(f, "{stage}: HTTP {status}"),
            UploadError::Transport { stage, msg } => write!(f, "{stage}: request failed: {msg}"),
            UploadError::Malformed { stage, msg } => write!(f, "{stage}: bad response: {msg}"),
        }
    }
}

impl UploadError {
    /// The backend status that means "this credential is no longer good", so the
    /// caller can route it to `device::note_rejection`. Only ever true for a leg
    /// we authenticated: the presigned PUT carries no credential of ours, so a
    /// 403 from storage is an expired signature, not a dead enrollment.
    fn auth_rejection(&self) -> Option<u16> {
        match self {
            UploadError::Http { stage, status }
                if matches!(status, 401 | 403) && *stage != "put" =>
            {
                Some(*status)
            }
            _ => None,
        }
    }
}

fn presign_url(base: &str) -> String {
    format!("{}/screenshots/presign", base.trim_end_matches('/'))
}

fn commit_url(base: &str) -> String {
    format!("{}/screenshots/commit", base.trim_end_matches('/'))
}

/// Presign request body. `run_id`/`seq` are OMITTED, not nulled, when absent —
/// an idle-machine snapshot belongs to no run, and the backend keys the object
/// on whether it was given a run at all.
fn presign_body(run_id: Option<&str>, seq: Option<i64>) -> Value {
    let mut body = json!({ "content_type": CONTENT_TYPE });
    if let Some(rid) = run_id {
        body["run_id"] = json!(rid);
    }
    if let Some(s) = seq {
        body["seq"] = json!(s);
    }
    body
}

/// Commit body. Same omission rule as `presign_body`, plus the key the backend
/// handed us — echoed VERBATIM, never rebuilt: the object layout is the
/// backend's to choose and this worker must not encode an assumption about it.
fn commit_body(object_key: &str, run_id: Option<&str>, seq: Option<i64>) -> Value {
    let mut body = json!({ "object_key": object_key });
    if let Some(rid) = run_id {
        body["run_id"] = json!(rid);
    }
    if let Some(s) = seq {
        body["seq"] = json!(s);
    }
    body
}

/// Pull `upload_url` + `object_key` out of a presign response.
fn parse_presign(v: &Value) -> Result<(String, String), UploadError> {
    let upload_url = v.get("upload_url").and_then(|u| u.as_str());
    let object_key = v.get("object_key").and_then(|k| k.as_str());
    match (upload_url, object_key) {
        (Some(u), Some(k)) if !u.is_empty() && !k.is_empty() => Ok((u.to_string(), k.to_string())),
        _ => Err(UploadError::Malformed {
            stage: "presign",
            msg: "missing upload_url/object_key".to_string(),
        }),
    }
}

/// The full presign → PUT → commit handshake for one JPEG.
async fn upload(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: Option<&str>,
    seq: Option<i64>,
    bytes: Vec<u8>,
) -> Result<String, UploadError> {
    // 1. Presign.
    let resp = crate::agent::with_bearer(
        app,
        client.post(presign_url(base)).json(&presign_body(run_id, seq)),
        auth,
    )
    .send()
    .await
    .map_err(|e| UploadError::Transport { stage: "presign", msg: e.to_string() })?;
    if !resp.status().is_success() {
        return Err(UploadError::Http { stage: "presign", status: resp.status().as_u16() });
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| UploadError::Malformed { stage: "presign", msg: e.to_string() })?;
    let (upload_url, object_key) = parse_presign(&body)?;

    // 2. PUT the raw bytes. Bare client — see the module header on why no bearer
    // goes anywhere near this request.
    let resp = client
        .put(&upload_url)
        .header("content-type", CONTENT_TYPE)
        .body(bytes)
        .send()
        .await
        .map_err(|e| UploadError::Transport { stage: "put", msg: e.to_string() })?;
    if !resp.status().is_success() {
        return Err(UploadError::Http { stage: "put", status: resp.status().as_u16() });
    }

    // 3. Commit, which is what actually makes the frame visible to the console.
    let resp = crate::agent::with_bearer(
        app,
        client
            .post(commit_url(base))
            .json(&commit_body(&object_key, run_id, seq)),
        auth,
    )
    .send()
    .await
    .map_err(|e| UploadError::Transport { stage: "commit", msg: e.to_string() })?;
    if !resp.status().is_success() {
        return Err(UploadError::Http { stage: "commit", status: resp.status().as_u16() });
    }
    Ok(object_key)
}

/// Run one upload to completion and log the outcome. Never returns an error:
/// this is the point past which a failure stops mattering to anyone but the log.
async fn upload_logged(
    app: AppHandle,
    client: reqwest::Client,
    base: String,
    auth: String,
    run_id: Option<String>,
    seq: Option<i64>,
    bytes: Vec<u8>,
    what: &'static str,
) {
    match upload(&app, &client, &base, &auth, run_id.as_deref(), seq, bytes).await {
        Ok(key) => eprintln!("[shots] {what} uploaded → {key}"),
        Err(e) => {
            if let Some(status) = e.auth_rejection() {
                crate::device::note_rejection(&app, status, "screenshots");
            }
            eprintln!("[shots] {what} upload failed: {e}");
        }
    }
}

/// Mirror one RUN screenshot to object storage, without blocking the caller.
///
/// `jpeg_base64` is the SAME string that goes into the model's image block and
/// onto local disk — already downscaled to the vision budget by `capture`
/// (`CU_VISION_EDGE`, ~1024px). Uploading that rather than a fresh
/// full-resolution capture is deliberate twice over: it is what the model
/// actually acted on (so the console shows the evidence, not a re-enactment),
/// and it is a fraction of the bytes.
///
/// Returns immediately. If the in-flight ceiling is reached the frame is dropped
/// here, BEFORE the base64 is decoded, so a saturated uploader costs the agent
/// loop one atomic load per frame.
pub(crate) fn enqueue_run_shot(
    app: &AppHandle,
    client: &reqwest::Client,
    base: &str,
    auth: &str,
    run_id: &str,
    seq: i64,
    jpeg_base64: &str,
) {
    let Some(slot) = Slot::claim(&RUN_UPLOADS, MAX_RUN_UPLOADS) else {
        let n = RUN_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("[shots] uploads saturated; dropped run frame seq {seq} ({n} dropped total)");
        return;
    };
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let bytes = match B64.decode(jpeg_base64) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[shots] run frame seq {seq}: base64 decode failed: {e}");
            return;
        }
    };
    let (app, client) = (app.clone(), client.clone());
    let (base, auth, run_id) = (base.to_string(), auth.to_string(), run_id.to_string());
    tauri::async_runtime::spawn(async move {
        let _slot = slot; // held until the upload finishes, then released
        upload_logged(app, client, base, auth, Some(run_id), Some(seq), bytes, "run frame").await;
    });
}

/// Capture this machine's screen right now and upload it with NO run id, for the
/// console's "what is this machine looking at" button.
///
/// Safe to call while a run is in flight, and that is the interesting case:
///   * it takes a plain `capture::take_screenshot`, which reads the monitor and
///     touches nothing — no `ComputerState`, so the run's coordinate scaling
///     (`set_screenshot_size`) is untouched, and no `AgentState`/`RunLease`, so
///     it cannot be mistaken for a second run or block the loop's locks;
///   * it passes no `run_id` and consumes no `shot_seq`, so an operator's
///     curiosity never inserts a phantom frame into a run's timeline;
///   * it uses its own in-flight slot, so it neither starves the run's uploads
///     nor is starved by them.
///
/// The capture itself is the one place the two flows meet — both read the same
/// screen — and reading is not a mutation.
pub(crate) fn spawn_snapshot(app: &AppHandle, base: &str, auth: &str) {
    let Some(slot) = Slot::claim(&SNAPSHOT_UPLOADS, 1) else {
        eprintln!("[shots] snapshot already uploading; ignoring this request");
        return;
    };
    let (app, base, auth) = (app.clone(), base.to_string(), auth.to_string());
    tauri::async_runtime::spawn(async move {
        let _slot = slot;
        let cap = match crate::capture::take_screenshot() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[shots] snapshot capture failed: {e}");
                return;
            }
        };
        use base64::{engine::general_purpose::STANDARD as B64, Engine};
        let bytes = match B64.decode(&cap.jpeg_base64) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[shots] snapshot: base64 decode failed: {e}");
                return;
            }
        };
        // A fresh client rather than the agent's: a snapshot can arrive on a
        // machine with no run in flight, so there is no loop-owned client to
        // borrow, and connection reuse across a once-in-a-while request buys
        // nothing.
        let client = reqwest::Client::new();
        upload_logged(app, client, base, auth, None, None, bytes, "snapshot").await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_do_not_double_the_slash() {
        assert_eq!(
            presign_url("http://localhost:8000/"),
            "http://localhost:8000/screenshots/presign"
        );
        assert_eq!(
            commit_url("https://api.example.com"),
            "https://api.example.com/screenshots/commit"
        );
    }

    /// An idle-machine snapshot has no run and no sequence; those keys must be
    /// ABSENT rather than null, which is the difference between "not part of a
    /// run" and "part of a run we failed to name".
    #[test]
    fn snapshot_bodies_omit_run_and_seq() {
        let b = presign_body(None, None);
        assert_eq!(b["content_type"], "image/jpeg");
        assert!(b.get("run_id").is_none());
        assert!(b.get("seq").is_none());

        let c = commit_body("shots/abc.jpg", None, None);
        assert_eq!(c["object_key"], "shots/abc.jpg");
        assert!(c.get("run_id").is_none());
        assert!(c.get("seq").is_none());
    }

    #[test]
    fn run_bodies_carry_run_and_seq() {
        let b = presign_body(Some("run-1"), Some(7));
        assert_eq!(b["run_id"], "run-1");
        assert_eq!(b["seq"], 7);
        let c = commit_body("k", Some("run-1"), Some(7));
        assert_eq!(c["object_key"], "k");
        assert_eq!(c["run_id"], "run-1");
        assert_eq!(c["seq"], 7);
    }

    #[test]
    fn presign_response_parses_and_rejects_junk() {
        let ok = json!({"upload_url": "https://s3/put?sig=x", "object_key": "a/b.jpg",
                        "expires_at": "2026-01-01T00:00:00Z"});
        assert_eq!(
            parse_presign(&ok).unwrap(),
            ("https://s3/put?sig=x".to_string(), "a/b.jpg".to_string())
        );
        assert!(parse_presign(&json!({"upload_url": "u"})).is_err());
        assert!(parse_presign(&json!({"upload_url": "", "object_key": "k"})).is_err());
    }

    /// The ceiling holds, and permits come back when the `Slot` drops — the
    /// property that a leaked permit would break permanently.
    #[test]
    fn in_flight_ceiling_holds_and_releases() {
        static C: AtomicUsize = AtomicUsize::new(0);
        let a = Slot::claim(&C, 2).expect("first");
        let b = Slot::claim(&C, 2).expect("second");
        assert!(Slot::claim(&C, 2).is_none(), "third must be refused");
        drop(a);
        let c = Slot::claim(&C, 2).expect("slot freed by drop");
        drop(b);
        drop(c);
        assert_eq!(C.load(Ordering::Acquire), 0, "all permits returned");
    }

    #[test]
    fn a_single_slot_serialises() {
        static C: AtomicUsize = AtomicUsize::new(0);
        let s = Slot::claim(&C, 1).expect("first");
        assert!(Slot::claim(&C, 1).is_none());
        drop(s);
        assert!(Slot::claim(&C, 1).is_some());
    }

    /// Only the legs we authenticate can report a dead enrollment. A 403 on the
    /// presigned PUT is an expired signature — routing it to `note_rejection`
    /// would tell an operator their worker had been un-enrolled when it had not.
    #[test]
    fn only_authenticated_legs_signal_a_rejection() {
        assert_eq!(
            UploadError::Http { stage: "presign", status: 401 }.auth_rejection(),
            Some(401)
        );
        assert_eq!(
            UploadError::Http { stage: "commit", status: 403 }.auth_rejection(),
            Some(403)
        );
        assert_eq!(
            UploadError::Http { stage: "put", status: 403 }.auth_rejection(),
            None
        );
        assert_eq!(
            UploadError::Http { stage: "presign", status: 500 }.auth_rejection(),
            None
        );
        assert_eq!(
            UploadError::Transport { stage: "presign", msg: "dns".into() }.auth_rejection(),
            None
        );
    }

    #[test]
    fn errors_name_the_leg_that_failed() {
        assert_eq!(
            UploadError::Http { stage: "put", status: 403 }.to_string(),
            "put: HTTP 403"
        );
        assert_eq!(
            UploadError::Transport { stage: "commit", msg: "timed out".into() }.to_string(),
            "commit: request failed: timed out"
        );
    }
}
