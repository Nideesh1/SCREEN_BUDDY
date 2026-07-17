// pinned.rs — pinned reference library (images + PDFs + text/markdown).
//
// Reusable named sets stored locally so the user can re-feed a known set of
// reference material into the agent. Layout:
//   app_data_dir/pinned/<set_id>/<copied source files...>      (legacy sets)
//   app_data_dir/pinned/<set_id>/<derived files: *.extracted.txt, *.pN.jpg>
//   app_data_dir/pinned/<set_id>/manifest.json
//      { name, files: [ { artifact_id? | file?, kind, mode, extracted?, pages? } ] }
// Each set_id is a random hex string.
//
// A set entry is one of two shapes, and both must keep working forever:
//   * artifact-backed (`artifact_id`): a REFERENCE into `artifacts/<id>/` (see
//     artifacts.rs). Nothing is copied — N sets citing one photo cost one copy.
//     Derivations live in `artifacts/<id>/derived/` so they're computed once and
//     shared by every set that references the artifact.
//   * legacy (`file`): a filename inside the set dir, from the old
//     copy-everything-in `pinned_create`. Derivations sit beside it in the set
//     dir. Sets already on disk have exactly this shape and are never migrated.
// `resolve_entry` is the ONE place that tells the two apart; every reader goes
// through it. Deleting a set deletes only the set dir — never artifact bytes.
//
// PDFium: PDF ingest uses `pdfium-render`, which binds to the PDFium dynamic
// library at RUNTIME via `Pdfium::bind_to_system_library()` — so building this
// crate does NOT require the dylib to be present. If the library can't be bound
// (or extraction/render fails), the PDF degrades to a text note rather than
// hard-failing the whole `pinned_create` command.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

const PINNED_DIR: &str = "pinned";
const MANIFEST: &str = "manifest.json";
/// Subdir of an artifact dir holding derivations computed FROM that artifact
/// (extracted text, rendered PDF pages). Lives under the artifact, not the set,
/// so it is computed once and reused by every set referencing the artifact.
const DERIVED_DIR: &str = "derived";
/// Cache record inside `<artifact>/derived/` describing what was derived.
const DERIVED_MANIFEST: &str = "pdf.json";

/// Per-file metadata in a set. Exactly one of `artifact_id` / `file` identifies
/// the bytes (see `resolve_entry`); `extracted` / `pages` are resolved relative
/// to whichever dir that entry's derivations live in.
///   artifact_id: reference into `artifacts/<id>/` — nothing copied.
///   file: LEGACY filename inside the set dir (pre-artifacts sets).
///   kind: "image" | "pdf" | "text"  (artifact-backed entries carry the
///         artifact's own kind, which may also be "video" / "other")
///   mode: "text" | "image"  (only meaningful for pdf; resolved at ingest)
///   extracted: for a text-mode pdf, the .txt filename holding the extracted
///              text layer (None => could not extract).
///   pages: for an image-mode pdf, the rendered page JPEG filenames in order.
#[derive(Debug, Serialize, Deserialize)]
pub struct FileEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub kind: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<String>,
}

/// On-disk per-set manifest.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    name: String,
    files: Vec<FileEntry>,
}

/// Summary returned by `pinned_list`.
#[derive(Debug, Serialize)]
pub struct PinnedSummary {
    pub id: String,
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct CreatedSet {
    pub id: String,
}

/// One item in a set, with a data URL for UI preview (empty for non-visual
/// items) plus its kind so the UI can label it.
#[derive(Debug, Serialize)]
pub struct PinnedImage {
    pub name: String,
    pub kind: String,
    pub data_url: String,
}

#[derive(Debug, Serialize)]
pub struct PinnedSet {
    pub name: String,
    pub images: Vec<PinnedImage>,
}

fn pinned_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?
        .join(PINNED_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create pinned dir: {e}"))?;
    Ok(dir)
}

fn read_manifest(set_dir: &Path) -> Result<Manifest, String> {
    let raw = fs::read(set_dir.join(MANIFEST)).map_err(|e| format!("read manifest: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("parse manifest: {e}"))
}

/// Reject ids that could escape the pinned root via path traversal.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && !id.contains('\\') && !id.contains("..")
}

/// Guess a MIME type from a file extension for data URLs / image blocks.
fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") | Some("markdown") => "text/markdown",
        _ => "application/octet-stream",
    }
}

