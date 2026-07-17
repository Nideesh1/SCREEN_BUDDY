//! artifacts.rs — the persistent local media library.
//!
//! A flat, content-addressed store of user-uploaded media (images, videos,
//! PDFs, text). Upload once, keep forever: the physical bytes are copied in and
//! live on disk under the app data dir, so the originals can move or disappear
//! without breaking the library. Layout:
//!
//!   app_data_dir/artifacts/<artifact_id>/
//!       blob.<ext>    the original file, copied in verbatim
//!       thumb.jpg     320px long-edge JPEG preview (absent for text/other, or
//!                     when thumbnailing failed — the UI falls back to an icon)
//!       meta.json     the ArtifactMeta below
//!
//! `artifact_id` is the lowercase-hex SHA-256 of the file CONTENTS, which buys
//! dedup for free: re-importing the same bytes resolves to the same directory,
//! so the second import is a no-op that returns the existing meta (and keeps the
//! name the user may have since edited).
//!
//! Everything here is best-effort by design. A batch import never aborts on one
//! bad file, thumbnail generation never fails an import, and `artifact_list`
//! skips corrupt/partial directories rather than erroring the whole listing.
//!
//! Mirrors `pinned.rs` conventions throughout (safe_id traversal guard, kind_for
//! / mime_for extension routing, serde manifest, `Result<T, String>` commands).

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::{AppHandle, Manager};

const ARTIFACTS_DIR: &str = "artifacts";
const META: &str = "meta.json";
const THUMB: &str = "thumb.jpg";
/// Long edge of every generated thumbnail, and its JPEG quality.
const THUMB_EDGE: u32 = 320;
const THUMB_Q: u8 = 80;
/// Read buffer for streaming the SHA-256 (never load a whole video into RAM).
const HASH_CHUNK: usize = 64 * 1024;

/// One artifact's metadata. This EXACT shape is written to meta.json and mirrored
/// to the backend (POST /artifacts) — the field names are a shared contract, do
/// not rename them without changing the backend too.
///
///   artifact_id: lowercase-hex SHA-256 of the file contents (64 chars)
///   name:        editable display name; defaults to original_filename
///   kind:        "image" | "video" | "pdf" | "text" | "other"
///   width/height:   images only (from the FULL image, not the thumbnail)
///   duration_ms:    video only (via ffprobe; None if it couldn't be probed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub artifact_id: String,
    pub name: String,
    pub original_filename: String,
    pub kind: String,
    pub mime: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub created_at: String,
}

pub(crate) fn artifacts_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join(ARTIFACTS_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create artifacts dir: {e}"))?;
    Ok(dir)
}

/// Reject ids that could escape the artifacts root via path traversal. Ids are
/// SHA-256 hex by construction, so we can be strict: exactly 64 lowercase hex
/// chars. That subsumes the `/`, `\` and `..` checks `pinned.rs` needs for its
/// looser ids.
pub(crate) fn safe_id(id: &str) -> bool {
    id.len() == 64
        && id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Guess a MIME type from a file extension. Superset of `pinned.rs::mime_for`
/// (it adds the video container types the artifact library accepts).
fn mime_for(path: &Path) -> &'static str {
    match ext_of(path).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("heic") => "image/heic",
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") | Some("markdown") => "text/markdown",
        _ => "application/octet-stream",
    }
}

/// Classify a source file by extension into one of the five artifact kinds.
/// Unlike `pinned.rs::kind_for` (which treats every unknown extension as text),
/// an unknown extension here is "other" — the library stores arbitrary files and
/// must not claim a random binary is readable text.
fn kind_for(path: &Path) -> &'static str {
    match ext_of(path).as_deref() {
        Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("webp") | Some("bmp")
        | Some("heic") => "image",
        Some("mp4") | Some("m4v") | Some("mov") | Some("webm") | Some("mkv") | Some("avi") => {
            "video"
        }
        Some("pdf") => "pdf",
        Some("txt") | Some("md") | Some("markdown") => "text",
        _ => "other",
    }
}

fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The blob's filename inside `artifacts/<id>/`, reconstructed from the meta.
///
/// `import_one` names the blob `blob.<ext>` where `<ext>` is the lowercased
/// extension of the SOURCE file — and `original_filename` is that same source
/// filename, so running `ext_of` over it reproduces the name exactly. We prefer
/// this over scanning the dir for `blob.*` because every caller already has to
/// read meta.json anyway (for `kind`), so this costs no extra I/O and can't be
/// confused by a stray sibling file.
pub(crate) fn blob_name(meta: &ArtifactMeta) -> String {
    match ext_of(Path::new(&meta.original_filename)) {
        Some(ext) => format!("blob.{ext}"),
        None => "blob".to_string(),
    }
}

