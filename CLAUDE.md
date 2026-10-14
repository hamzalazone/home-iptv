# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A home IPTV head-end written in Rust. It captures/encodes video into HLS streams and serves them over HTTP, so any TV or device on the local network can watch them as "channels" — either through a generic IPTV app (via a standard M3U playlist) or through the bundled minimal web player.

A channel's source is either a looping local file playlist, or a live capture of one specific window of a running app (+ system audio) on the host Mac, identified by a window-title substring — see `capture_app`/`window_title_contains` below.

All capture/encoding work happens on the host PC; the TV side only ever needs an HTTP client — a browser, or any generic IPTV app via the M3U playlist — so the same server works across TV platforms without a native client per platform.

## Commands

- Build: `cargo build`
- Run: `cargo run` (reads `channels.toml` from the current directory by default; pass an alternate path as the first arg: `cargo run -- other-config.toml`)
- Run with request/pipeline logging: `RUST_LOG=info cargo run`
- Lint: `cargo clippy`
- Format: `cargo fmt`
- Tests: none exist yet (`cargo test` runs nothing)

`ffmpeg`/`ffprobe` must be installed and on `PATH` — invoked as subprocesses, not linked as a library. Live-capture channels (`capture_app`) additionally need `osascript` and `mkfifo` (both standard on macOS) and are macOS-only (avfoundation + AppleScript/JXA). The server fails fast at startup if a channel's playlist file doesn't exist, but does not check for `ffmpeg` until a channel is first requested.

## Architecture

Three modules, each with a single responsibility:

- **`src/config.rs`** — loads and validates `channels.toml`. A channel has an `id`, display `name`, and exactly one source: `playlist` (a list of local video file paths, looped) or `capture_app` + `window_title_contains` (the app, and which of its windows — the first whose title contains that substring — gets live-captured). Validation (files exist; exactly one source set; `window_title_contains` required whenever `capture_app` is set) happens eagerly at load time so a bad config fails at startup, not mid-stream.

