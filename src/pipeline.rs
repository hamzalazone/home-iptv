use crate::config::ChannelConfig;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};

/// How long a channel's ffmpeg pipeline is kept alive after the last request for it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the reaper checks for idle channels.
const REAP_INTERVAL: Duration = Duration::from_secs(10);
/// Max time to wait for the first HLS playlist to appear after starting a pipeline.
const STARTUP_WAIT: Duration = Duration::from_secs(8);
/// HLS segment duration target, in seconds. GOP size is derived from this and the
/// source's actual framerate so keyframes land exactly on segment boundaries
/// instead of wherever the encoder's scene-cut detector happens to land.
const HLS_SEGMENT_SECS: u32 = 2;
/// Target video bitrate, tuned for clean 1080p over a home connection. Revisit
/// once real upload-bandwidth constraints (e.g. for remote viewers) are known.
const VIDEO_BITRATE_KBPS: u32 = 6000;
const AUDIO_BITRATE_KBPS: u32 = 128;
/// Framerate requested from the screen capture device for live `capture_app` channels
/// (no source file to probe fps from, unlike playlist channels).
const CAPTURE_FRAMERATE: u32 = 30;
/// avfoundation device indices on this Mac (`ffmpeg -f avfoundation -list_devices true -i ""`):
/// "[1] Capture screen 0" and "[0] BlackHole 2ch". Re-check these if ever run on another machine.
const SCREEN_DEVICE_INDEX: &str = "1";
const AUDIO_DEVICE_INDEX: &str = "0";

struct PlaylistRuntimeState {
    process: Option<Child>,
    last_access: Instant,
}

/// The single shared screen-capture process backing *all* currently-watched
/// live `capture_app` channels at once. macOS only allows one process to
/// capture the screen at a time (confirmed empirically: a second concurrent
/// avfoundation screen capture just stalls at ~0% CPU until the first one
/// releases the device) — so instead of one capture process per channel, one
/// process captures the screen once and `-filter_complex` branches it into
/// one crop per included channel. Membership is rebuilt (process torn down
/// and restarted) whenever the set of currently-requested capture channels
/// changes, via `reconcile_capture_group`.
#[derive(Default)]
struct CaptureGroup {
    encode_process: Option<Child>,
    remux_processes: HashMap<String, Child>,
    /// Channel ids currently baked into `encode_process`'s filter graph, sorted.
    included: Vec<String>,
}

pub struct Supervisor {
    configs: HashMap<String, ChannelConfig>,
    output_root: PathBuf,
    playlist_runtime: HashMap<String, Mutex<PlaylistRuntimeState>>,
    capture_last_access: HashMap<String, Mutex<Instant>>,
    capture_group: Mutex<CaptureGroup>,
}

impl Supervisor {
    pub fn new(channels: Vec<ChannelConfig>, output_root: &std::path::Path) -> Result<Arc<Self>> {
        let mut configs = HashMap::new();
        let mut playlist_runtime = HashMap::new();
        let mut capture_last_access = HashMap::new();
        for cfg in channels {
            let dir = output_root.join(&cfg.id);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating output dir for channel {}", cfg.id))?;
            if cfg.playlist.is_some() {
                playlist_runtime.insert(
                    cfg.id.clone(),
                    Mutex::new(PlaylistRuntimeState {
                        process: None,
                        last_access: Instant::now(),
                    }),
                );
            } else {
                // Far enough in the past that a freshly-started server doesn't
                // treat every capture channel as "wanted" before anyone asks.
                capture_last_access.insert(
                    cfg.id.clone(),
                    Mutex::new(Instant::now() - IDLE_TIMEOUT * 2),
                );
            }
            configs.insert(cfg.id.clone(), cfg);
        }
        Ok(Arc::new(Self {
            configs,
            output_root: output_root.to_path_buf(),
            playlist_runtime,
            capture_last_access,
            capture_group: Mutex::new(CaptureGroup::default()),
        }))
    }

    pub fn channels(&self) -> impl Iterator<Item = &ChannelConfig> {
        self.configs.values()
    }

    pub fn channel_config(&self, id: &str) -> Option<&ChannelConfig> {
        self.configs.get(id)
    }

    pub fn output_dir(&self, id: &str) -> PathBuf {
        self.output_root.join(id)
    }

