# Sidecar binaries

Real GPL static `ffmpeg` / `ffprobe` binaries for the video → set ingestion
feature, bundled as Tauri sidecars (`externalBin` in `tauri.conf.json`), named
with the target triple:

- `ffmpeg-aarch64-apple-darwin`, `ffprobe-aarch64-apple-darwin` (arm64)
- `ffmpeg-x86_64-apple-darwin`, `ffprobe-x86_64-apple-darwin` (x86_64)

There is **no `$PATH` fallback**: `video.rs::resolve_bin` uses ONLY the bundled
sidecar and errors clearly if it is missing. See `SOURCE.md` for versions /
source / GPL compliance, `LICENSE` for the GPL text, and `../../scripts/fetch-ffmpeg.md`
to rebuild from source.