- **`src/pipeline.rs`** — the `Supervisor`. Playlist channels and live-capture channels work completely differently under the hood, because of one hardware constraint: **macOS only allows one process to capture the screen at a time** (confirmed empirically — a second concurrent `avfoundation` screen capture just stalls at ~0% CPU until the first releases the device). So:

  **Playlist channels** (`spawn_ffmpeg_playlist`, via `ensure_playlist_running`/`playlist_runtime`): fully independent per-channel, started **on demand** on first request, each one `ffmpeg` process, `-stream_loop -1` over a generated `concat.txt` (absolute, quote-escaped paths), encoding to H.264 (`h264_videotoolbox`, hardware)/AAC straight to HLS (`-f hls`, 2s segments, `delete_segments` to keep old `.ts` files from piling up). GOP size is computed from the source's real framerate (via `ffprobe`) × segment length, so keyframes land exactly on segment boundaries — a fixed GOP that doesn't match the source fps produces irregular segment durations. Reaped after 60s idle (`IDLE_TIMEOUT`), independently per channel.

  **Live-capture channels** (`spawn_shared_capture`, via `ensure_capture_running`/`capture_last_access`/`capture_group`): *all* currently-watched capture channels share **one** screen-capture process, not one each — it can't be one-per-channel like playlist channels, since the OS won't allow a second one to run alongside it. That one process uses `-filter_complex` to branch the single captured frame into one `crop=W:H:X:Y[vN]` per currently-wanted channel (crop from AppleScript window bounds — see `get_window_bounds`, which finds the window by title substring — scaled by the display's Retina `backingScaleFactor` since avfoundation captures at native pixel resolution, not points; scaled down to ≤1920px wide since full Retina resolution is both wasteful and large enough to make `h264_videotoolbox` unreliable), each branch going through its own named pipe (FIFO, at `output/<id>/capture.fifo`) into its own second `ffmpeg` process that just remuxes (`-c copy`, no re-encode, ~0% CPU) into that channel's HLS output. The FIFO/remux split exists because, separately: `h264_videotoolbox` fed directly into `-f hls` from a live `avfoundation` device **stalls indefinitely** (no output, growing memory, `-t` ignored) on this machine/ffmpeg build — reproducible with or without crop/audio/B-frames/filter_complex branching; the same encode to a plain file or `-f mpegts` works fine at real-time speed, hence mpegts-into-FIFO-into-separate-remux instead.

  `reconcile_capture_group` (called on every capture-channel request, and every `reap_loop` tick) recomputes the desired set — capture channels requested within `IDLE_TIMEOUT` — and, if it differs from what the shared process currently serves (or that process died), tears the whole thing down and respawns it with exactly that set. This means: requesting a 2nd/3rd live window while others are already playing causes a brief (~1-8s) interruption of the *already-playing* ones too, since the shared process restarts from scratch — there's no way to add/remove a branch from a running ffmpeg process. Verified working in both directions (2→1 on idle, 1→2 on a new request) with the already-running channel's stream resuming cleanly afterwards (an `#EXT-X-DISCONTINUITY` tag appears in its playlist at the restart point).

  **Window capture is crop-by-screen-region, not true per-window capture**: it grabs whatever pixels are at that screen location, regardless of which window actually owns them. Two requirements follow, both unenforced by the code — true per-window capture would need a different capture API (e.g. ScreenCaptureKit) that ffmpeg's avfoundation input doesn't use: (1) windows for different capture channels must not overlap on screen, or they'll show identical/wrong content; (2) nothing else (another app, a terminal) may cover a captured window's screen region while its channel is live, or *that* gets captured instead. Confirmed directly while testing: a terminal window happening to overlap two Safari windows' screen region caused both capture channels to show the terminal, not Safari — moving the terminal away (or the target windows to an uncovered area) fixed it immediately, live, with no restart needed.

  Audio for live capture comes from **BlackHole 2ch** (a virtual audio driver, installed via `brew install blackhole-2ch`), not the real output device — the Mac's audio output must be set to a **Multi-Output Device** (built-in speakers + BlackHole, created in Audio MIDI Setup) so audio is both audible and captured. `get_window_bounds` needs **Accessibility** permission granted to the terminal process (System Settings → Privacy & Security → Accessibility); actual screen capture needs **Screen Recording** permission granted to `ffmpeg` itself (same pane) — both are one-time manual grants, not automatable.

  Device indices (`SCREEN_DEVICE_INDEX`, `AUDIO_DEVICE_INDEX`) are hardcoded from `ffmpeg -f avfoundation -list_devices true -i ""` on this machine; re-check if ever run elsewhere.

  **Do not add `-level` to the `h264_videotoolbox` args** (on either pipeline): forcing one, in any format (`"4.1"`, `"41"`), makes VideoToolbox fail encoder init with `Cannot prepare encoder: -12902`, independent of resolution/bitrate. Let it auto-select; `-profile:v high -pix_fmt yuv420p` alone is enough for broad IPTV-app compatibility.

  Output per channel lands in `output/<channel-id>/` regardless of which pipeline produced it.

- **`src/http.rs`** — axum router and the only place that talks HTTP:
  - `GET /` — serves the bundled HTML5 player (`static/index.html`, hls.js from CDN), which fetches `/playlist.m3u` client-side and lets the user pick a channel.
  - `GET /playlist.m3u` — generates a standard extended-M3U playlist (`tvg-id`/`tvg-name`/`group-title`) so any generic IPTV app can load it directly. **Content-Type is deliberately `application/octet-stream`, not `audio/x-mpegurl`** — the latter makes some browsers/apps try to hand the URL to a native HLS player instead of fetching it as plain data, which fails because this file is a channel list, not a playable stream on its own.
  - `GET /ch/:id/*file` — serves a channel's HLS manifest/segments, triggering `Supervisor::ensure_running` first. This is where on-demand startup actually happens.
  - A `log_requests` middleware logs every request (peer IP, method, URI, status) at `info` level — this was added specifically to debug client-side-only failures (see below) and is worth keeping when troubleshooting a specific TV/app.

## Known client compatibility notes

- **GSE Smart IPTV** is the known-working client for loading `/playlist.m3u` on a phone/TV.
- **IPTV Smarters** failed to load the same playlist even though server logs confirmed it was successfully fetching both `/playlist.m3u` and the per-channel `/ch/:id/stream.m3u8` (200 OK) — the failure is in that app's own handling, not the server or network. Don't spend time debugging IPTV Smarters further; prefer GSE Smart IPTV.
