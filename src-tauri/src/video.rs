//! video.rs — local video → candidate-frame extraction for set creation.
//!
//! One new front door to a pinned set: the user drops a short video and this
//! pipeline proposes good candidate frames that the UI reviews. It is purely
//! additive — the selected frames become a NORMAL pinned set via the exact same
//! `pinned_create` write path hand-picked images use today (the frontend feeds
//! the JPEG paths this returns straight back into `pinned_create`). The video
//! bytes NEVER leave the machine: everything here is local ffmpeg + pure-Rust
//! image math. No LLM, no OpenCV, no network.
//!
//! Pipeline (6 stages, mirrors the spec):
//!   1. Probe        — ffprobe metadata (duration/fps/resolution/codec).
//!   2. Sample       — ffmpeg scene-change select='gt(scene,0.4)'; fall back to
//!                     uniform 1 frame / 1.5s when a static video yields < 5 hits.
//!   3. Dedupe       — pure-Rust dHash perceptual hash, drop within Hamming 10.
//!   4. Quality      — Laplacian-variance sharpness; drop near-black/near-white.
//!   5. Select top-K — rank by sharpness, take <= K, keep temporal order.
//!   6. Downscale    — long edge <= 1568px, JPEG q85 to a temp staging dir;
//!                     return paths + base64 thumbnails.
//!
//! ffmpeg is bundled as a Tauri sidecar (see `externalBin` in tauri.conf.json,
//! the real GPL static binaries in `src-tauri/binaries/`, and their SOURCE.md).
//! `resolve_bin` uses ONLY the bundled sidecar sitting next to the app
//! executable — there is deliberately NO `$PATH` fallback: downloaders are not
//! assumed to have ffmpeg installed, and a missing sidecar returns a clear error
//! rather than silently using some other ffmpeg.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::{DynamicImage, GenericImageView};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Emitter, Manager};

// ---- tunables (from the spec) ---------------------------------------------

// ── Content-adaptive sampler ──
// We decode cheaply at a modest stride and keep a frame whenever the picture has
// drifted far enough SINCE THE LAST KEPT FRAME (cumulative perceptual change),
// not frame-to-frame. That's what catches a slow pan (each step is tiny, but the
// change accumulates) which ffmpeg's instantaneous `scene` score misses. This
// replaces both the old scene/uniform sampling AND the separate dedupe stage:
// a keeper is by construction already different-enough from the previous one.

/// Decode stride for hashing (frames/sec). Cheap — we only hash these, and only
/// re-encode the handful we keep.
const DECODE_FPS: u32 = 4;
/// Keep a new frame once the 64-bit dHash has drifted this many bits from the
/// last kept frame. Lower = more keepers. Tuned so a ~20s pan yields ~15–30.
const CHANGE_THRESHOLD: u32 = 10;
/// When a keep triggers, save the SHARPEST frame within ±this many decoded
/// frames of the trigger (avoids saving a motion-blurred mid-pan frame).
const SHARP_WINDOW: usize = 2;
/// Absolute Laplacian-variance floor below which a keeper is "clearly blurry"
/// garbage and dropped in the light final pass. Deliberately low — we over-serve
/// and let the human prune, so only obvious blur is culled.
const BLUR_FLOOR: f32 = 3.0;
/// Hard cap on decoded frames (safety for long videos): 1200 = ~5 min at 4 fps.
const MAX_CANDIDATES: usize = 1200;
/// Mean-luma extremes (0-255) that mark a near-black / near-white transition.
const LUMA_DARK: f32 = 14.0;
const LUMA_BRIGHT: f32 = 242.0;
/// Output encode: long edge and JPEG quality.
const MAX_EDGE: u32 = 1568;
const JPEG_Q: u8 = 85;
/// Thumbnail long edge for the review grid.
const THUMB_EDGE: u32 = 320;
/// Absolute ceiling on frames served to the grid (over-serve, user prunes).
const MAX_OUTPUT: usize = 30;

/// Progress event the UI subscribes to (matches the `agent://*` convention).
pub const EV_VIDEO_PROGRESS: &str = "agent://video_extract_progress";