pub(crate) fn read_meta(dir: &Path) -> Result<ArtifactMeta, String> {
    let raw = fs::read(dir.join(META)).map_err(|e| format!("read meta: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("parse meta: {e}"))
}

fn write_meta(dir: &Path, meta: &ArtifactMeta) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(meta).map_err(|e| format!("serialize meta: {e}"))?;
    fs::write(dir.join(META), raw).map_err(|e| format!("write meta: {e}"))
}

/// Stream the file through SHA-256 and return the lowercase hex digest. Chunked
/// so a multi-GB video never lands in memory.
fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Downscale so the long edge is <= THUMB_EDGE (never upscales) and encode JPEG.
fn write_thumb(dir: &Path, img: &image::DynamicImage) -> Result<(), String> {
    let thumb = img.resize(
        THUMB_EDGE,
        THUMB_EDGE,
        image::imageops::FilterType::Triangle,
    );
    let bytes = crate::video::encode_jpeg(&thumb, THUMB_Q)?;
    fs::write(dir.join(THUMB), bytes).map_err(|e| format!("write thumb: {e}"))
}

/// Video duration via the bundled ffprobe sidecar. Returns None (never an Err)
/// on any failure — duration is a nice-to-have, not a reason to fail an import.
/// Deliberately separate from `video.rs::probe`, which hard-errors on unreadable
/// input and also computes the extraction-pipeline's resolution warnings.
fn probe_duration_ms(path: &Path) -> Option<u64> {
    let ffprobe = crate::video::resolve_bin("ffprobe").ok()?;
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            &path.to_string_lossy(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let secs = meta
        .get("format")?
        .get("duration")?
        .as_str()?
        .parse::<f64>()
        .ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some((secs * 1000.0) as u64)
    } else {
        None
    }
}

/// Grab one frame ~1s in via the bundled ffmpeg sidecar and write it as the
/// thumbnail. `-ss` before `-i` seeks cheaply; if the clip is shorter than 1s
/// ffmpeg yields nothing, so we retry from the very first frame.
fn thumb_from_video(dir: &Path, blob: &Path) -> Result<(), String> {
    let ffmpeg = crate::video::resolve_bin("ffmpeg")?;
    let tmp = dir.join("_thumb_src.png");

    let grab = |seek: &str| -> bool {
        let out = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", seek, "-i"])
            .arg(blob)
            .args(["-frames:v", "1"])
            .arg(&tmp)
            .output();
        matches!(out, Ok(o) if o.status.success()) && tmp.is_file()
    };

    if !grab("1") && !grab("0") {
        return Err("ffmpeg could not extract a frame".into());
    }
    let img = image::open(&tmp).map_err(|e| format!("decode extracted frame: {e}"));
    let _ = fs::remove_file(&tmp);
    write_thumb(dir, &img?)
}

/// Render page 1 of a PDF to the thumbnail via PDFium (bound at RUNTIME, exactly
/// like `pinned.rs::ingest_pdf` — a missing dylib is not a build or import
/// failure, just a missing thumbnail).
fn thumb_from_pdf(dir: &Path, blob: &Path) -> Result<(), String> {
    use pdfium_render::prelude::*;

    let bindings =
        Pdfium::bind_to_system_library().map_err(|e| format!("PDFium unavailable: {e}"))?;
    let pdfium = Pdfium::new(bindings);
    let doc = pdfium
        .load_pdf_from_file(blob, None)
        .map_err(|e| format!("open pdf: {e}"))?;
    let page = doc
        .pages()
        .first()
        .map_err(|e| format!("pdf has no first page: {e}"))?;
    let cfg = PdfRenderConfig::new().set_target_width(THUMB_EDGE as i32);
    let bitmap = page
        .render_with_config(&cfg)
        .map_err(|e| format!("render pdf page 1: {e}"))?;
    // Re-resize: target_width alone leaves a portrait page taller than THUMB_EDGE.
    write_thumb(dir, &bitmap.as_image())
}

/// Import ONE already-hashed file into `dir` (which does not yet exist). Split
/// out so `artifact_import` can cleanly skip a single bad file without unwinding
/// the batch, and so a half-written dir can be torn down on failure.
fn import_one(root: &Path, src: &Path, id: String) -> Result<ArtifactMeta, String> {
    let original_filename = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} has no filename", src.display()))?;
    let kind = kind_for(src);
    let dir = root.join(&id);
    fs::create_dir_all(&dir).map_err(|e| format!("create artifact dir: {e}"))?;

    // Everything past this point writes into `dir`; on any hard failure remove
    // it so a later import retries cleanly instead of finding a partial dir.
    let build = || -> Result<ArtifactMeta, String> {
        let blob_name = match ext_of(src) {
            Some(ext) => format!("blob.{ext}"),
            None => "blob".to_string(),
        };
        let blob = dir.join(&blob_name);
        let size_bytes = fs::copy(src, &blob)
            .map_err(|e| format!("copy {} -> {}: {e}", src.display(), blob.display()))?;

        // Thumbnail + probe are per-kind and NEVER fatal: an artifact with no
        // thumb.jpg is valid (the UI shows a kind icon instead).
        let mut width = None;
        let mut height = None;
        let mut duration_ms = None;
        match kind {
            "image" => match image::open(&blob) {
                Ok(img) => {
                    width = Some(img.width());
                    height = Some(img.height());
                    if let Err(e) = write_thumb(&dir, &img) {
                        eprintln!("[artifacts] thumb for '{original_filename}': {e}");
                    }
                }
                Err(e) => eprintln!("[artifacts] decode image '{original_filename}': {e}"),
            },
            "video" => {
                duration_ms = probe_duration_ms(&blob);
                if let Err(e) = thumb_from_video(&dir, &blob) {
                    eprintln!("[artifacts] thumb for video '{original_filename}': {e}");
                }
            }
            "pdf" => {
                if let Err(e) = thumb_from_pdf(&dir, &blob) {
                    eprintln!("[artifacts] thumb for pdf '{original_filename}': {e}");
                }
            }
            // text / other: no thumbnail, the UI shows a kind icon.
            _ => {}
        }

        let meta = ArtifactMeta {
            artifact_id: id.clone(),
            name: original_filename.clone(),
            original_filename: original_filename.clone(),
            kind: kind.to_string(),
            mime: mime_for(src).to_string(),
            size_bytes,
            width,
            height,
            duration_ms,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        };
        write_meta(&dir, &meta)?;
        Ok(meta)
    };

    build().inspect_err(|_| {
        let _ = fs::remove_dir_all(&dir);
    })
}

