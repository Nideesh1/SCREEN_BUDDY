# ScreenBuddy

A macOS app that runs Claude computer use as a real, shippable product. It drives
your Mac the way a person would (looking, clicking, typing) to do tasks that have
no API, like ordering groceries on Amazon.

The demo is easy. The point of this repo is the parts that make it production:
secure by design, reliable across an entire run, and shipped as a signed,
notarized macOS app.

## What it does

- **Computer use, locally.** A screenshot to model to action loop runs on your own
  Mac and drives the real UI.
- **Bring your own key.** Your Anthropic API key is encrypted on-device (AES-256-GCM,
  master key in the macOS Keychain) and talks directly to Anthropic. No server ever
  sees it.
- **Pinned sets.** Reference images the agent works from, loaded as a cached prompt
  prefix so long runs stay cheap.
- **Video to set.** Drop a short video and a fully on-device pipeline extracts the
  best reference frames for you to pick from. No footage leaves your machine.
- **Reliable runs.** A run keeps going even if the machine drops offline and resumes
  on reconnect.

## Stack

- **Desktop:** Rust + Tauri 2, React 19 + Vite.
- **Model:** Anthropic Claude computer use (`computer_20251124`).
- **Video pipeline:** local frame extraction (adaptive perceptual-change sampling,
  sharpness scoring) with a bundled FFmpeg sidecar. No cloud, no ML at ingest.

## Build

```bash
npm install
npm run tauri dev      # run locally
npm run tauri build    # release build (see notarization notes below)
```

### FFmpeg binaries (required for the video feature)

The FFmpeg / FFprobe sidecar binaries are **not** committed (they are large and
GPL-licensed). Before a release build, fetch them into `src-tauri/binaries/` per
`scripts/fetch-ffmpeg.md`. They are invoked as a separate subprocess, not linked,
so they do not affect this project's MIT license.

## Security model

The Mac holds the key and drives the screen. The key is encrypted at rest and sent
directly to Anthropic; nothing model-key related ever touches a server. This is the
whole reason the app is open: you can read exactly how your key is handled.

## License

This project's source is MIT licensed (see [LICENSE](LICENSE)).

Release builds bundle the FFmpeg / FFprobe command-line tools, which are GPL
licensed. They are **not** committed here and are invoked as a separate
subprocess (not linked) — "mere aggregation" — so they do not place this
MIT-licensed source under the GPL. Fetch them via `scripts/fetch-ffmpeg.md`;
see `src-tauri/binaries/LICENSE` and `SOURCE.md` for their terms.
