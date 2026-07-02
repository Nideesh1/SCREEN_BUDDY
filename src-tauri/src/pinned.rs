// pinned.rs — pinned reference library (images + PDFs + text/markdown).
//
// Reusable named sets stored locally so the user can re-feed a known set of
// reference material into the agent. Layout:
//   app_data_dir/pinned/<set_id>/<copied source files...>
//   app_data_dir/pinned/<set_id>/<derived files: *.extracted.txt, *.pN.jpg>
//   app_data_dir/pinned/<set_id>/manifest.json
//      { name, files: [ { file, kind, mode, extracted?, pages? } ] }
// Each set_id is a random hex string. Source files are copied in (not
// referenced), so the originals can move/disappear without breaking the set.
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

/// Per-file metadata in a set.
///   kind: "image" | "pdf" | "text"
///   mode: "text" | "image"  (only meaningful for pdf; resolved at ingest)
///   extracted: for a text-mode pdf, the sibling .txt filename holding the
///              extracted text layer (None => could not extract).
///   pages: for an image-mode pdf, the rendered page JPEG filenames in order.
#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    file: String,
    kind: String,
    mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extracted: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pages: Vec<String>,
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

/// Create a new set: copy each source file into the set dir, ingest any PDFs,
/// write the manifest, return the new id.
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
                file: dest_name,
                kind: "image".into(),
                mode: "image".into(),
                extracted: None,
                pages: Vec::new(),
            },
            _ => FileEntry {
                file: dest_name,
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

/// Ingest one already-copied-in PDF (hybrid auto-route, local via PDFium).
///
/// Runs PDFium text extraction; if the text layer is clean (>= ~100 chars per
/// page averaged) writes `<file>.extracted.txt` beside the original and returns
/// mode="text"; otherwise renders each page to a JPEG and returns mode="image".
/// Never panics or hard-fails: if PDFium can't bind or anything errors, returns
/// a degraded text-mode entry with no extraction (a note block at send time).
fn ingest_pdf(set_dir: &Path, pdf_file: &str) -> FileEntry {
    use pdfium_render::prelude::*;

    let degrade = || FileEntry {
        file: pdf_file.to_string(),
        kind: "pdf".into(),
        mode: "text".into(),
        extracted: None,
        pages: Vec::new(),
    };

    // Bind to the system PDFium library at call time (NOT a build requirement).
    let bindings = match Pdfium::bind_to_system_library() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[pinned] PDFium unavailable ({e}); '{pdf_file}' sent as a note");
            return degrade();
        }
    };
    let pdfium = Pdfium::new(bindings);
    let pdf_path = set_dir.join(pdf_file);
    let doc = match pdfium.load_pdf_from_file(&pdf_path, None) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[pinned] open pdf '{pdf_file}' failed: {e}");
            return degrade();
        }
    };

    let pages = doc.pages();
    let page_count = pages.len() as usize;
    if page_count == 0 {
        return degrade();
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
        let ex_name = format!("{pdf_file}.extracted.txt");
        if fs::write(set_dir.join(&ex_name), &text).is_ok() {
            return FileEntry {
                file: pdf_file.to_string(),
                kind: "pdf".into(),
                mode: "text".into(),
                extracted: Some(ex_name),
                pages: Vec::new(),
            };
        }
        eprintln!("[pinned] write extracted text for '{pdf_file}' failed");
        return degrade();
    }

    // 2) Scanned / image-only PDF: render each page to a JPEG.
    let cfg = PdfRenderConfig::new().set_target_width(1280);
    let mut page_files = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        match page.render_with_config(&cfg) {
            Ok(bitmap) => {
                let jpg = format!("{pdf_file}.p{i}.jpg");
                // .save() infers JPEG from the .jpg extension.
                if bitmap.as_image().save(set_dir.join(&jpg)).is_ok() {
                    page_files.push(jpg);
                } else {
                    eprintln!("[pinned] save rendered page {i} of '{pdf_file}' failed");
                }
            }
            Err(e) => eprintln!("[pinned] render page {i} of '{pdf_file}': {e}"),
        }
    }
    if page_files.is_empty() {
        return degrade();
    }
    FileEntry {
        file: pdf_file.to_string(),
        kind: "pdf".into(),
        mode: "image".into(),
        extracted: None,
        pages: page_files,
    }
}

/// Delete a set and all its files.
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

    let mut images = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        let data_url = match entry.kind.as_str() {
            "image" => {
                let path = set_dir.join(&entry.file);
                fs::read(&path)
                    .map(|bytes| format!("data:{};base64,{}", mime_for(&path), B64.encode(&bytes)))
                    .unwrap_or_default()
            }
            "pdf" if entry.mode == "image" => entry
                .pages
                .first()
                .and_then(|p| fs::read(set_dir.join(p)).ok())
                .map(|bytes| format!("data:image/jpeg;base64,{}", B64.encode(&bytes)))
                .unwrap_or_default(),
            _ => String::new(),
        };
        images.push(PinnedImage {
            name: entry.file.clone(),
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

    let mut blocks = Vec::new();
    for entry in &manifest.files {
        match entry.kind.as_str() {
            "image" => {
                let path = set_dir.join(&entry.file);
                if let Ok(bytes) = fs::read(&path) {
                    blocks.push(image_block(mime_for(&path), &bytes));
                }
            }
            "text" => {
                let path = set_dir.join(&entry.file);
                if let Ok(text) = fs::read_to_string(&path) {
                    blocks.push(text_doc_block(&text, &entry.file));
                }
            }
            "pdf" if entry.mode == "image" => {
                for page in &entry.pages {
                    if let Ok(bytes) = fs::read(set_dir.join(page)) {
                        blocks.push(image_block("image/jpeg", &bytes));
                    }
                }
            }
            "pdf" => {
                // text mode
                match entry
                    .extracted
                    .as_ref()
                    .and_then(|ex| fs::read_to_string(set_dir.join(ex)).ok())
                {
                    Some(text) => blocks.push(text_doc_block(&text, &entry.file)),
                    None => blocks.push(json!({
                        "type": "text",
                        "text": format!("[PDF {} — could not extract]", entry.file),
                    })),
                }
            }
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
