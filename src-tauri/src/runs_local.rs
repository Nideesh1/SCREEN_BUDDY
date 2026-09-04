//! runs_local.rs — what this machine remembers about its own past runs.
//!
//! WHY this exists: an enrolled worker cannot read run history back from the
//! backend — its device token never enters the webview, and the run-history
//! routes refuse device tokens by design (a compromised worker must not replay
//! the fleet's records). So the only past a worker can show is the one already
//! on its own disk:
//!
//!   app_data_dir/runs/<run_id>/<seq>.jpg     every frame, written per-shot by
//!                                            `agent::runs_save_screenshot_local`
//!   app_data_dir/runs/<run_id>/outcome.json  how the run ended, appended once
//!                                            at finalize (this module writes it)
//!
//! The frames were always there; `outcome.json` is new. Before it, the only
//! durable trace of a finished run was the frame directory itself — the outcome
//! lived solely in `channel::note_run_outcome`'s in-memory slot and died with
//! the app. Writing one tiny JSON file where every terminal path already
//! funnels (`agent::runs_finalize`) makes "how did it end" survive a restart.
//!
//! Honesty contract, mirrored by the UI: this is the machine's OWN record, not
//! backend-grade metadata. Timestamps are file mtimes (first frame ≈ started,
//! outcome/last frame ≈ finished), a run older than `outcome.json`'s
//! introduction has no outcome at all, and the authoritative history stays in
//! the operator's console.
//!
//! Everything here is best-effort: `write_outcome` can never fail a run (a
//! full disk loses a breadcrumb, not the work), and the listing skips
//! unreadable directories rather than erroring the whole card.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const RUNS_DIR: &str = "runs";
const OUTCOME_FILE: &str = "outcome.json";

/// Newest-first cap on the listing. This is a glanceable card on the Machine
/// screen, not an archive browser; 50 rows is more history than a fleet node
/// keeps relevant, and the cap bounds the scan's stat() count.
const LIST_CAP: usize = 50;

/// Cap on frame paths returned for one expanded run. A pathological run can
/// hold thousands of frames; the card's thumbnail strip is a skim, and the cap
/// keeps the IPC payload and the DOM sane. Evenly-spaced sampling would hide
/// the gap; a hard prefix cap at least says "first N" honestly.
const FRAMES_CAP: usize = 200;

/// What `outcome.json` holds — the terminal facts `runs_finalize` knows at the
/// moment a run ends. Field names are the file format; a future reader must
/// keep parsing old files, so additions belong behind `serde(default)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub run_id: String,
    /// "completed" | "failed" | "cancelled" — whatever finalize was told.
    pub status: String,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub num_steps: i64,
    /// RFC3339 UTC, written at finalize time.
    pub finished_at: String,
}

/// One row of the "Recent runs (local)" card, exactly as the webview needs it.
/// Frame paths are ABSOLUTE local paths for the asset protocol
/// (`convertFileSrc`), the same way run events carry `screenshot_local` paths.
#[derive(Debug, Clone, Serialize)]
pub struct LocalRun {
    pub run_id: String,
    /// Earliest frame mtime (≈ when the first screenshot landed), RFC3339 UTC.
    /// None for a run that saved no frames.
    pub started_at: Option<String>,
    /// `outcome.json`'s finished_at when it exists, else the latest frame
    /// mtime — the best "when" this disk can testify to.
    pub finished_at: Option<String>,
    /// Terminal status from `outcome.json`, or None for runs that predate it
    /// (or died without finalizing). The UI must not dress None up as anything.
    pub outcome: Option<String>,
    pub error_message: Option<String>,
    pub frame_count: usize,
    pub first_frame: Option<String>,
    pub last_frame: Option<String>,
}

fn runs_root(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join(RUNS_DIR))
}

// ---- the finalize-time append ----------------------------------------------

/// Write `runs/<run_id>/outcome.json`. Called from `agent::runs_finalize` —
/// the single funnel every terminal path goes through — right beside
/// `note_run_outcome`, so the durable record and the in-memory one can never
/// disagree about a run this build finalized.
///
/// Best-effort BY CONTRACT: every failure is a log line. This runs on the run's
/// own task at the moment it ends, and nothing about remembering a run may be
/// allowed to fail it. Synchronous like `runs_save_screenshot_local`: one tiny
/// file, no network.
pub(crate) fn write_outcome(
    app: &AppHandle,
    run_id: &str,
    status: &str,
    error_message: Option<&str>,
    num_steps: i64,
) {
    let root = match runs_root(app) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[runs] outcome save: {e}");
            return;
        }
    };
    let dir = root.join(run_id);
    // A run can end before its first frame (an immediate refusal); the outcome
    // is still worth remembering, so create the directory rather than skip.
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[runs] outcome save: create_dir_all failed: {e}");
        return;
    }
    let record = OutcomeRecord {
        run_id: run_id.to_string(),
        status: status.to_string(),
        error_message: error_message.map(str::to_string),
        num_steps,
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    let json = match serde_json::to_vec_pretty(&record) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[runs] outcome save: serialize failed: {e}");
            return;
        }
    };
    if let Err(e) = fs::write(dir.join(OUTCOME_FILE), json) {
        eprintln!("[runs] outcome save: write failed: {e}");
    }
}