/// The blocking worker: hashing is I/O-bound and ffmpeg/PDFium are slow, so the
/// whole batch runs off the async runtime.
fn import_blocking(app: AppHandle, paths: Vec<String>) -> Result<Vec<ArtifactMeta>, String> {
    let root = artifacts_root(&app)?;
    let mut out = Vec::with_capacity(paths.len());

    for src in &paths {
        let src_path = Path::new(src);
        // A file we can't even hash can't be addressed — skip it, keep the batch.
        let id = match hash_file(src_path) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("[artifacts] skip '{src}': {e}");
                continue;
            }
        };

        // Dedup: identical bytes => identical id => already imported. Return the
        // EXISTING meta (preserving any rename) rather than re-copying.
        let dir = root.join(&id);
        if dir.is_dir() {
            match read_meta(&dir) {
                Ok(meta) => {
                    out.push(meta);
                    continue;
                }
                // A dir with no/corrupt meta.json is a half-finished import from
                // a previous crash: tear it down and redo it.
                Err(e) => {
                    eprintln!("[artifacts] rebuilding partial artifact {id}: {e}");
                    let _ = fs::remove_dir_all(&dir);
                }
            }
        }

        match import_one(&root, src_path, id) {
            Ok(meta) => out.push(meta),
            Err(e) => eprintln!("[artifacts] skip '{src}': {e}"),
        }
    }

    Ok(out)
}

/// Import files into the library. Returns the meta for every file that landed —
/// one bad file is skipped (and logged), never aborting the batch. Re-importing
/// bytes already in the library is a no-op that returns the existing meta.
#[tauri::command]
pub async fn artifact_import(
    app: AppHandle,
    paths: Vec<String>,
) -> Result<Vec<ArtifactMeta>, String> {
    tauri::async_runtime::spawn_blocking(move || import_blocking(app, paths))
        .await
        .map_err(|e| format!("import task panicked: {e}"))?
}