/// Classify a source file by extension into one of the three set kinds.
fn kind_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("webp") | Some("bmp") => {
            "image"
        }
        Some("pdf") => "pdf",
        // txt / md / markdown and any other extension: treat as text.
        _ => "text",
    }
}

/// Where one manifest entry's bytes and derivations actually live on disk.
struct Resolved {
    /// The file itself (an `artifacts/<id>/blob.<ext>`, or a set-dir file).
    path: PathBuf,
    /// Dir that this entry's `extracted` / `pages` names are relative to:
    /// `artifacts/<id>/derived/` for artifact-backed entries, the set dir for
    /// legacy ones.
    base: PathBuf,
    /// Display name / block title.
    name: String,
}

/// THE resolution rule, used by every reader of the manifest (`pinned_get`,
/// `load_blocks`). Takes both roots as plain paths (no AppHandle) so it is
/// directly testable.
///
/// * `artifact_id` set  -> `artifacts/<id>/blob.<ext>`, derivations under
///   `artifacts/<id>/derived/`.
/// * else `file` set    -> `set_dir/<file>`, derivations beside it (unchanged
///   legacy behavior).
/// * neither / unresolvable -> None, logged. A dangling reference (artifact
///   deleted out from under the set) must degrade to a skipped item, never a
///   panic and never a failed set.
fn resolve_entry(set_dir: &Path, artifacts_root: &Path, entry: &FileEntry) -> Option<Resolved> {
    if let Some(id) = &entry.artifact_id {
        if !crate::artifacts::safe_id(id) {
            eprintln!("[pinned] skip entry: invalid artifact id '{id}'");
            return None;
        }
        let dir = artifacts_root.join(id);
        let meta = match crate::artifacts::read_meta(&dir) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[pinned] skip artifact {id}: {e}");
                return None;
            }
        };
        let path = dir.join(crate::artifacts::blob_name(&meta));
        if !path.is_file() {
            eprintln!("[pinned] skip artifact {id}: blob missing at {}", path.display());
            return None;
        }
        return Some(Resolved {
            path,
            base: dir.join(DERIVED_DIR),
            name: meta.name,
        });
    }
    if let Some(file) = &entry.file {
        return Some(Resolved {
            path: set_dir.join(file),
            base: set_dir.to_path_buf(),
            name: file.clone(),
        });
    }
    eprintln!("[pinned] skip entry: neither artifact_id nor file");
    None
}

/// List all pinned sets (id + name + file count).
#[tauri::command]
pub fn pinned_list(app: AppHandle) -> Result<Vec<PinnedSummary>, String> {
    let root = pinned_root(&app)?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&root).map_err(|e| format!("read pinned dir: {e}"))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        // Skip malformed sets (no/unreadable manifest) rather than erroring out.
        if let Ok(manifest) = read_manifest(&entry.path()) {
            out.push(PinnedSummary {
                id,
                name: manifest.name,
                count: manifest.files.len(),
            });
        }
    }
    Ok(out)
}

/// Create a new set by COPYING each source file into the set dir, ingesting any
/// PDFs, writing the manifest, and returning the new id.
///
/// The original, copy-everything path. Still used by flows that hand over
/// throwaway files not in the artifact library (e.g. extracted video frames).
/// For user media prefer `pinned_create_from_artifacts`, which references the
/// artifact store instead of duplicating bytes per set.
#[tauri::command]
pub fn pinned_create(
    app: AppHandle,
    name: String,
    paths: Vec<String>,
) -> Result<CreatedSet, String> {
    let mut id_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut id_bytes);
    let id = id_bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let set_dir = pinned_root(&app)?.join(&id);
    fs::create_dir_all(&set_dir).map_err(|e| format!("create set dir: {e}"))?;

    let mut files = Vec::with_capacity(paths.len());
    for (idx, src) in paths.iter().enumerate() {
        let src_path = Path::new(src);
        // Prefix with index to keep copies unique even if basenames collide.
        let base = src_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file_{idx}"));
        let dest_name = format!("{idx}_{base}");
        let dest = set_dir.join(&dest_name);
        fs::copy(src_path, &dest)
            .map_err(|e| format!("copy {src} -> {}: {e}", dest.display()))?;

        let kind = kind_for(src_path);
        let entry = match kind {
            "pdf" => ingest_pdf(&set_dir, &dest_name),
            "image" => FileEntry {
                artifact_id: None,
                file: Some(dest_name),
                kind: "image".into(),
                mode: "image".into(),
                extracted: None,
                pages: Vec::new(),
            },
            _ => FileEntry {
                artifact_id: None,
                file: Some(dest_name),
                kind: "text".into(),
                mode: "text".into(),
                extracted: None,
                pages: Vec::new(),
            },
        };
        files.push(entry);
    }

    let manifest = Manifest { name, files };
    let raw =
        serde_json::to_vec_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(set_dir.join(MANIFEST), raw).map_err(|e| format!("write manifest: {e}"))?;

    Ok(CreatedSet { id })
}

