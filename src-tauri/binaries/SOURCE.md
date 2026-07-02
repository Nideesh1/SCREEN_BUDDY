# Bundled ffmpeg / ffprobe — source & license (GPL compliance)

The video → set ingestion feature (`extract_frames_from_video`) invokes these
binaries as a **separate subprocess** (a Tauri sidecar), not by linking against
the FFmpeg libraries. This is mere aggregation: no FFmpeg copyleft propagates to
ScreenBuddy's own source. We still redistribute the FFmpeg binaries, so we ship
FFmpeg's license and cite the exact source below.

## What is bundled

| File | Arch | FFmpeg version | Build config |
|------|------|----------------|--------------|
| `ffmpeg-aarch64-apple-darwin`  | arm64  | 8.1 | `--enable-gpl` (GPLv3+), **no** `--enable-nonfree` |
| `ffprobe-aarch64-apple-darwin` | arm64  | 8.1 | same |
| `ffmpeg-x86_64-apple-darwin`   | x86_64 | 8.0 | `--enable-gpl` (GPLv3+), **no** `--enable-nonfree` |
| `ffprobe-x86_64-apple-darwin`  | x86_64 | 8.0 | same |

These are **GPL** static builds (`--enable-gpl --enable-version3`). They are
deliberately NOT `--enable-nonfree` builds — a `--enable-nonfree` FFmpeg may not
be redistributed at all, so it is unusable for a shipped app. `LICENSE` in this
directory is FFmpeg's `COPYING.GPLv3` (the effective license of these builds).

## Source of the prebuilt binaries

- **arm64 (Apple Silicon):** https://www.osxexperts.net/ — `ffmpeg81arm.zip`,
  `ffprobe81arm.zip` (FFmpeg 8.1, GPL, static).
- **x86_64 (Intel):** https://www.osxexperts.net/ — `ffmpeg80intel.zip`,
  `ffprobe80intel.zip` (FFmpeg 8.0, GPL, static).

Verify before shipping:

```bash
file  ffmpeg-aarch64-apple-darwin                 # -> Mach-O ... arm64
./ffmpeg-aarch64-apple-darwin -version | grep -o 'enable-gpl\|nonfree'
#   -> enable-gpl   (and NOT nonfree)
```

## Corresponding source code (GPLv3 §6)

The complete corresponding source for these builds is the upstream FFmpeg release
at the matching tag:

- FFmpeg 8.1: https://github.com/FFmpeg/FFmpeg/tree/n8.1  (also https://ffmpeg.org/releases/ffmpeg-8.1.tar.xz)
- FFmpeg 8.0: https://github.com/FFmpeg/FFmpeg/tree/n8.0  (also https://ffmpeg.org/releases/ffmpeg-8.0.tar.xz)

To rebuild an equivalent clean-GPL binary from source, see
`../../scripts/fetch-ffmpeg.md`.

## Notarization

The four binaries are signed + notarized as part of the macOS app bundle using
the existing Developer ID identity in `tauri.conf.json`; `entitlements.plist`
covers the child process. Re-run notarization whenever these are updated.
