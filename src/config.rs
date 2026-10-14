use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct ChannelConfig {
    pub id: String,
    pub name: String,
    /// Loop these local files as the channel's source. Mutually exclusive with `capture_app`.
    #[serde(default)]
    pub playlist: Option<Vec<PathBuf>>,
    /// Instead of a playlist, live-capture this app's window (+ system audio
    /// via BlackHole) as the channel's source. Mutually exclusive with `playlist`.
    #[serde(default)]
    pub capture_app: Option<String>,
    /// Which of `capture_app`'s windows to capture: the first whose title
    /// contains this substring. Required when `capture_app` is set — needed
    /// to tell multiple windows of the same app apart.
    #[serde(default)]
    pub window_title_contains: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default, rename = "channels")]
    pub channels: Vec<ChannelConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        for ch in &cfg.channels {
            match (&ch.playlist, &ch.capture_app) {
                (Some(files), None) => {
                    if files.is_empty() {
                        anyhow::bail!("channel '{}' has an empty playlist", ch.id);
                    }
                    for file in files {
                        if !file.exists() {
                            anyhow::bail!(
                                "channel '{}': playlist file not found: {}",
                                ch.id,
                                file.display()
                            );
                        }
                    }
                }
                (None, Some(app)) => {
                    if app.trim().is_empty() {
                        anyhow::bail!("channel '{}': capture_app is empty", ch.id);
                    }
                    match &ch.window_title_contains {
                        Some(t) if !t.trim().is_empty() => {}
                        _ => anyhow::bail!(
                            "channel '{}': capture_app requires a non-empty window_title_contains",
                            ch.id
                        ),
                    }
                }
                (Some(_), Some(_)) => anyhow::bail!(
                    "channel '{}': set either 'playlist' or 'capture_app', not both",
                    ch.id
                ),
                (None, None) => anyhow::bail!(
                    "channel '{}': must set either 'playlist' or 'capture_app'",
                    ch.id
                ),
            }
        }
        Ok(cfg)
    }
}