// ---- pure logic (unit-tested) ------------------------------------------------

/// The frame sequence number of a `<seq>.jpg` filename, or None for anything
/// else in the directory (outcome.json included). Strict on purpose: only what
/// `runs_save_screenshot_local` writes counts as a frame, so a stray file can
/// never inflate a run's step count or become its "last frame".
fn frame_seq(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".jpg")?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<i64>().ok()
}

/// Parse an `outcome.json` body, tolerating garbage: a truncated or hand-edited
/// file yields None and the run simply lists without an outcome, exactly like a
/// run that predates the file.
fn parse_outcome(bytes: &[u8]) -> Option<OutcomeRecord> {
    serde_json::from_slice(bytes).ok()
}

/// A run_id is safe to join under the runs root: non-empty ASCII alphanumerics
/// and dashes only (run ids are UUIDs everywhere in this app). Anything else is
/// refused before it can name a path — same posture as `artifacts::safe_id`.
fn safe_run_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn mtime_rfc3339(meta: &fs::Metadata) -> Option<String> {
    let t = meta.modified().ok()?;
    Some(DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true))
}

// ---- the scan ----------------------------------------------------------------

/// Read one run directory into a row, or None when it holds nothing this module
/// recognises (no frames AND no outcome — e.g. a dir another feature left).
/// Cheap by design: names, mtimes, and one tiny JSON read; never image bytes.
fn scan_run_dir(dir: &Path, run_id: &str) -> Option<LocalRun> {
    let entries = fs::read_dir(dir).ok()?;

    // Track only the extremes: the earliest frame dates the run, the lowest /
    // highest seq name its first and last frames. Frames are numbered by the
    // agent's shot counter, so seq order IS chronological order.
    let mut frame_count = 0usize;
    let mut min_seq: Option<(i64, PathBuf)> = None;
    let mut max_seq: Option<(i64, PathBuf)> = None;
    let mut earliest_mtime: Option<String> = None;
    let mut latest_mtime: Option<String> = None;
    let mut outcome: Option<OutcomeRecord> = None;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == OUTCOME_FILE {
            if let Ok(bytes) = fs::read(entry.path()) {
                outcome = parse_outcome(&bytes);
            }
            continue;
        }
        let Some(seq) = frame_seq(name) else { continue };
        frame_count += 1;
        if min_seq.as_ref().map_or(true, |(s, _)| seq < *s) {
            min_seq = Some((seq, entry.path()));
        }
        if max_seq.as_ref().map_or(true, |(s, _)| seq > *s) {
            max_seq = Some((seq, entry.path()));
        }
        if let Ok(meta) = entry.metadata() {
            if let Some(ts) = mtime_rfc3339(&meta) {
                // RFC3339 UTC with 'Z' is fixed-width, so lexical == chronological
                // (the same property artifact_list sorts by).
                if earliest_mtime.as_ref().map_or(true, |e| ts < *e) {
                    earliest_mtime = Some(ts.clone());
                }
                if latest_mtime.as_ref().map_or(true, |l| ts > *l) {
                    latest_mtime = Some(ts);
                }
            }
        }
    }

    if frame_count == 0 && outcome.is_none() {
        return None;
    }

    let finished_at = outcome
        .as_ref()
        .map(|o| o.finished_at.clone())
        .or_else(|| latest_mtime.clone());
    Some(LocalRun {
        run_id: run_id.to_string(),
        started_at: earliest_mtime,
        finished_at,
        outcome: outcome.as_ref().map(|o| o.status.clone()),
        error_message: outcome.and_then(|o| o.error_message),
        frame_count,
        first_frame: min_seq.map(|(_, p)| p.to_string_lossy().to_string()),
        last_frame: max_seq.map(|(_, p)| p.to_string_lossy().to_string()),
    })
}