/// One proposed frame returned to the review grid. `path` is a real JPEG on
/// disk in the staging dir — the frontend feeds the chosen paths straight into
/// `pinned_create`, identical to hand-picked images.
#[derive(Debug, Serialize)]
pub struct FrameCandidate {
    pub path: String,
    pub ts_ms: u64,
    pub sharpness: f32,
    pub thumb_b64: String,
}

fn emit_pct(app: &AppHandle, pct: u32) {
    let _ = app.emit(EV_VIDEO_PROGRESS, serde_json::json!({ "pct": pct }));
}

/// Resolve an ffmpeg-family binary to the bundled sidecar sitting next to the
/// app executable (Tauri copies `externalBin` there, stripped of the target
/// triple). There is NO `$PATH` fallback: if the sidecar is missing we return a
/// clear error so the caller never silently runs some unknown ffmpeg.
fn resolve_bin(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate app executable: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "app executable has no parent dir".to_string())?;
    let candidate = dir.join(name);
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(format!(
        "bundled `{name}` sidecar not found next to the app ({}). \
         The ffmpeg/ffprobe sidecar binaries must be present in src-tauri/binaries/ \
         (see src-tauri/binaries/SOURCE.md).",
        candidate.display()
    ))
}

/// Per-run staging dir under app data: `app_data_dir/video_staging/<rand>`.
fn staging_dir(app: &AppHandle) -> Result<PathBuf, String> {
    use rand::RngCore;
    let mut id = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut id);
    let name = id.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join("video_staging")
        .join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create staging dir: {e}"))?;
    Ok(dir)
}

/// Stage 1 — probe. Returns (duration_s, note). Rejects unreadable input early;
/// warns (non-fatally) on very long / very large inputs.
fn probe(ffprobe: &Path, path: &str) -> Result<(f32, Option<String>), String> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .map_err(|e| format!("ffprobe spawn failed ({}): {e}", ffprobe.display()))?;
    if !out.status.success() {
        return Err("ffprobe could not read this file (unsupported codec or corrupt).".into());
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parse ffprobe json: {e}"))?;

    let duration = meta
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0);

    // Find the first video stream for a resolution sanity warning.
    let mut long_edge = 0u64;
    if let Some(streams) = meta.get("streams").and_then(|s| s.as_array()) {
        for s in streams {
            if s.get("codec_type").and_then(|c| c.as_str()) == Some("video") {
                let w = s.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
                let h = s.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                long_edge = w.max(h);
                break;
            }
        }
    }

    let mut warn = None;
    if duration > 600.0 {
        warn = Some(format!(
            "Long video ({:.0}s) — only the first scene hits are used.",
            duration
        ));
    } else if long_edge > 1080 {
        warn = Some(format!("High resolution ({long_edge}px) — extraction may be slow."));
    }
    Ok((duration, warn))
}

/// Run ffmpeg with a `-vf` filter that ends in `showinfo`, writing frames to
/// `stage/cand_%05d.png`, and parse `pts_time` from stderr. Output frames map
/// 1:1 in order to the `pts_time` lines showinfo prints, so we zip them.
fn run_extract(ffmpeg: &Path, path: &str, vf: &str, stage: &Path) -> Result<Vec<(PathBuf, u64)>, String> {
    let pattern = stage.join("cand_%05d.png");
    let out = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-i",
            path,
            "-vf",
            vf,
            "-vsync",
            "vfr",
            "-frames:v",
            &MAX_CANDIDATES.to_string(),
        ])
        .arg(&pattern)
        .output()
        .map_err(|e| format!("ffmpeg spawn failed ({}): {e}", ffmpeg.display()))?;

    // ffmpeg writes showinfo diagnostics to stderr even on success.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut times_ms: Vec<u64> = Vec::new();
    for line in stderr.lines() {
        if let Some(idx) = line.find("pts_time:") {
            let rest = &line[idx + "pts_time:".len()..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num.parse::<f64>() {
                times_ms.push((secs * 1000.0) as u64);
            }
        }
    }

    // Collect produced frames in filename order.
    let mut frames: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(stage) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("png") {
                frames.push(p);
            }
        }
    }
    frames.sort();

    // Zip frames with parsed timestamps (fall back to index cadence if showinfo
    // gave fewer lines than frames, which shouldn't normally happen).
    let out = frames
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let ts = times_ms.get(i).copied().unwrap_or((i as u64) * 1000);
            (p, ts)
        })
        .collect();
    Ok(out)
}

