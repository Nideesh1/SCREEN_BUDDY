//! Faithful Rust port of the capture pipeline from the Anthropic reference:
//! `computer_use/image.py` (sizing/encoding) + the `take_screenshot` / `_zoom`
//! methods of `computer_use/tools/computer.py`.
//!
//! WHY this replaces ScreenBuddy's existing capture: ScreenBuddy was built for
//! OpenAI Realtime, where it aggressively compressed screenshots to fit a live
//! WebRTC budget. For computer-use, latency is acceptable and *fidelity is the
//! product*. The model must see the screen at the exact resolution we send, or
//! its click coordinates land in a space we never observed (~14% drift). So we
//! pre-resize to the API's vision budget and remember those dimensions to scale
//! coordinates back — exactly what image.py does.
//!
//! Vision encoder facts (image.py docstring): images are tiled into 28x28
//! patches; both the long edge (<=1568px) and total tile count (<=1568) are
//! capped. Violate either and the *server* resizes again behind our back.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use xcap::image::{
    codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, ExtendedColorType,
};
use xcap::Monitor;

// Ported verbatim from constants.py.
const PX_PER_TOKEN: u32 = 28;
const MAX_EDGE_PX: u32 = 1568;
const MAX_TOKENS: u32 = 1568;
/// Hard cap on total pixels for the OFFICIAL computer-use tool: the model's
/// `computer_20251124` vision path requires the long edge <= 1568px AND total
/// <= 1,150,000px. `target_image_size` enforces both as absolute ceilings.
const MAX_TOTAL_PX: u32 = 1_150_000;
const JPEG_QUALITY: u8 = 75;
const MIN_SCREENSHOT_BYTES: usize = 1024;

/// Effective long-edge target for computer-use screenshots, tunable via
/// `CU_VISION_EDGE` (clamped to 640..=1568; default 1024 ≈ XGA-class).
///
/// WHY downscale below the 1568px ceiling: a smaller base screenshot means
/// fewer image tokens per turn (tokens scale with the 28px tile grid). The
/// official enhanced tool's built-in `zoom` action recovers fine detail on
/// demand, so we pay for low resolution by default and spend tokens on detail
/// only where the model actually needs it. MAX_EDGE_PX / MAX_TOTAL_PX remain the
/// absolute ceilings — `target_edge()` only ever lowers the edge, never raises
/// it past those caps.
fn target_edge() -> u32 {
    std::env::var("CU_VISION_EDGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&e| e >= 640 && e <= 1568)
        .unwrap_or(1024)
}

#[derive(Debug)]
pub enum CaptureError {
    NoMonitor(String),
    Capture(String),
    Encode(String),
    /// Below the sanity threshold — usually missing Screen Recording permission
    /// or a failed (black) capture. Mirrors image.py `ScreenshotTooSmall`.
    TooSmall { got: usize, min: usize },
    BadRegion(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NoMonitor(s) => write!(f, "no monitor: {s}"),
            CaptureError::Capture(s) => write!(f, "capture failed: {s}"),
            CaptureError::Encode(s) => write!(f, "encode failed: {s}"),
            CaptureError::TooSmall { got, min } => write!(
                f,
                "screenshot is {got} bytes (< {min}); Screen Recording permission missing or capture failed"
            ),
            CaptureError::BadRegion(s) => write!(f, "bad zoom region: {s}"),
        }
    }
}
impl std::error::Error for CaptureError {}

type R<T> = Result<T, CaptureError>;

/// A captured, encoded screenshot ready to send to the model.
#[derive(Debug, Clone)]
pub struct Capture {
    /// Base64 JPEG to put in an image content block.
    pub jpeg_base64: String,
    /// Dimensions of the image the model sees. Feed these into
    /// `Computer::set_screenshot_size` so click coordinates scale correctly.
    pub sent_w: u32,
    pub sent_h: u32,
    /// Logical screen size (the click coordinate space).
    pub screen_w: u32,
    pub screen_h: u32,
}

impl Capture {
    pub fn raw_jpeg_len(&self) -> usize {
        // approximate decoded length of the base64 payload
        self.jpeg_base64.len() * 3 / 4
    }
    pub fn n_tokens(&self) -> u32 {
        n_tokens_for_img(self.sent_w, self.sent_h)
    }
}

// ---- image.py port --------------------------------------------------------

fn n_tokens_for_px(px: u32) -> u32 {
    (px.max(1) - 1) / PX_PER_TOKEN + 1
}
fn n_tokens_for_img(w: u32, h: u32) -> u32 {
    n_tokens_for_px(w) * n_tokens_for_px(h)
}

