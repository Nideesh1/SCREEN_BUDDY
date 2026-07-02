# Bundling ffmpeg / ffprobe as a Tauri sidecar

The video → set ingestion feature (`extract_frames_from_video`) shells out to
`ffmpeg` + `ffprobe`. They are bundled as a **Tauri sidecar** so the shipped app
carries its own binaries. **There is NO `$PATH` fallback** — downloaders are not
assumed to have ffmpeg installed; `video.rs::resolve_bin` uses ONLY the bundled
sidecar and errors clearly if it is missing.

## What ships today

Real **GPL** static builds are already committed in `src-tauri/binaries/`
(source + versions in `SOURCE.md`, GPL text in `LICENSE`):

| File | Arch | Version |
|------|------|---------|
| `ffmpeg-aarch64-apple-darwin`  / `ffprobe-aarch64-apple-darwin`  | arm64  | 8.1 |
| `ffmpeg-x86_64-apple-darwin`   / `ffprobe-x86_64-apple-darwin`   | x86_64 | 8.0 |

We invoke ffmpeg as a separate subprocess (not linked), so bundling a GPL binary
is mere aggregation and does not impose copyleft on ScreenBuddy's own source.
The builds are `--enable-gpl` **without** `--enable-nonfree` — a
`--enable-nonfree` FFmpeg may not be redistributed at all, so it is never usable.

## Re-fetching the prebuilt binaries

Source: https://www.osxexperts.net/ (GPL static macOS builds).

```bash
cd src-tauri/binaries
# arm64 (Apple Silicon)
curl -L -o ffmpeg81arm.zip  https://www.osxexperts.net/ffmpeg81arm.zip
curl -L -o ffprobe81arm.zip https://www.osxexperts.net/ffprobe81arm.zip
unzip -o ffmpeg81arm.zip  && mv ffmpeg  ffmpeg-aarch64-apple-darwin
unzip -o ffprobe81arm.zip && mv ffprobe ffprobe-aarch64-apple-darwin
# x86_64 (Intel)
curl -L -o ffmpeg80intel.zip  https://www.osxexperts.net/ffmpeg80intel.zip
curl -L -o ffprobe80intel.zip https://www.osxexperts.net/ffprobe80intel.zip
unzip -o ffmpeg80intel.zip  && mv ffmpeg  ffmpeg-x86_64-apple-darwin
unzip -o ffprobe80intel.zip && mv ffprobe ffprobe-x86_64-apple-darwin
chmod +x ffmpeg-*-apple-darwin ffprobe-*-apple-darwin
rm -f *.zip; rm -rf __MACOSX
# verify: correct arch + GPL (and NOT nonfree)
file ./ffmpeg-aarch64-apple-darwin
./ffmpeg-aarch64-apple-darwin -version | grep -o 'enable-gpl\|nonfree'
```

Tauri's `externalBin: ["binaries/ffmpeg", "binaries/ffprobe"]` resolves each
entry to the triple-suffixed file at build time and copies it next to the app
executable (triple stripped) at runtime, where `resolve_bin` finds it.

## Rebuilding a clean-GPL ffmpeg from source (GPLv3 §6 corresponding source)

```bash
brew install nasm pkg-config
git clone --depth 1 --branch n8.1 https://github.com/FFmpeg/FFmpeg.git ffmpeg-src
cd ffmpeg-src
./configure \
  --prefix="$PWD/out" \
  --enable-gpl --enable-version3 --disable-nonfree \
  --enable-static --disable-shared \
  --disable-doc --disable-ffplay \
  --arch=arm64
make -j"$(sysctl -n hw.ncpu)"
TRIPLE=aarch64-apple-darwin
cp ffmpeg  ../src-tauri/binaries/ffmpeg-$TRIPLE
cp ffprobe ../src-tauri/binaries/ffprobe-$TRIPLE
chmod +x  ../src-tauri/binaries/{ffmpeg,ffprobe}-$TRIPLE
```

## Notarization / entitlements

The macOS bundle signs + notarizes with the existing Developer ID identity in
`tauri.conf.json`; the sidecar binaries are signed as part of the app bundle and
`entitlements.plist` covers the child process. Re-run notarization after updating
the binaries.

## Bundle-size impact

The four binaries total ~260 MB on disk unpacked (arm64 ffmpeg+ffprobe ~52 MB
each; x86_64 ~78 MB each). A per-arch `.app` only carries its own two (~104 MB
arm64 / ~156 MB x86_64). Trim by disabling unused encoders/muxers in
`./configure` — this pipeline only needs decode + the `scene`, `fps`, `showinfo`
and image2 (PNG/JPEG) paths.