    /// Makes sure the channel's pipeline is running, starting it on demand,
    /// and refreshes its last-access timestamp so the reaper leaves it alone.
    pub async fn ensure_running(&self, id: &str) -> Result<()> {
        let cfg = self
            .configs
            .get(id)
            .with_context(|| format!("unknown channel '{id}'"))?;
        match (&cfg.playlist, &cfg.capture_app) {
            (Some(files), _) => self.ensure_playlist_running(id, cfg, files).await,
            (None, Some(_)) => self.ensure_capture_running(id).await,
            (None, None) => unreachable!("Config::load validates every channel has a source"),
        }
    }

    async fn ensure_playlist_running(
        &self,
        id: &str,
        cfg: &ChannelConfig,
        files: &[PathBuf],
    ) -> Result<()> {
        let state_lock = self
            .playlist_runtime
            .get(id)
            .expect("runtime state must exist for every configured playlist channel");
        let mut state = state_lock.lock().await;
        state.last_access = Instant::now();

        let running = match &mut state.process {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if running {
            return Ok(());
        }

        let output_dir = self.output_dir(id);
        info!(channel = id, "starting pipeline");
        let child = spawn_ffmpeg_playlist(cfg, files, &output_dir)?;
        state.process = Some(child);
        drop(state);

        wait_for_playlist(&output_dir.join("stream.m3u8"), STARTUP_WAIT).await;
        Ok(())
    }

    async fn ensure_capture_running(&self, id: &str) -> Result<()> {
        {
            let la_lock = self
                .capture_last_access
                .get(id)
                .expect("last-access entry must exist for every configured capture channel");
            let mut last_access = la_lock.lock().await;
            *last_access = Instant::now();
        }
        self.reconcile_capture_group().await?;
        wait_for_playlist(&self.output_dir(id).join("stream.m3u8"), STARTUP_WAIT).await;
        Ok(())
    }

    /// Recomputes which capture channels are currently "wanted" (requested
    /// within `IDLE_TIMEOUT`) and, if that set differs from what the shared
    /// process is currently serving, tears it down and restarts it with
    /// exactly that set of window crops.
    async fn reconcile_capture_group(&self) -> Result<()> {
        let mut desired = Vec::new();
        for (cid, la_lock) in &self.capture_last_access {
            let last_access = la_lock.lock().await;
            if last_access.elapsed() <= IDLE_TIMEOUT {
                desired.push(cid.clone());
            }
        }
        desired.sort();

        let mut group = self.capture_group.lock().await;

        let group_alive = match &mut group.encode_process {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if group.included == desired && (group_alive || desired.is_empty()) {
            return Ok(());
        }

        info!(channels = ?desired, "reconfiguring shared capture group");
        if let Some(mut child) = group.encode_process.take() {
            let _ = child.kill().await;
        }
        for (_, mut child) in group.remux_processes.drain() {
            let _ = child.kill().await;
        }
        group.included.clear();

        if desired.is_empty() {
            return Ok(());
        }

        let (encode_child, remux_children) =
            spawn_shared_capture(&self.configs, &self.output_root, &desired)?;
        group.encode_process = Some(encode_child);
        group.remux_processes = remux_children;
        group.included = desired;
        Ok(())
    }

    /// Background task: periodically kills playlist pipelines that haven't
    /// been requested in a while, and shrinks/stops the shared capture group
    /// the same way.
    pub async fn reap_loop(self: Arc<Self>) {
        loop {
            sleep(REAP_INTERVAL).await;
            for (id, state_lock) in &self.playlist_runtime {
                let mut state = state_lock.lock().await;
                if state.process.is_some() && state.last_access.elapsed() > IDLE_TIMEOUT {
                    info!(channel = id.as_str(), "idle timeout reached, stopping pipeline");
                    if let Some(mut child) = state.process.take() {
                        let _ = child.kill().await;
                    }
                }
            }
            if !self.capture_last_access.is_empty() {
                if let Err(err) = self.reconcile_capture_group().await {
                    warn!(error = ?err, "failed to reconcile capture group");
                }
            }
        }
    }
}

fn spawn_ffmpeg_playlist(
    cfg: &ChannelConfig,
    files: &[PathBuf],
    output_dir: &std::path::Path,
) -> Result<Child> {
    let concat_list = output_dir.join("concat.txt");
    let mut list_body = String::new();
    for file in files {
        let abs = std::fs::canonicalize(file)
            .with_context(|| format!("resolving playlist file {}", file.display()))?;
        let escaped = abs.display().to_string().replace('\'', r"'\''");
        list_body.push_str(&format!("file '{escaped}'\n"));
    }
    std::fs::write(&concat_list, list_body)
        .with_context(|| format!("writing concat list for channel {}", cfg.id))?;

    let playlist_path = output_dir.join("stream.m3u8");
    let segment_pattern = output_dir.join("seg_%05d.ts");

    // Drive GOP size off the source's real framerate so each keyframe lands
    // exactly on an HLS segment boundary (fixes the irregular segment
    // durations seen with a fixed GOP that didn't match the source fps).
    let fps = probe_framerate(&files[0]).unwrap_or_else(|err| {
        warn!(channel = %cfg.id, error = ?err, "framerate probe failed, assuming 30fps");
        30.0
    });
    let gop = (fps * HLS_SEGMENT_SECS as f64).round().max(1.0) as u32;
    let video_bitrate = format!("{VIDEO_BITRATE_KBPS}k");
    let max_bitrate = format!("{}k", VIDEO_BITRATE_KBPS * 11 / 10);
    let bufsize = format!("{}k", VIDEO_BITRATE_KBPS * 2);
    let audio_bitrate = format!("{AUDIO_BITRATE_KBPS}k");

    let args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-re".into(),
        "-stream_loop".into(),
        "-1".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        concat_list.display().to_string(),
        // h264_videotoolbox: Apple's hardware encoder. Massively cheaper than
        // libx264 software encoding, which is what lets us run several
        // channels at once without pegging the CPU.
        "-c:v".into(),
        "h264_videotoolbox".into(),
        // High profile + yuv420p: broadly supported by IPTV apps on modern
        // smart TVs without being needlessly conservative. Deliberately no
        // explicit `-level`: h264_videotoolbox fails to initialize at all
        // (VTCompressionSession error -12902) when a level is forced here,
        // regardless of value or format ("4.1" or "41") — let it auto-select.
        "-profile:v".into(),
        "high".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-b:v".into(),
        video_bitrate,
        "-maxrate".into(),
        max_bitrate,
        "-bufsize".into(),
        bufsize,
        "-g".into(),
        gop.to_string(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        audio_bitrate,
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        HLS_SEGMENT_SECS.to_string(),
        "-hls_list_size".into(),
        "6".into(),
        "-hls_flags".into(),
        "delete_segments+append_list".into(),
        "-hls_segment_filename".into(),
        segment_pattern.display().to_string(),
        playlist_path.display().to_string(),
    ];

    let child = Command::new("ffmpeg")
        .args(&args)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| "failed to spawn ffmpeg (is it installed and on PATH?)")?;

    Ok(child)
}

/// Reads the real framerate of a video file via `ffprobe` (e.g. "30/1" or
/// "60000/1001") so the encoder's GOP size can be aligned to it.
fn probe_framerate(path: &std::path::Path) -> Result<f64> {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running ffprobe on {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (num, den) = text
        .trim()
        .split_once('/')
        .context("unexpected ffprobe r_frame_rate format")?;
    let num: f64 = num.parse().context("parsing frame rate numerator")?;
    let den: f64 = den.parse().context("parsing frame rate denominator")?;
    if den == 0.0 {
        anyhow::bail!("ffprobe returned zero denominator frame rate");
    }
    Ok(num / den)
}

struct CaptureBranch {
    id: String,
    fifo_path: PathBuf,
    crop_expr: String,
}

/// Spawns the single shared screen-capture process (one `-filter_complex`
/// crop branch per id in `channel_ids`, each piped via its own FIFO into its
/// own HLS remux stage) backing every currently-watched live-capture channel
/// at once.
///
/// Why one process with N branches instead of N independent capture
/// processes: macOS only allows one process to actively capture the screen
/// at a time (confirmed empirically — a second concurrent avfoundation
/// screen capture just stalls at ~0% CPU until the first releases the
/// device). So all currently-watched window channels have to share a single
/// capture pass.
///
/// Why FIFOs + a separate remux stage per branch rather than each branch
/// encoding straight to `-f hls`: ffmpeg's HLS muxer stalls indefinitely (no
/// output, growing memory, `-t` ignored) when fed directly from a live
/// avfoundation device on this machine/ffmpeg build — reproducible with or
/// without crop/audio/B-frames. Plain `-f mpegts` from the same source works
/// fine and at real-time speed, so each branch encodes to mpegts into its own
/// FIFO, and a second ffmpeg process per branch just remuxes (`-c copy`) that
/// into HLS segments.
fn spawn_shared_capture(
    configs: &HashMap<String, ChannelConfig>,
    output_root: &std::path::Path,
    channel_ids: &[String],
) -> Result<(Child, HashMap<String, Child>)> {
    let scale = get_backing_scale_factor().unwrap_or_else(|err| {
        warn!(error = ?err, "couldn't read display scale factor, assuming 2.0 (Retina)");
        2.0
    });

    let mut branches = Vec::new();
    for cid in channel_ids {
        let cfg = configs
            .get(cid)
            .with_context(|| format!("unknown channel '{cid}' in capture group"))?;
        let app = cfg
            .capture_app
            .as_deref()
            .with_context(|| format!("channel '{cid}' has no capture_app"))?;
        let title = cfg
            .window_title_contains
            .as_deref()
            .with_context(|| format!("channel '{cid}' has no window_title_contains"))?;

        let (x_pt, y_pt, w_pt, h_pt) = get_window_bounds(app, title)?;
        // AppleScript reports window bounds in points; avfoundation captures
        // at the display's native pixel resolution, so the crop rect must be
        // scaled to match (e.g. x2 on a Retina display).
        let crop_x = (x_pt as f64 * scale).round() as i64;
        let crop_y = (y_pt as f64 * scale).round() as i64;
        let crop_w = (w_pt as f64 * scale).round() as i64;
        let crop_h = (h_pt as f64 * scale).round() as i64;
        if crop_w <= 0 || crop_h <= 0 {
            anyhow::bail!(
                "channel '{cid}': '{app}' window matching '{title}' has an invalid size \
                 ({crop_w}x{crop_h}) — is it open and not minimized?"
            );
        }
        // Retina screen capture is full backing-pixel resolution (e.g.
        // ~3000px wide), which is both wasted bandwidth and, above ~1920
        // wide, large enough that h264_videotoolbox's encoder init can
        // become unreliable. Cap it down to 1080p-ish, aspect preserved.
        let crop_expr = if crop_w > 1920 {
            format!("crop={crop_w}:{crop_h}:{crop_x}:{crop_y},scale=1920:-2")
        } else {
            format!("crop={crop_w}:{crop_h}:{crop_x}:{crop_y}")
        };

        let output_dir = output_root.join(cid);
        let fifo_path = output_dir.join("capture.fifo");
        let _ = std::fs::remove_file(&fifo_path); // stale FIFO from a previous run, if any
        let mkfifo_status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .context("running mkfifo (is it available on PATH?)")?;
        if !mkfifo_status.success() {
            anyhow::bail!("mkfifo failed for {}", fifo_path.display());
        }

        branches.push(CaptureBranch {
            id: cid.clone(),
            fifo_path,
            crop_expr,
        });
    }

    let gop = CAPTURE_FRAMERATE * HLS_SEGMENT_SECS;
    let video_bitrate = format!("{VIDEO_BITRATE_KBPS}k");
    let max_bitrate = format!("{}k", VIDEO_BITRATE_KBPS * 11 / 10);
    let bufsize = format!("{}k", VIDEO_BITRATE_KBPS * 2);
    let audio_bitrate = format!("{AUDIO_BITRATE_KBPS}k");

    let filter_complex = branches
        .iter()
        .enumerate()
        .map(|(i, b)| format!("[0:v]{}[v{i}]", b.crop_expr))
        .collect::<Vec<_>>()
        .join(";");

    let mut encode_args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-f".into(),
        "avfoundation".into(),
        "-capture_cursor".into(),
        "1".into(),
        "-framerate".into(),
        CAPTURE_FRAMERATE.to_string(),
        "-i".into(),
        format!("{SCREEN_DEVICE_INDEX}:{AUDIO_DEVICE_INDEX}"),
        "-filter_complex".into(),
        filter_complex,
    ];
    for (i, branch) in branches.iter().enumerate() {
        encode_args.extend([
            "-map".into(),
            format!("[v{i}]"),
            "-map".into(),
            "0:a".into(),
            "-c:v".into(),
            "h264_videotoolbox".into(),
            // No explicit `-level`: see the comment in spawn_ffmpeg_playlist —
            // h264_videotoolbox refuses to initialize when one is forced.
            "-profile:v".into(),
            "high".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-b:v".into(),
            video_bitrate.clone(),
            "-maxrate".into(),
            max_bitrate.clone(),
            "-bufsize".into(),
            bufsize.clone(),
            "-g".into(),
            gop.to_string(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            audio_bitrate.clone(),
            "-f".into(),
            "mpegts".into(),
            branch.fifo_path.display().to_string(),
        ]);
    }

    let encode_child = Command::new("ffmpeg")
        .args(&encode_args)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            "failed to spawn shared capture/encode process (is ffmpeg installed and on PATH?)"
        })?;