/// Validity predicate for a candidate (w, h): within the effective long-edge
/// cap, the tile-count budget, AND the 1.15MP total-pixel ceiling. `edge` is the
/// effective long-edge cap (`target_edge()` clamped to MAX_EDGE_PX).
fn within_budget(w: u32, h: u32, edge: u32) -> bool {
    w <= edge
        && h <= edge
        && n_tokens_for_img(w, h) <= MAX_TOKENS
        && w.saturating_mul(h) <= MAX_TOTAL_PX
}

/// Largest (w, h) preserving aspect ratio within `target_edge()` (capped at
/// MAX_EDGE_PX), the tile-count budget, AND MAX_TOTAL_PX. Returns input
/// unchanged if already valid. Port of image.py `target_image_size`, extended
/// with the computer-use target edge + the 1.15MP total-pixel ceiling.
pub fn target_image_size(width: u32, height: u32) -> (u32, u32) {
    let edge = target_edge().min(MAX_EDGE_PX);
    if within_budget(width, height, edge) {
        return (width, height);
    }
    // Normalize to landscape for the search; transpose result back.
    if height > width {
        let (w, h) = target_image_size(height, width);
        return (h, w);
    }
    let aspect = width as f64 / height as f64;
    let (mut lo, mut hi) = (1u32, width); // lo always valid, hi always invalid
    loop {
        if lo + 1 == hi {
            return (lo, ((lo as f64 / aspect).round() as u32).max(1));
        }
        let mid_w = (lo + hi) / 2;
        let mid_h = ((mid_w as f64 / aspect).round() as u32).max(1);
        if within_budget(mid_w, mid_h, edge) {
            lo = mid_w;
        } else {
            hi = mid_w;
        }
    }
}

/// Compute the (sent_w, sent_h) we *would* send for the primary monitor without
/// capturing pixels. Lets the agent size the official computer tool's
/// `display_width_px` / `display_height_px` when an initial real capture isn't
/// available (the coordinate contract requires display dims == sent dims).
pub fn primary_sent_size() -> R<(u32, u32)> {
    let mon = primary_monitor()?;
    let w = mon.width().map_err(|e| CaptureError::Capture(e.to_string()))?;
    let h = mon
        .height()
        .map_err(|e| CaptureError::Capture(e.to_string()))?;
    Ok(target_image_size(w, h))
}