fn list_blocking(app: AppHandle) -> Result<Vec<LocalRun>, String> {
    let root = runs_root(&app)?;
    // A machine that has never run anything has no runs dir at all; that is an
    // empty history, not an error.
    let Ok(entries) = fs::read_dir(&root) else { return Ok(Vec::new()) };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(run_id) = name.to_str() else { continue };
        if let Some(run) = scan_run_dir(&entry.path(), run_id) {
            out.push(run);
        }
    }
    // Newest first; runs with no timestamp at all sink to the end (None < Some,
    // and the sort is descending).
    out.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    out.truncate(LIST_CAP);
    Ok(out)
}

// ---- commands ------------------------------------------------------------------

/// This machine's local run record, newest first, capped at 50. Off the main
/// thread because the scan stats every frame of every run — cheap per file, but
/// a long-lived worker can hold tens of thousands of frames.
#[tauri::command]
pub async fn local_runs(app: AppHandle) -> Result<Vec<LocalRun>, String> {
    tauri::async_runtime::spawn_blocking(move || list_blocking(app))
        .await
        .map_err(|e| format!("local runs task panicked: {e}"))?
}

/// Every frame path of one local run, in shot order, capped. The list command
/// deliberately returns only first/last so 50 rows stay one small payload; this
/// is the follow-up an expanded row makes for the run someone is looking at.
#[tauri::command]
pub async fn local_run_frames(app: AppHandle, run_id: String) -> Result<Vec<String>, String> {
    if !safe_run_id(&run_id) {
        return Err("invalid run id".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let dir = runs_root(&app)?.join(&run_id);
        let Ok(entries) = fs::read_dir(&dir) else { return Ok(Vec::new()) };
        let mut frames: Vec<(i64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let seq = frame_seq(name.to_str()?)?;
                Some((seq, e.path()))
            })
            .collect();
        frames.sort_by_key(|(seq, _)| *seq);
        frames.truncate(FRAMES_CAP);
        Ok(frames.into_iter().map(|(_, p)| p.to_string_lossy().to_string()).collect())
    })
    .await
    .map_err(|e| format!("local frames task panicked: {e}"))?
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Only what the agent writes counts as a frame: `<digits>.jpg`, nothing
    /// else. A stray file must never become a run's "last frame".
    #[test]
    fn frame_seq_accepts_only_the_agent_naming() {
        assert_eq!(frame_seq("0.jpg"), Some(0));
        assert_eq!(frame_seq("12.jpg"), Some(12));
        assert_eq!(frame_seq("007.jpg"), Some(7));
        assert_eq!(frame_seq("12.png"), None);
        assert_eq!(frame_seq(".jpg"), None);
        assert_eq!(frame_seq("outcome.json"), None);
        assert_eq!(frame_seq("-1.jpg"), None, "a sign is not a digit");
        assert_eq!(frame_seq("12.jpg.tmp"), None);
        assert_eq!(frame_seq("a12.jpg"), None);
    }

    #[test]
    fn parse_outcome_reads_what_write_outcome_writes() {
        let json = br#"{
            "run_id": "r1",
            "status": "completed",
            "error_message": null,
            "num_steps": 9,
            "finished_at": "2026-09-03T10:00:00Z"
        }"#;
        let o = parse_outcome(json).expect("valid record parses");
        assert_eq!(o.status, "completed");
        assert_eq!(o.num_steps, 9);
        assert_eq!(o.error_message, None);
    }

    /// Optional fields default rather than fail: an older (or future) file
    /// missing them still yields its status.
    #[test]
    fn parse_outcome_tolerates_missing_optional_fields() {
        let json = br#"{"run_id": "r1", "status": "failed", "finished_at": "2026-01-01T00:00:00Z"}"#;
        let o = parse_outcome(json).expect("minimal record parses");
        assert_eq!(o.status, "failed");
        assert_eq!(o.num_steps, 0);
    }

    /// Garbage on disk means "no outcome", never an error: the run lists like
    /// one that predates outcome.json.
    #[test]
    fn parse_outcome_swallows_garbage() {
        assert!(parse_outcome(b"").is_none());
        assert!(parse_outcome(b"{ truncated").is_none());
        assert!(parse_outcome(b"[1,2,3]").is_none());
    }

    /// The traversal guard: run ids are UUIDs, so anything that could escape
    /// the runs root — separators, dots, empties — is refused outright.
    #[test]
    fn safe_run_id_refuses_path_shapes() {
        assert!(safe_run_id("03b699fa-7f12-42e0-9625-94461be492de"));
        assert!(!safe_run_id(""));
        assert!(!safe_run_id(".."));
        assert!(!safe_run_id("a/b"));
        assert!(!safe_run_id("a\\b"));
        assert!(!safe_run_id("a.b"));
    }
}
