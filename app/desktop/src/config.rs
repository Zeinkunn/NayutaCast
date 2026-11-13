use std::fs;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub device_name: String,
    pub port: u16,
    pub mtu: u32,
    pub resolution_width: u32,
    pub resolution_height: u32,
    pub frame_rate: u8,
    pub video_bitrate_mbps: u32,
    pub video_encoder: String,
    pub video_decoder: String,
    pub audio_sample_rate: u32,
    pub audio_bitrate_kbps: u32,
    #[serde(default = "default_display_mode")]
    pub display_mode: String,
    #[serde(default = "default_auto")]
    pub target_monitor: String,
    #[serde(default = "default_true")]
    pub create_virtual_display: bool,
    #[serde(default = "default_true")]
    pub capture_audio: bool,
    #[serde(default = "default_auto")]
    pub audio_source: String,
    #[serde(default = "default_preferred_protocol")]
    pub preferred_protocol: String,
    #[serde(default)]
    pub wayland_restore_token: Option<String>,
}

fn default_display_mode() -> String {
    "mirror".to_string()
}

fn default_auto() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_preferred_protocol() -> String {
    "Auto".to_string()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            device_name: "NayutaCast Device".to_string(),
            port: 8080,
            mtu: 1500,
            resolution_width: 1920,
            resolution_height: 1080,
            frame_rate: 60,
            video_bitrate_mbps: 10,
            video_encoder: "VAAPI (Hardware)".to_string(),
            video_decoder: "VAAPI (Hardware)".to_string(),
            audio_sample_rate: 48000,
            audio_bitrate_kbps: 128,
            display_mode: default_display_mode(),
            target_monitor: default_auto(),
            create_virtual_display: true,
            capture_audio: true,
            audio_source: default_auto(),
            preferred_protocol: default_preferred_protocol(),
            wayland_restore_token: None,
        }
    }
}

impl AppSettings {
    pub fn config_file_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("nayutacast");
        let _ = fs::create_dir_all(&dir);
        dir.join("settings.json")
    }

    pub fn load() -> Self {
        let path = Self::config_file_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(settings) = serde_json::from_str::<AppSettings>(&content) {
                    return settings;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_file_path();
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }
}