/// Resize to target size (no-op if already within budget), JPEG-encode, return
/// (base64, (sent_w, sent_h)). Port of image.py `resize_and_encode`.
fn resize_and_encode(img: DynamicImage, min_bytes: usize) -> R<(String, (u32, u32))> {
    let (tw, th) = target_image_size(img.width(), img.height());
    let img = if (tw, th) != (img.width(), img.height()) {
        img.resize_exact(tw, th, FilterType::Lanczos3)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    {
        let mut enc = JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
        enc.encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
            .map_err(|e| CaptureError::Encode(e.to_string()))?;
    }
    if buf.len() < min_bytes {
        return Err(CaptureError::TooSmall {
            got: buf.len(),
            min: min_bytes,
        });
    }
    Ok((B64.encode(&buf), (tw, th)))
}

// ---- capture --------------------------------------------------------------

fn primary_monitor() -> R<Monitor> {
    let mons = Monitor::all().map_err(|e| CaptureError::NoMonitor(e.to_string()))?;
    if mons.is_empty() {
        return Err(CaptureError::NoMonitor("no monitors found".into()));
    }
    let idx = mons
        .iter()
        .position(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(0);
    Ok(mons.into_iter().nth(idx).unwrap())
}

/// Capture the primary screen and resize/encode to the vision budget. Port of
/// computer.py `take_screenshot`.
pub fn take_screenshot() -> R<Capture> {
    let mon = primary_monitor()?;
    let screen_w = mon.width().map_err(|e| CaptureError::Capture(e.to_string()))?;
    let screen_h = mon
        .height()
        .map_err(|e| CaptureError::Capture(e.to_string()))?;

    let t_grab = std::time::Instant::now();
    let phys = mon
        .capture_image()
        .map_err(|e| CaptureError::Capture(e.to_string()))?;
    let grab = t_grab.elapsed().as_millis();
    let img = DynamicImage::ImageRgba8(phys);
    let (pw, ph) = (img.width(), img.height());

    // Resize ONCE: feed the PHYSICAL (retina) capture straight into
    // resize_and_encode, which scales physical->target (~1024) in a single
    // Lanczos3 pass. The old intermediate physical->logical resize_exact was
    // pure waste — a second full Lanczos3 pass that doesn't change the final
    // sent dims (target_image_size derives those from aspect ratio alone).
    // screen_w/screen_h stay LOGICAL (mon.width()/height()) because the
    // coordinate contract scales model coords by screen/sent, which only holds
    // if screen_* is the logical mouse/click space — independent of the
    // physical pixels we captured.
    let t_resize = std::time::Instant::now();
    let (b64, (sent_w, sent_h)) = resize_and_encode(img, MIN_SCREENSHOT_BYTES)?;
    let resize = t_resize.elapsed().as_millis();
    eprintln!("[capture] grab={grab}ms resize={resize}ms phys={pw}x{ph} sent={sent_w}x{sent_h}");
    Ok(Capture {
        jpeg_base64: b64,
        sent_w,
        sent_h,
        screen_w,
        screen_h,
    })
}

/// Result of a zoom: a sharp crop of one region at full physical resolution.
#[derive(Debug, Clone)]
pub struct Zoom {
    pub jpeg_base64: String,
    pub shown_w: u32,
    pub shown_h: u32,
    /// Human/agent note: coordinates still refer to the full screenshot.
    pub note: String,
}

/// Re-capture at full physical resolution and crop `region` ([x1,y1,x2,y2] in
/// the same image space as `sent_w`x`sent_h`), so small text is legible.
/// Port of computer.py `_zoom`. Coordinates the model uses afterwards still
/// refer to the full screenshot, not this crop.
pub fn zoom(region: [i32; 4], sent_w: u32, sent_h: u32) -> R<Zoom> {
    let [x1, y1, x2, y2] = region;
    if x2 <= x1 || y2 <= y1 {
        return Err(CaptureError::BadRegion(
            "region must have x2 > x1 and y2 > y1".into(),
        ));
    }
    let mon = primary_monitor()?;
    let full = mon
        .capture_image()
        .map_err(|e| CaptureError::Capture(e.to_string()))?;
    let (fw, fh) = (full.width(), full.height());
    let fx = fw as f64 / sent_w as f64;
    let fy = fh as f64 / sent_h as f64;

    let bx1 = ((x1 as f64 * fx).round().max(0.0) as u32).min(fw.saturating_sub(1));
    let by1 = ((y1 as f64 * fy).round().max(0.0) as u32).min(fh.saturating_sub(1));
    let bx2 = ((x2 as f64 * fx).round() as u32).min(fw);
    let by2 = ((y2 as f64 * fy).round() as u32).min(fh);
    let (cw, ch) = (bx2.saturating_sub(bx1).max(1), by2.saturating_sub(by1).max(1));

    let full_dyn = DynamicImage::ImageRgba8(full);
    let crop = full_dyn.crop_imm(bx1, by1, cw, ch);
    let (b64, (shown_w, shown_h)) = resize_and_encode(crop, 0)?;
    Ok(Zoom {
        jpeg_base64: b64,
        shown_w,
        shown_h,
        note: format!(
            "zoom of ({x1},{y1})-({x2},{y2}) in {sent_w}x{sent_h} image, shown at \
             {shown_w}x{shown_h}. Subsequent coordinates still refer to the full \
             {sent_w}x{sent_h} screenshot, not this crop."
        ),
    })
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Default target edge (1024) governs a large landscape screen: the long
    /// side is downscaled to <= target_edge and the result stays within both the
    /// 1568px hard edge and the 1.15MP total-pixel ceiling.
    #[test]
    fn downscales_large_screen_to_target_edge_and_total_cap() {
        // Env-independent: assert against whatever target_edge() resolves to,
        // but the default (no CU_VISION_EDGE) is 1024.
        let edge = target_edge().min(MAX_EDGE_PX);
        let (w, h) = target_image_size(1710, 1107);
        assert!(
            w.max(h) <= edge,
            "long side {} must be <= target edge {edge}",
            w.max(h)
        );
        assert!(w <= MAX_EDGE_PX && h <= MAX_EDGE_PX, "within hard edge");
        assert!(
            w.saturating_mul(h) <= MAX_TOTAL_PX,
            "total {} must be <= {MAX_TOTAL_PX}",
            w * h
        );
        // Aspect ratio is roughly preserved (within a couple percent).
        let src = 1710.0 / 1107.0;
        let got = w as f64 / h as f64;
        assert!((src - got).abs() / src < 0.03, "aspect preserved: {got} vs {src}");
    }

    /// A small image already within every budget is returned unchanged.
    #[test]
    fn passes_small_image_through() {
        assert_eq!(target_image_size(800, 600), (800, 600));
    }

    /// The 1.15MP total-pixel cap binds even when both edges are under 1568.
    /// 1568x1568 = 2.46MP — far above MAX_TOTAL_PX — so it must be shrunk.
    #[test]
    fn enforces_total_pixel_cap() {
        let (w, h) = target_image_size(1568, 1568);
        assert!(
            w.saturating_mul(h) <= MAX_TOTAL_PX,
            "square image must respect the 1.15MP cap, got {}",
            w * h
        );
    }
}