// ---- pure-Rust image math (no OpenCV) -------------------------------------

/// 64-bit dHash: resize to 9x8 luma, compare each pixel to its right neighbor.
fn dhash(img: &DynamicImage) -> u64 {
    let small = img
        .resize_exact(9, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut hash = 0u64;
    let mut bit = 0;
    for y in 0..8 {
        for x in 0..8 {
            let left = small.get_pixel(x, y)[0];
            let right = small.get_pixel(x + 1, y)[0];
            if left > right {
                hash |= 1 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Laplacian-variance sharpness on the luma plane (higher = sharper).
fn sharpness(img: &DynamicImage) -> f32 {
    // Downscale big frames first so this stays cheap and scale-stable.
    let g = img
        .resize(512, 512, image::imageops::FilterType::Triangle)
        .to_luma8();
    let (w, h) = g.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }
    let px = |x: u32, y: u32| g.get_pixel(x, y)[0] as f32;
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut n = 0.0f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            // 4-neighbour Laplacian kernel.
            let lap = px(x - 1, y) + px(x + 1, y) + px(x, y - 1) + px(x, y + 1) - 4.0 * px(x, y);
            sum += lap as f64;
            sum_sq += (lap * lap) as f64;
            n += 1.0;
        }
    }
    if n == 0.0 {
        return 0.0;
    }
    let mean = sum / n;
    ((sum_sq / n) - mean * mean).max(0.0) as f32
}

/// Mean luma (0-255) to spot near-black / near-white transition frames.
fn mean_luma(img: &DynamicImage) -> f32 {
    let g = img
        .resize(64, 64, image::imageops::FilterType::Triangle)
        .to_luma8();
    let total: u64 = g.pixels().map(|p| p[0] as u64).sum();
    let n = (g.width() * g.height()).max(1) as f32;
    total as f32 / n
}

/// Downscale so the long edge is <= MAX_EDGE (never upscales).
fn downscale(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= MAX_EDGE {
        return img.clone();
    }
    img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3)
}

/// Encode a DynamicImage to JPEG bytes at the given quality.
fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let rgb = img.to_rgb8();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| format!("jpeg encode: {e}"))?;
    Ok(buf)
}

/// One cheaply-decoded frame: on-disk PNG + the three scalars the adaptive
/// sampler needs. We deliberately do NOT hold the DynamicImage (only re-open the
/// handful we finally keep) so memory stays flat even on long clips.
struct Decoded {
    path: PathBuf,
    ts_ms: u64,
    hash: u64,
    sharp: f32,
    luma: f32,
}

/// The content-adaptive selection (pure, so it can be unit-tested without ffmpeg
/// or an AppHandle). Given the decoded frames in temporal order, returns the
/// indices to SAVE, in temporal order.
///
/// 1. Greedy keep-on-accumulated-change: keep frame 0; walking forward, trigger
///    a new keep once the dHash has drifted `CHANGE_THRESHOLD` bits from the LAST
///    kept frame (cumulative change — catches slow pans). Reset the reference to
///    each new keeper.
/// 2. Sharpest-in-window: for each trigger, save the sharpest non-extreme-luma
///    frame within ±`SHARP_WINDOW` (dodges motion-blurred mid-pan frames).
/// 3. Light final pass: drop only clearly-blurry keepers (`BLUR_FLOOR`), then cap
///    to `MAX_OUTPUT` by even temporal subsampling (preserve whole-timeline
///    coverage, not just the front).
fn select_keepers(frames: &[Decoded], cap: usize) -> Vec<usize> {
    if frames.is_empty() {
        return Vec::new();
    }

    // (1) accumulated-change triggers.
    let mut triggers = vec![0usize];
    let mut last_hash = frames[0].hash;
    for i in 1..frames.len() {
        if (frames[i].hash ^ last_hash).count_ones() >= CHANGE_THRESHOLD {
            triggers.push(i);
            last_hash = frames[i].hash;
        }
    }

    // (2) sharpest-in-window per trigger; de-duplicate overlapping picks.
    let mut chosen: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &t in &triggers {
        let lo = t.saturating_sub(SHARP_WINDOW);
        let hi = (t + SHARP_WINDOW).min(frames.len() - 1);
        let mut best = t;
        let mut best_sharp = f32::NEG_INFINITY;
        let mut found = false;
        for j in lo..=hi {
            let f = &frames[j];
            if f.luma <= LUMA_DARK || f.luma >= LUMA_BRIGHT {
                continue; // skip near-black / near-white transition frames
            }
            if f.sharp > best_sharp {
                best_sharp = f.sharp;
                best = j;
                found = true;
            }
        }
        // If every frame in the window was an extreme-luma transition, fall back
        // to the trigger frame itself rather than dropping the keep entirely.
        let pick = if found { best } else { t };
        if seen.insert(pick) {
            chosen.push(pick);
        }
    }
    chosen.sort_unstable(); // index order == temporal order

    // (3a) drop clearly-blurry keepers, but never everything (best-effort floor).
    let filtered: Vec<usize> = chosen
        .iter()
        .copied()
        .filter(|&j| frames[j].sharp >= BLUR_FLOOR)
        .collect();
    let mut kept = if filtered.is_empty() { chosen } else { filtered };

    // (3b) cap by even temporal subsampling.
    if kept.len() > cap && cap > 0 {
        let step = kept.len() as f32 / cap as f32;
        let mut capped = Vec::with_capacity(cap);
        for n in 0..cap {
            capped.push(kept[((n as f32) * step) as usize]);
        }
        capped.dedup();
        kept = capped;
    }
    kept
}