/// What ingesting a PDF produced. Also the on-disk cache record written to
/// `<artifact>/derived/pdf.json` so the (slow) derivation happens once per PDF
/// rather than once per set referencing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PdfDerivation {
    mode: String,
    #[serde(default)]
    extracted: Option<String>,
    #[serde(default)]
    pages: Vec<String>,
}

impl PdfDerivation {
    /// PDFium missing, PDF unreadable, or nothing came out: send it as a note.
    fn degraded() -> Self {
        PdfDerivation {
            mode: "text".into(),
            extracted: None,
            pages: Vec::new(),
        }
    }

    /// A degraded result is a *transient* failure (PDFium not bound yet, bad
    /// read) — never cache it, or one unlucky run would poison the artifact
    /// forever. Only a real extraction/render is worth persisting.
    fn is_cacheable(&self) -> bool {
        self.extracted.is_some() || !self.pages.is_empty()
    }
}

/// The single PDF ingest core (hybrid auto-route, local via PDFium), shared by
/// the legacy copy-in path and the artifact-backed path — only the output dir
/// and filename stem differ.
///
/// Runs PDFium text extraction; if the text layer is clean (>= ~100 chars per
/// page averaged) writes `<stem>.extracted.txt` into `out_dir` and returns
/// mode="text"; otherwise renders each page to `<stem>.p<N>.jpg` there and
/// returns mode="image". Never panics or hard-fails: if PDFium can't bind or
/// anything errors, returns a degraded text-mode result with no extraction.
fn derive_pdf(pdf_path: &Path, out_dir: &Path, stem: &str) -> PdfDerivation {
    use pdfium_render::prelude::*;

    if let Err(e) = fs::create_dir_all(out_dir) {
        eprintln!("[pinned] create derived dir {}: {e}", out_dir.display());
        return PdfDerivation::degraded();
    }

    // Bind to the system PDFium library at call time (NOT a build requirement).
    let bindings = match Pdfium::bind_to_system_library() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[pinned] PDFium unavailable ({e}); '{stem}' sent as a note");
            return PdfDerivation::degraded();
        }
    };
    let pdfium = Pdfium::new(bindings);
    let doc = match pdfium.load_pdf_from_file(pdf_path, None) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[pinned] open pdf '{stem}' failed: {e}");
            return PdfDerivation::degraded();
        }
    };

    let pages = doc.pages();
    let page_count = pages.len() as usize;
    if page_count == 0 {
        return PdfDerivation::degraded();
    }

    // 1) Try the text layer.
    let mut text = String::new();
    for page in pages.iter() {
        if let Ok(t) = page.text() {
            text.push_str(&t.all());
            text.push('\n');
        }
    }
    let avg_chars = text.trim().chars().count() / page_count;
    if avg_chars >= 100 {
        let ex_name = format!("{stem}.extracted.txt");
        if fs::write(out_dir.join(&ex_name), &text).is_ok() {
            return PdfDerivation {
                mode: "text".into(),
                extracted: Some(ex_name),
                pages: Vec::new(),
            };
        }
        eprintln!("[pinned] write extracted text for '{stem}' failed");
        return PdfDerivation::degraded();
    }

    // 2) Scanned / image-only PDF: render each page to a JPEG.
    let cfg = PdfRenderConfig::new().set_target_width(1280);
    let mut page_files = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        match page.render_with_config(&cfg) {
            Ok(bitmap) => {
                let jpg = format!("{stem}.p{i}.jpg");
                // .save() infers JPEG from the .jpg extension.
                if bitmap.as_image().save(out_dir.join(&jpg)).is_ok() {
                    page_files.push(jpg);
                } else {
                    eprintln!("[pinned] save rendered page {i} of '{stem}' failed");
                }
            }
            Err(e) => eprintln!("[pinned] render page {i} of '{stem}': {e}"),
        }
    }
    if page_files.is_empty() {
        return PdfDerivation::degraded();
    }
    PdfDerivation {
        mode: "image".into(),
        extracted: None,
        pages: page_files,
    }
}