/// List every artifact, newest first. Corrupt/partial dirs are skipped rather
/// than failing the whole listing.
#[tauri::command]
pub fn artifact_list(app: AppHandle) -> Result<Vec<ArtifactMeta>, String> {
    let root = artifacts_root(&app)?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&root).map_err(|e| format!("read artifacts dir: {e}"))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(meta) = read_meta(&entry.path()) {
            out.push(meta);
        }
    }
    // created_at is RFC3339 UTC ('Z', fixed-width), so lexical == chronological.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Return an artifact's thumbnail as a `data:image/jpeg;base64,...` URL. Errs
/// clearly when there is no thumb.jpg (text/other kinds, or a failed generation)
/// so the UI can fall back to a kind icon.
#[tauri::command]
pub fn artifact_thumb(app: AppHandle, id: String) -> Result<String, String> {
    if !safe_id(&id) {
        return Err("invalid artifact id".into());
    }
    let path = artifacts_root(&app)?.join(&id).join(THUMB);
    if !path.is_file() {
        return Err("no thumbnail for this artifact".into());
    }
    let bytes = fs::read(&path).map_err(|e| format!("read thumb: {e}"))?;
    Ok(format!("data:image/jpeg;base64,{}", B64.encode(&bytes)))
}

/// Rename an artifact (display name only — the blob and id are immutable).
#[tauri::command]
pub fn artifact_rename(app: AppHandle, id: String, name: String) -> Result<(), String> {
    if !safe_id(&id) {
        return Err("invalid artifact id".into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("name cannot be empty".into());
    }
    let dir = artifacts_root(&app)?.join(&id);
    let mut meta = read_meta(&dir)?;
    meta.name = name.to_string();
    write_meta(&dir, &meta)
}

/// Delete an artifact and its files. Idempotent: deleting a missing artifact is
/// a success, so a retry after a partial failure always converges.
#[tauri::command]
pub fn artifact_delete(app: AppHandle, id: String) -> Result<(), String> {
    if !safe_id(&id) {
        return Err("invalid artifact id".into());
    }
    let dir = artifacts_root(&app)?.join(&id);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("delete artifact: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The traversal guard: only 64-char lowercase hex passes, so no id can ever
    /// address anything outside the artifacts root.
    #[test]
    fn safe_id_only_accepts_sha256_hex() {
        let ok = "a".repeat(64);
        assert!(safe_id(&ok));
        assert!(safe_id(&"0123456789abcdef".repeat(4)));

        assert!(!safe_id(""), "empty");
        assert!(!safe_id(&"a".repeat(63)), "too short");
        assert!(!safe_id(&"a".repeat(65)), "too long");
        assert!(!safe_id(&"A".repeat(64)), "uppercase hex");
        assert!(!safe_id(&"g".repeat(64)), "non-hex");
        assert!(!safe_id("../../etc/passwd"), "traversal");
        assert!(
            !safe_id(&format!("..{}", "a".repeat(62))),
            "traversal padded to 64 chars"
        );
    }

    /// Dedup depends on the id being the digest of the CONTENTS, not the path:
    /// same bytes at two different paths must collide, different bytes must not.
    #[test]
    fn hash_file_is_content_addressed() {
        let tmp = std::env::temp_dir().join(format!("sb_art_hash_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let a = tmp.join("a.txt");
        let b = tmp.join("b.txt");
        let c = tmp.join("c.txt");
        std::fs::write(&a, b"hello artifacts").unwrap();
        std::fs::write(&b, b"hello artifacts").unwrap();
        std::fs::write(&c, b"different bytes").unwrap();

        let ha = hash_file(&a).unwrap();
        assert_eq!(ha, hash_file(&b).unwrap(), "same bytes => same id (dedup)");
        assert_ne!(ha, hash_file(&c).unwrap(), "different bytes => different id");
        assert!(safe_id(&ha), "a real digest passes the traversal guard");

        // The known SHA-256 of the empty file — proves we hash contents, and that
        // the chunked read loop terminates correctly on a zero-length input.
        let empty = tmp.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            hash_file(&empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A file larger than HASH_CHUNK must hash identically to a single-shot
    /// digest of the same bytes (guards the streaming loop's buffer handling).
    #[test]
    fn hash_file_streams_multi_chunk_input() {
        let tmp = std::env::temp_dir().join(format!("sb_art_chunk_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let big = tmp.join("big.bin");
        // ~2.5 chunks of non-uniform bytes.
        let bytes: Vec<u8> = (0..HASH_CHUNK * 5 / 2).map(|i| (i % 251) as u8).collect();
        std::fs::write(&big, &bytes).unwrap();

        let expected = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(hash_file(&big).unwrap(), expected);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Extension routing: every accepted kind lands in the right bucket, and an
    /// unknown extension is "other" (NOT text — we must not claim a random binary
    /// is readable).
    #[test]
    fn kind_and_mime_routing() {
        let cases = [
            ("a.png", "image", "image/png"),
            ("a.JPG", "image", "image/jpeg"),
            ("a.mp4", "video", "video/mp4"),
            ("a.mov", "video", "video/quicktime"),
            ("a.pdf", "pdf", "application/pdf"),
            ("a.txt", "text", "text/plain"),
            ("a.md", "text", "text/markdown"),
            ("a.xyz", "other", "application/octet-stream"),
            ("noext", "other", "application/octet-stream"),
        ];
        for (file, kind, mime) in cases {
            let p = Path::new(file);
            assert_eq!(kind_for(p), kind, "kind_for({file})");
            assert_eq!(mime_for(p), mime, "mime_for({file})");
        }
    }

    /// `artifact_list` sorts newest-first, and the RFC3339 format `created_at`
    /// uses makes a lexical sort chronological (the assumption that sort relies
    /// on). Sorts the raw strings the same way the command does.
    #[test]
    fn rfc3339_sorts_chronologically() {
        let mut stamps = vec![
            "2026-01-02T03:04:05.000Z".to_string(),
            "2026-01-02T03:04:05.999Z".to_string(),
            "2025-12-31T23:59:59.000Z".to_string(),
            "2026-01-02T03:04:06.000Z".to_string(),
        ];
        stamps.sort_by(|a, b| b.cmp(a)); // newest-first, as artifact_list does
        assert_eq!(
            stamps,
            vec![
                "2026-01-02T03:04:06.000Z",
                "2026-01-02T03:04:05.999Z",
                "2026-01-02T03:04:05.000Z",
                "2025-12-31T23:59:59.000Z",
            ]
        );

        // And the real generator actually emits that fixed-width UTC shape.
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        assert!(now.ends_with('Z'), "UTC 'Z' suffix, got {now}");
        assert_eq!(now.len(), 24, "fixed width (lexical == chronological)");
    }

    /// meta.json is the shared contract with the backend: it must round-trip and
    /// keep its exact snake_case field names.
    #[test]
    fn meta_json_round_trips_with_contract_field_names() {
        let tmp = std::env::temp_dir().join(format!("sb_art_meta_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let meta = ArtifactMeta {
            artifact_id: "a".repeat(64),
            name: "renamed.png".into(),
            original_filename: "orig.png".into(),
            kind: "image".into(),
            mime: "image/png".into(),
            size_bytes: 4242,
            width: Some(800),
            height: Some(600),
            duration_ms: None,
            created_at: "2026-01-02T03:04:05.000Z".into(),
        };
        write_meta(&tmp, &meta).unwrap();

        let back = read_meta(&tmp).unwrap();
        assert_eq!(back.artifact_id, meta.artifact_id);
        assert_eq!(back.name, "renamed.png");
        assert_eq!(back.original_filename, "orig.png");
        assert_eq!(back.size_bytes, 4242);
        assert_eq!(back.width, Some(800));
        assert_eq!(back.duration_ms, None);

        // Field names on the wire (what the backend indexes on).
        let raw = std::fs::read_to_string(tmp.join(META)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        for key in [
            "artifact_id",
            "name",
            "original_filename",
            "kind",
            "mime",
            "size_bytes",
            "width",
            "height",
            "duration_ms",
            "created_at",
        ] {
            assert!(v.get(key).is_some(), "meta.json must carry `{key}`");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A corrupt meta.json must be reported as an error (which is what makes
    /// `artifact_list` skip it and `import_blocking` rebuild the dir) rather than
    /// panicking or silently yielding a half-populated struct.
    #[test]
    fn read_meta_rejects_corrupt_dir() {
        let tmp = std::env::temp_dir().join(format!("sb_art_bad_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        assert!(read_meta(&tmp).is_err(), "missing meta.json");
        std::fs::write(tmp.join(META), b"{not json").unwrap();
        assert!(read_meta(&tmp).is_err(), "corrupt meta.json");
        std::fs::write(tmp.join(META), br#"{"name":"x"}"#).unwrap();
        assert!(read_meta(&tmp).is_err(), "meta.json missing required fields");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