/// The blocking worker (ffmpeg is slow + the image math is CPU-bound).
fn extract_blocking(
    app: AppHandle,
    path: String,
    target_k: usize,
) -> Result<Vec<FrameCandidate>, String> {
    let ffmpeg = resolve_bin("ffmpeg")?;
    let ffprobe = resolve_bin("ffprobe")?;
    // Over-serve: the user prunes in the grid, so err toward MORE frames.
    let cap = target_k.clamp(1, MAX_OUTPUT);

    // Stage 1 — probe.
    emit_pct(&app, 2);
    let (_duration, _warn) = probe(&ffprobe, &path)?;
    let stage = staging_dir(&app)?;

    // Stage 2 — decode cheaply at DECODE_FPS for hashing (not the final frames).
    emit_pct(&app, 8);
    let vf = format!("fps={DECODE_FPS},showinfo");
    let raw = run_extract(&ffmpeg, &path, &vf, &stage)?;
    if raw.is_empty() {
        return Err("No frames could be extracted from this video.".into());
    }

    // Stage 3 — compute dHash + sharpness + mean-luma per decoded frame.
    let total = raw.len().max(1);
    let mut frames: Vec<Decoded> = Vec::with_capacity(raw.len());
    for (i, (frame_path, ts_ms)) in raw.iter().enumerate() {
        if let Ok(img) = image::open(frame_path) {
            frames.push(Decoded {
                path: frame_path.clone(),
                ts_ms: *ts_ms,
                hash: dhash(&img),
                sharp: sharpness(&img),
                luma: mean_luma(&img),
            });
        }
        // 8 → 62% across the decode/hash pass (the bulk of the work).
        emit_pct(&app, 8 + ((i + 1) * 54 / total) as u32);
    }
    if frames.is_empty() {
        return Err("Frames were extracted but none could be decoded.".into());
    }

    // Stage 4 — content-adaptive keeper selection.
    let keepers = select_keepers(&frames, cap);
    if keepers.is_empty() {
        return Err("No distinct frames survived selection.".into());
    }
    emit_pct(&app, 66);

    // Stage 5 — re-open each keeper, downscale + encode JPEG, build a thumbnail.
    let mut out = Vec::with_capacity(keepers.len());
    let n = keepers.len().max(1);
    for (i, &j) in keepers.iter().enumerate() {
        let f = &frames[j];
        let img = match image::open(&f.path) {
            Ok(im) => im,
            Err(_) => continue,
        };
        let full = downscale(&img);
        let jpeg = encode_jpeg(&full, JPEG_Q)?;
        let out_name = format!("frame_{:03}_{}ms.jpg", i, f.ts_ms);
        let out_path = stage.join(&out_name);
        std::fs::write(&out_path, &jpeg).map_err(|e| format!("write frame: {e}"))?;

        let thumb = img.resize(THUMB_EDGE, THUMB_EDGE, image::imageops::FilterType::Triangle);
        let thumb_bytes = encode_jpeg(&thumb, 78)?;
        let thumb_b64 = format!("data:image/jpeg;base64,{}", B64.encode(&thumb_bytes));

        out.push(FrameCandidate {
            path: out_path.to_string_lossy().into_owned(),
            ts_ms: f.ts_ms,
            sharpness: f.sharp,
            thumb_b64,
        });
        emit_pct(&app, 66 + ((i + 1) * 34 / n) as u32);
    }

    // Clean up every intermediate decoded PNG; the returned JPEGs are separate
    // files (copied into the pinned set on Save; the temp dir is disposable).
    for (frame_path, _) in &raw {
        let _ = std::fs::remove_file(frame_path);
    }

    emit_pct(&app, 100);
    Ok(out)
}