/// Ingest one already-copied-in PDF for a LEGACY set: derive into the set dir
/// itself, keyed by the copied-in filename (byte-identical output to before).
fn ingest_pdf(set_dir: &Path, pdf_file: &str) -> FileEntry {
    let d = derive_pdf(&set_dir.join(pdf_file), set_dir, pdf_file);
    FileEntry {
        artifact_id: None,
        file: Some(pdf_file.to_string()),
        kind: "pdf".into(),
        mode: d.mode,
        extracted: d.extracted,
        pages: d.pages,
    }
}

/// Derivations for an artifact-backed PDF, computed lazily and cached under
/// `artifacts/<id>/derived/`.
///
/// Keyed by artifact id (the SHA-256 of the PDF's bytes) — so identical bytes
/// share one derivation no matter how many sets cite them, and the cache can
/// never go stale: different content is a different id, hence a different dir.
fn derive_pdf_for_artifact(artifact_dir: &Path, blob: &Path, stem: &str) -> PdfDerivation {
    let out_dir = artifact_dir.join(DERIVED_DIR);
    let cache = out_dir.join(DERIVED_MANIFEST);

    if let Ok(raw) = fs::read(&cache) {
        if let Ok(d) = serde_json::from_slice::<PdfDerivation>(&raw) {
            return d;
        }
        eprintln!("[pinned] corrupt {}; re-deriving", cache.display());
    }

    let d = derive_pdf(blob, &out_dir, stem);
    if d.is_cacheable() {
        match serde_json::to_vec_pretty(&d) {
            Ok(raw) => {
                if let Err(e) = fs::write(&cache, raw) {
                    eprintln!("[pinned] write {}: {e}", cache.display());
                }
            }
            Err(e) => eprintln!("[pinned] serialize pdf derivation: {e}"),
        }
    }
    d
}

/// Create a set that REFERENCES artifacts instead of copying them. Copies zero
/// bytes: five sets built from one photo cost one blob on disk.
///
/// Unknown / unreadable ids are skipped and logged rather than failing the batch
/// (mirrors `artifact_import`), but a batch where NOTHING resolved is an error —
/// an empty set is never what the user asked for. `kind` comes from the
/// artifact's own meta.json (the library already classified these bytes; the
/// blob path is a content hash and re-deriving from it would be wrong).
#[tauri::command]
pub fn pinned_create_from_artifacts(
    app: AppHandle,
    name: String,
    artifact_ids: Vec<String>,
) -> Result<CreatedSet, String> {
    let artifacts_root = crate::artifacts::artifacts_root(&app)?;

    // Resolve everything BEFORE creating the set dir, so a fully-unresolvable
    // batch leaves nothing behind.
    let mut files = Vec::with_capacity(artifact_ids.len());
    for id in &artifact_ids {
        if !crate::artifacts::safe_id(id) {
            eprintln!("[pinned] skip: invalid artifact id '{id}'");
            continue;
        }
        let dir = artifacts_root.join(id);
        let meta = match crate::artifacts::read_meta(&dir) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[pinned] skip artifact {id}: {e}");
                continue;
            }
        };
        let blob_name = crate::artifacts::blob_name(&meta);
        let blob = dir.join(&blob_name);
        if !blob.is_file() {
            eprintln!("[pinned] skip artifact {id}: blob missing");
            continue;
        }

        // mode is only meaningful for PDFs; for everything else it follows the
        // existing convention (image kinds "image", the rest "text").
        let entry = if meta.kind == "pdf" {
            let d = derive_pdf_for_artifact(&dir, &blob, &blob_name);
            FileEntry {
                artifact_id: Some(id.clone()),
                file: None,
                kind: meta.kind.clone(),
                mode: d.mode,
                extracted: d.extracted,
                pages: d.pages,
            }
        } else {
            FileEntry {
                artifact_id: Some(id.clone()),
                file: None,
                kind: meta.kind.clone(),
                mode: if meta.kind == "image" { "image" } else { "text" }.into(),
                extracted: None,
                pages: Vec::new(),
            }
        };
        files.push(entry);
    }

    if files.is_empty() {
        return Err("no valid artifacts to pin".into());
    }

    let mut id_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut id_bytes);
    let id = id_bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let set_dir = pinned_root(&app)?.join(&id);
    fs::create_dir_all(&set_dir).map_err(|e| format!("create set dir: {e}"))?;

    let manifest = Manifest { name, files };
    let raw =
        serde_json::to_vec_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(set_dir.join(MANIFEST), raw).map_err(|e| format!("write manifest: {e}"))?;

    Ok(CreatedSet { id })
}