    let mut remux_children = HashMap::new();
    for branch in &branches {
        let output_dir = output_root.join(&branch.id);
        let playlist_path = output_dir.join("stream.m3u8");
        let segment_pattern = output_dir.join("seg_%05d.ts");
        let remux_args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "warning".into(),
            "-i".into(),
            branch.fifo_path.display().to_string(),
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            HLS_SEGMENT_SECS.to_string(),
            "-hls_list_size".into(),
            "6".into(),
            "-hls_flags".into(),
            "delete_segments+append_list".into(),
            "-hls_segment_filename".into(),
            segment_pattern.display().to_string(),
            playlist_path.display().to_string(),
        ];
        let remux_child = Command::new("ffmpeg")
            .args(&remux_args)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn HLS remux stage for channel '{}'",
                    branch.id
                )
            })?;
        remux_children.insert(branch.id.clone(), remux_child);
    }

    Ok((encode_child, remux_children))
}

/// Gets the position+size (in points) of the first window of `app_name` whose
/// title contains `title_contains`, via System Events. Requires Accessibility
/// permission granted to whatever process runs this (the terminal /
/// home-iptv binary itself).
fn get_window_bounds(app_name: &str, title_contains: &str) -> Result<(i64, i64, i64, i64)> {
    let script = format!(
        r#"tell application "System Events"
    tell process "{app_name}"
        set matchingWindows to (windows whose title contains "{title_contains}")
        if (count of matchingWindows) = 0 then
            error "no window of '{app_name}' with title containing '{title_contains}'"
        end if
        set targetWindow to item 1 of matchingWindows
        set {{wx, wy}} to position of targetWindow
        set {{ww, wh}} to size of targetWindow
        return (wx as string) & "," & (wy as string) & "," & (ww as string) & "," & (wh as string)
    end tell
end tell"#
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .context("running osascript to get window bounds")?;
    if !output.status.success() {
        anyhow::bail!(
            "osascript failed to get '{app_name}' window bounds for title containing '{title_contains}' \
             (is it running and not minimized, and is Accessibility permission granted to the terminal \
             in System Settings?): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<i64> = text
        .trim()
        .split(',')
        .map(|p| p.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing window bounds from osascript output: {text}"))?;
    if parts.len() != 4 {
        anyhow::bail!("unexpected osascript output for window bounds: {text}");
    }
    Ok((parts[0], parts[1], parts[2], parts[3]))
}

/// Gets the main display's Retina backing scale factor (e.g. 2.0), so window
/// bounds reported in points can be converted to the pixel coordinates
/// avfoundation's screen capture actually produces.
fn get_backing_scale_factor() -> Result<f64> {
    let output = std::process::Command::new("osascript")
        .args([
            "-l",
            "JavaScript",
            "-e",
            "ObjC.import('AppKit'); $.NSScreen.mainScreen.backingScaleFactor",
        ])
        .output()
        .context("running osascript (JXA) to get display scale factor")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to get backing scale factor: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("parsing backing scale factor")
}

async fn wait_for_playlist(path: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return;
        }
        sleep(Duration::from_millis(200)).await;
    }
    warn!(path = %path.display(), "playlist not ready after startup timeout");
}