/// Extract candidate frames from a local video for set creation. Runs the whole
/// pipeline on a blocking task and reports progress via `EV_VIDEO_PROGRESS`.
/// 100% local — the video bytes never leave the machine.
#[tauri::command]
pub async fn extract_frames_from_video(
    app: AppHandle,
    path: String,
    target_k: usize,
) -> Result<Vec<FrameCandidate>, String> {
    tauri::async_runtime::spawn_blocking(move || extract_blocking(app, path, target_k))
        .await
        .map_err(|e| format!("extraction task panicked: {e}"))?
}

// ---- tests -----------------------------------------------------------------
// These exercise the real bundled sidecar (no mocks): resolution next to the
// executable (with NO $PATH fallback) and the ffmpeg-driven sample stage.

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed GPL sidecar binary for the host arch (used to both prove
    /// resolution and to drive the real pipeline).
    #[cfg(target_arch = "aarch64")]
    const HOST_FFMPEG: &str = "ffmpeg-aarch64-apple-darwin";
    #[cfg(target_arch = "x86_64")]
    const HOST_FFMPEG: &str = "ffmpeg-x86_64-apple-darwin";

    fn bundled(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries").join(name)
    }

    /// resolve_bin returns the binary sitting next to the current exe, and
    /// returns an Err (NOT a $PATH fallback) when it is absent.
    #[test]
    fn resolve_bin_uses_sidecar_next_to_exe_and_has_no_path_fallback() {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();

        // A name that surely exists on $PATH ("ls") but NOT next to the exe must
        // still error — proving there is no $PATH fallback.
        assert!(
            resolve_bin("ls").is_err(),
            "resolve_bin must not fall back to $PATH"
        );

        // Drop a real binary next to the exe under the stripped name and confirm
        // resolve_bin returns exactly that path.
        let target = dir.join("sidecar_probe_ffmpeg");
        std::fs::copy(bundled(HOST_FFMPEG), &target).unwrap();
        let resolved = resolve_bin("sidecar_probe_ffmpeg").unwrap();
        assert_eq!(resolved, target);
        let _ = std::fs::remove_file(&target);
    }

    /// End-to-end proof that the committed sidecar binary actually drives the
    /// pipeline: generate a tiny clip with it, then probe + sample real frames.
    #[test]
    fn bundled_ffmpeg_drives_probe_and_sample() {
        let ffmpeg = bundled(HOST_FFMPEG);
        let ffprobe = bundled(&HOST_FFMPEG.replace("ffmpeg", "ffprobe"));
        assert!(ffmpeg.is_file() && ffprobe.is_file(), "sidecars committed");

        let tmp = std::env::temp_dir().join(format!("sb_vid_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let clip = tmp.join("in.mp4");

        // 3s clip, three distinct colored segments (gives scene changes).
        let gen = Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "color=c=red:s=160x120:d=1",
                "-f", "lavfi", "-i", "color=c=green:s=160x120:d=1",
                "-f", "lavfi", "-i", "color=c=blue:s=160x120:d=1",
                "-filter_complex", "[0][1][2]concat=n=3:v=1:a=0",
            ])
            .arg(&clip)
            .output()
            .expect("spawn bundled ffmpeg");
        assert!(gen.status.success(), "bundled ffmpeg generated a clip");

        let clip_s = clip.to_string_lossy().into_owned();
        let (dur, _) = probe(&ffprobe, &clip_s).expect("bundled ffprobe reads metadata");
        assert!(dur > 2.5, "duration parsed (~3s), got {dur}");

        // Uniform sample is deterministic for this synthetic clip.
        let frames = run_extract(&ffmpeg, &clip_s, "fps=1,showinfo", &tmp)
            .expect("bundled ffmpeg samples frames");
        assert!(!frames.is_empty(), "got candidate frames + timestamps");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The core fix, deterministic (no ffmpeg): a SLOW drift where each decoded
    /// frame differs from the previous by a single bit — instantaneous change is
    /// tiny (a `scene` score would see ~nothing), but it ACCUMULATES. The
    /// keep-on-accumulated-change walk must yield a steady stream of keepers, not
    /// collapse to one. hash_i = low `i` bits set, so Hamming(i, j) == |i - j|.
    #[test]
    fn select_keepers_catches_slow_drift() {
        let n = 60usize;
        let frames: Vec<Decoded> = (0..n)
            .map(|i| Decoded {
                path: PathBuf::from(format!("f{i}")),
                ts_ms: (i as u64) * 250, // 4 fps
                hash: if i >= 64 { u64::MAX } else { (1u64 << i) - 1 },
                sharp: 100.0,  // above BLUR_FLOOR
                luma: 128.0,   // mid-tone, never an extreme
            })
            .collect();

        let keepers = select_keepers(&frames, MAX_OUTPUT);
        // With CHANGE_THRESHOLD=10 over 60 single-bit steps → keeps at
        // 0,10,20,30,40,50 ≈ 6. The point: many, spread across time, NOT 1.
        eprintln!("[select_keepers] slow-drift keepers = {}", keepers.len());
        assert!(
            keepers.len() >= 4 && keepers.len() <= 8,
            "slow drift should yield ~6 keepers, got {}",
            keepers.len()
        );
        assert!(keepers.windows(2).all(|w| w[0] < w[1]), "temporal order");
    }

    /// End-to-end keeper count through the REAL bundled sidecar on a clip with
    /// gradual change (a moving test pattern). Logs the count as a sanity check
    /// that the adaptive sampler serves many frames (not the old "22s → 3").
    #[test]
    fn bundled_pipeline_keeper_count_on_gradual_clip() {
        let ffmpeg = bundled(HOST_FFMPEG);
        let tmp = std::env::temp_dir().join(format!("sb_vid_grad_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let clip = tmp.join("grad.mp4");

        // 20s "slow pan": a structured test pattern (bars of distinct luma)
        // SCROLLED horizontally, so the coarse downsampled luma — what dHash sees
        // — actually drifts over time, exactly like panning a real scene. (A
        // moving-element pattern like testsrc2 keeps a static coarse layout and
        // is a poor stand-in for a pan.)
        let gen = Command::new(&ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi", "-i", "testsrc=s=640x480:r=30:d=20",
                "-vf", "scroll=horizontal=0.006",
                "-pix_fmt", "yuv420p",
            ])
            .arg(&clip)
            .output()
            .expect("spawn bundled ffmpeg");
        assert!(gen.status.success(), "generated a slow-pan clip");

        let clip_s = clip.to_string_lossy().into_owned();
        let raw = run_extract(&ffmpeg, &clip_s, &format!("fps={DECODE_FPS},showinfo"), &tmp)
            .expect("decode at DECODE_FPS");
        assert!(!raw.is_empty(), "decoded frames");

        let frames: Vec<Decoded> = raw
            .iter()
            .filter_map(|(p, ts)| {
                image::open(p).ok().map(|img| Decoded {
                    path: p.clone(),
                    ts_ms: *ts,
                    hash: dhash(&img),
                    sharp: sharpness(&img),
                    luma: mean_luma(&img),
                })
            })
            .collect();

        let keepers = select_keepers(&frames, MAX_OUTPUT);
        eprintln!(
            "[select_keepers] 20s gradual clip: decoded {} @ {}fps → {} keepers (cap {})",
            frames.len(),
            DECODE_FPS,
            keepers.len(),
            MAX_OUTPUT
        );
        // The whole point of the tuning: WAY more than the old ~3, up to the cap.
        assert!(
            keepers.len() > 3 && keepers.len() <= MAX_OUTPUT,
            "expected many keepers (>3, <= {MAX_OUTPUT}), got {}",
            keepers.len()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