/// Delete a set: removes ONLY the set dir (its manifest, and for legacy sets the
/// files copied into it). Artifact-backed entries are references — the blobs
/// under `artifacts/` belong to the library and are shared by other sets, so
/// nothing here may ever reach into that tree. Artifacts are deleted solely via
/// `artifact_delete`.
#[tauri::command]
pub fn pinned_delete(app: AppHandle, id: String) -> Result<(), String> {
    // Guard against path traversal via a crafted id.
    if !safe_id(&id) {
        return Err("invalid set id".into());
    }
    let set_dir = pinned_root(&app)?.join(&id);
    if set_dir.exists() {
        fs::remove_dir_all(&set_dir).map_err(|e| format!("delete set: {e}"))?;
    }
    Ok(())
}

/// Read a full set for UI preview. Image items (and the first rendered page of
/// an image-mode PDF) carry a `data:<mime>;base64,...` URL; text / text-mode
/// PDF items carry an empty `data_url` and rely on `kind` for display.
#[tauri::command]
pub fn pinned_get(app: AppHandle, id: String) -> Result<PinnedSet, String> {
    if !safe_id(&id) {
        return Err("invalid set id".into());
    }
    let set_dir = pinned_root(&app)?.join(&id);
    let manifest = read_manifest(&set_dir)?;
    let artifacts_root = crate::artifacts::artifacts_root(&app)?;

    let mut images = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        // An unresolvable entry (dangling artifact ref, malformed manifest) is
        // dropped from the preview rather than failing the whole set.
        let Some(r) = resolve_entry(&set_dir, &artifacts_root, entry) else {
            continue;
        };
        let data_url = match entry.kind.as_str() {
            "image" => fs::read(&r.path)
                .map(|bytes| format!("data:{};base64,{}", mime_for(&r.path), B64.encode(&bytes)))
                .unwrap_or_default(),
            "pdf" if entry.mode == "image" => entry
                .pages
                .first()
                .and_then(|p| fs::read(r.base.join(p)).ok())
                .map(|bytes| format!("data:image/jpeg;base64,{}", B64.encode(&bytes)))
                .unwrap_or_default(),
            _ => String::new(),
        };
        images.push(PinnedImage {
            name: r.name,
            kind: entry.kind.clone(),
            data_url,
        });
    }

    Ok(PinnedSet {
        name: manifest.name,
        images,
    })
}

/// Build ready-to-send Anthropic content blocks for a set:
///   image           -> {"type":"image","source":{"type":"base64",...}}
///   text            -> {"type":"document","source":{"type":"text",...},"title":...}
///   pdf mode=text   -> document block from the extracted .txt (or a note if none)
///   pdf mode=image  -> one image block per rendered page JPEG
/// Returns `[]` if the set id is missing/invalid.
#[allow(dead_code)]
pub fn load_blocks(app: &AppHandle, set_id: &str) -> Vec<Value> {
    if !safe_id(set_id) {
        return Vec::new();
    }
    let Ok(root) = pinned_root(app) else {
        return Vec::new();
    };
    let set_dir = root.join(set_id);
    let Ok(manifest) = read_manifest(&set_dir) else {
        return Vec::new();
    };
    let Ok(artifacts_root) = crate::artifacts::artifacts_root(app) else {
        return Vec::new();
    };

    let mut blocks = Vec::new();
    for entry in &manifest.files {
        // Unresolvable entries contribute no blocks (logged in resolve_entry).
        let Some(r) = resolve_entry(&set_dir, &artifacts_root, entry) else {
            continue;
        };
        match entry.kind.as_str() {
            "image" => {
                if let Ok(bytes) = fs::read(&r.path) {
                    blocks.push(image_block(mime_for(&r.path), &bytes));
                }
            }
            "text" => {
                if let Ok(text) = fs::read_to_string(&r.path) {
                    blocks.push(text_doc_block(&text, &r.name));
                }
            }
            "pdf" if entry.mode == "image" => {
                for page in &entry.pages {
                    if let Ok(bytes) = fs::read(r.base.join(page)) {
                        blocks.push(image_block("image/jpeg", &bytes));
                    }
                }
            }
            "pdf" => {
                // text mode
                match entry
                    .extracted
                    .as_ref()
                    .and_then(|ex| fs::read_to_string(r.base.join(ex)).ok())
                {
                    Some(text) => blocks.push(text_doc_block(&text, &r.name)),
                    None => blocks.push(json!({
                        "type": "text",
                        "text": format!("[PDF {} — could not extract]", r.name),
                    })),
                }
            }
            // Kinds the artifact library accepts but a set can't send inline
            // (video, other): resolved fine, just no block.
            _ => {}
        }
    }
    blocks
}

fn image_block(media_type: &str, bytes: &[u8]) -> Value {
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": B64.encode(bytes)},
    })
}

fn text_doc_block(text: &str, title: &str) -> Value {
    json!({
        "type": "document",
        "source": {"type": "text", "media_type": "text/plain", "data": text},
        "title": title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactMeta;

    /// A scratch dir with the two roots the resolver takes. `resolve_entry`
    /// deliberately takes plain paths (not an AppHandle) so it is testable here.
    struct Env {
        tmp: PathBuf,
        set_dir: PathBuf,
        artifacts: PathBuf,
    }

    impl Env {
        fn new(tag: &str) -> Env {
            let tmp = std::env::temp_dir().join(format!(
                "sb_pinned_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&tmp);
            let set_dir = tmp.join("pinned").join("set0");
            let artifacts = tmp.join("artifacts");
            fs::create_dir_all(&set_dir).unwrap();
            fs::create_dir_all(&artifacts).unwrap();
            Env {
                tmp,
                set_dir,
                artifacts,
            }
        }

        /// Materialize an artifact the way `artifacts::import_one` does:
        /// `<id>/blob.<ext>` + `<id>/meta.json`.
        fn artifact(&self, id_char: char, filename: &str, kind: &str, bytes: &[u8]) -> String {
            let id = id_char.to_string().repeat(64);
            let dir = self.artifacts.join(&id);
            fs::create_dir_all(&dir).unwrap();
            let ext = Path::new(filename).extension().unwrap().to_str().unwrap();
            fs::write(dir.join(format!("blob.{ext}")), bytes).unwrap();
            let meta = ArtifactMeta {
                artifact_id: id.clone(),
                name: format!("Nice {filename}"),
                original_filename: filename.to_string(),
                kind: kind.to_string(),
                mime: "application/octet-stream".into(),
                size_bytes: bytes.len() as u64,
                width: None,
                height: None,
                duration_ms: None,
                created_at: "2026-01-02T03:04:05.000Z".into(),
            };
            fs::write(
                dir.join("meta.json"),
                serde_json::to_vec_pretty(&meta).unwrap(),
            )
            .unwrap();
            id
        }

        fn resolve(&self, entry: &FileEntry) -> Option<Resolved> {
            resolve_entry(&self.set_dir, &self.artifacts, entry)
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }

    fn entry(json: Value) -> FileEntry {
        serde_json::from_value(json).expect("manifest entry must deserialize")
    }

    /// Sets written before artifacts existed have `file` and no `artifact_id`.
    /// They must deserialize and resolve into the SET dir, exactly as before.
    #[test]
    fn legacy_manifest_still_resolves() {
        let env = Env::new("legacy");
        fs::write(env.set_dir.join("0_notes.txt"), b"hello").unwrap();

        // Byte-for-byte a pre-artifacts manifest: no artifact_id key at all.
        let m: Manifest = serde_json::from_str(
            r#"{"name":"old set","files":[
                 {"file":"0_notes.txt","kind":"text","mode":"text"},
                 {"file":"1_scan.pdf","kind":"pdf","mode":"image","pages":["1_scan.pdf.p0.jpg"]}
               ]}"#,
        )
        .unwrap();
        assert_eq!(m.name, "old set");
        assert_eq!(m.files.len(), 2);
        assert!(m.files[0].artifact_id.is_none());

        let r = env.resolve(&m.files[0]).expect("legacy entry resolves");
        assert_eq!(r.path, env.set_dir.join("0_notes.txt"));
        assert_eq!(fs::read_to_string(&r.path).unwrap(), "hello");
        assert_eq!(r.name, "0_notes.txt");
        // Legacy derivations sit beside the file, in the SET dir.
        assert_eq!(r.base, env.set_dir);

        let pdf = env.resolve(&m.files[1]).unwrap();
        assert_eq!(
            pdf.base.join(&m.files[1].pages[0]),
            env.set_dir.join("1_scan.pdf.p0.jpg")
        );
    }

    /// An artifact-backed entry resolves to the blob in the artifact store, and
    /// its derivations to `artifacts/<id>/derived/` — never the set dir.
    #[test]
    fn artifact_backed_manifest_resolves() {
        let env = Env::new("artifact");
        let id = env.artifact('a', "photo.png", "image", b"PNGDATA");

        let e = entry(json!({"artifact_id": id, "kind": "image", "mode": "image"}));
        let r = env.resolve(&e).expect("artifact entry resolves");

        assert_eq!(r.path, env.artifacts.join(&id).join("blob.png"));
        assert_eq!(fs::read(&r.path).unwrap(), b"PNGDATA");
        // Display name comes from meta.json (honors a user rename).
        assert_eq!(r.name, "Nice photo.png");
        assert_eq!(r.base, env.artifacts.join(&id).join(DERIVED_DIR));
        // The blob keeps its real extension, so mime routing still works.
        assert_eq!(mime_for(&r.path), "image/png");

        // A PDF artifact's pages resolve under the artifact's derived dir, so
        // every set citing this PDF reuses one render.
        let pid = env.artifact('b', "doc.pdf", "pdf", b"%PDF-1.4");
        let pe = entry(json!({
            "artifact_id": pid, "kind": "pdf", "mode": "image",
            "pages": ["blob.pdf.p0.jpg"]
        }));
        let pr = env.resolve(&pe).unwrap();
        assert_eq!(
            pr.base.join(&pe.pages[0]),
            env.artifacts.join(&pid).join("derived").join("blob.pdf.p0.jpg")
        );
    }

    /// One manifest may hold both shapes at once (a legacy set is never
    /// migrated, but nothing stops the two from coexisting).
    #[test]
    fn mixed_manifest_resolves_each_shape() {
        let env = Env::new("mixed");
        let id = env.artifact('c', "photo.jpg", "image", b"JPEGDATA");
        fs::write(env.set_dir.join("0_old.txt"), b"legacy bytes").unwrap();

        let m: Manifest = serde_json::from_value(json!({
            "name": "mixed",
            "files": [
                {"file": "0_old.txt", "kind": "text", "mode": "text"},
                {"artifact_id": id, "kind": "image", "mode": "image"},
            ]
        }))
        .unwrap();

        let a = env.resolve(&m.files[0]).unwrap();
        let b = env.resolve(&m.files[1]).unwrap();
        assert!(a.path.starts_with(&env.set_dir), "legacy -> set dir");
        assert!(b.path.starts_with(&env.artifacts), "artifact -> store");
        assert_eq!(fs::read(&b.path).unwrap(), b"JPEGDATA");
    }

    /// Every unresolvable shape must return None (and log), never panic — a set
    /// whose artifact was deleted still opens, just short an item.
    #[test]
    fn unresolvable_entries_are_skipped_not_fatal() {
        let env = Env::new("missing");

        // Dangling reference: well-formed id, nothing on disk.
        let gone = entry(json!({"artifact_id": "d".repeat(64), "kind": "image", "mode": "image"}));
        assert!(env.resolve(&gone).is_none(), "missing artifact dir");

        // Dir exists but the blob is gone (half-torn-down artifact).
        let id = env.artifact('e', "photo.png", "image", b"X");
        fs::remove_file(env.artifacts.join(&id).join("blob.png")).unwrap();
        let hollow = entry(json!({"artifact_id": id, "kind": "image", "mode": "image"}));
        assert!(env.resolve(&hollow).is_none(), "blob missing");

        // Dir exists, meta.json corrupt.
        let id2 = env.artifact('f', "photo.png", "image", b"X");
        fs::write(env.artifacts.join(&id2).join("meta.json"), b"{nope").unwrap();
        let corrupt = entry(json!({"artifact_id": id2, "kind": "image", "mode": "image"}));
        assert!(env.resolve(&corrupt).is_none(), "corrupt meta");

        // Traversal attempt via a crafted id.
        let evil = entry(json!({"artifact_id": "../../etc/passwd", "kind": "text", "mode": "text"}));
        assert!(env.resolve(&evil).is_none(), "traversal id rejected");

        // Neither field: skip, don't panic.
        let empty = entry(json!({"kind": "text", "mode": "text"}));
        assert!(empty.artifact_id.is_none() && empty.file.is_none());
        assert!(env.resolve(&empty).is_none(), "neither artifact_id nor file");

        // artifact_id wins when (impossibly) both are set, and a dangling one
        // must NOT silently fall back to a set-dir file.
        fs::write(env.set_dir.join("decoy.txt"), b"decoy").unwrap();
        let both = entry(json!({
            "artifact_id": "9".repeat(64), "file": "decoy.txt",
            "kind": "text", "mode": "text"
        }));
        assert!(env.resolve(&both).is_none(), "no fallback to the set dir");
    }

    /// The whole point of the split: N sets referencing one artifact store the
    /// bytes ONCE. Both sets must resolve to the identical blob path, and the
    /// set dirs must contain nothing but their manifests.
    #[test]
    fn two_sets_referencing_one_artifact_copy_zero_bytes() {
        let env = Env::new("dedup");
        let bytes = b"the only copy of these bytes";
        let id = env.artifact('1', "shared.png", "image", bytes);

        let mut set_dirs = Vec::new();
        for n in 0..2 {
            let set_dir = env.tmp.join("pinned").join(format!("dedup{n}"));
            fs::create_dir_all(&set_dir).unwrap();
            let manifest = Manifest {
                name: format!("set {n}"),
                files: vec![FileEntry {
                    artifact_id: Some(id.clone()),
                    file: None,
                    kind: "image".into(),
                    mode: "image".into(),
                    extracted: None,
                    pages: Vec::new(),
                }],
            };
            fs::write(
                set_dir.join(MANIFEST),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            set_dirs.push(set_dir);
        }

        // Both sets point at the same physical file.
        let mut paths = Vec::new();
        for set_dir in &set_dirs {
            let m = read_manifest(set_dir).unwrap();
            let r = resolve_entry(set_dir, &env.artifacts, &m.files[0]).unwrap();
            assert_eq!(fs::read(&r.path).unwrap(), bytes);
            paths.push(r.path);
        }
        assert_eq!(paths[0], paths[1], "one artifact => one blob, shared");

        // And no set copied anything: manifest.json is the only file in each.
        for set_dir in &set_dirs {
            let names: Vec<String> = fs::read_dir(set_dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names, vec![MANIFEST.to_string()], "set dir holds no copies");
        }

        // Exactly one blob exists in the store for these bytes.
        assert_eq!(fs::read_dir(&env.artifacts).unwrap().count(), 1);
    }

    /// The manifest is the on-disk contract: an artifact-backed entry must not
    /// write a `file` key (and vice versa), or an older reader would resolve the
    /// wrong thing.
    #[test]
    fn manifest_round_trips_both_shapes() {
        let manifest = Manifest {
            name: "s".into(),
            files: vec![
                FileEntry {
                    artifact_id: Some("a".repeat(64)),
                    file: None,
                    kind: "pdf".into(),
                    mode: "text".into(),
                    extracted: Some("blob.pdf.extracted.txt".into()),
                    pages: Vec::new(),
                },
                FileEntry {
                    artifact_id: None,
                    file: Some("0_x.png".into()),
                    kind: "image".into(),
                    mode: "image".into(),
                    extracted: None,
                    pages: Vec::new(),
                },
            ],
        };
        let v: Value = serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(v["files"][0].get("file").is_none(), "no stray file key");
        assert_eq!(v["files"][0]["artifact_id"], "a".repeat(64));
        assert!(
            v["files"][1].get("artifact_id").is_none(),
            "no stray artifact_id key"
        );
        assert_eq!(v["files"][1]["file"], "0_x.png");

        let back: Manifest = serde_json::from_value(v).unwrap();
        assert_eq!(back.files[0].extracted.as_deref(), Some("blob.pdf.extracted.txt"));
        assert_eq!(back.files[1].file.as_deref(), Some("0_x.png"));
    }

    /// A degraded PDF derivation (PDFium unavailable) must never be cached, or a
    /// single unlucky run would poison the artifact permanently.
    #[test]
    fn only_real_pdf_derivations_are_cacheable() {
        assert!(!PdfDerivation::degraded().is_cacheable());
        assert!(PdfDerivation {
            mode: "text".into(),
            extracted: Some("blob.pdf.extracted.txt".into()),
            pages: Vec::new(),
        }
        .is_cacheable());
        assert!(PdfDerivation {
            mode: "image".into(),
            extracted: None,
            pages: vec!["blob.pdf.p0.jpg".into()],
        }
        .is_cacheable());
    }
}
