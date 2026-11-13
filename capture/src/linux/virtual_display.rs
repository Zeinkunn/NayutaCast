use std::process::Command;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Display projection mode for casting output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    Mirror,
    Extend,
}

impl DisplayMode {
    pub fn from_str_lossy(s: &str) -> Self {
        if s.eq_ignore_ascii_case("extend") {
            Self::Extend
        } else {
            Self::Mirror
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Extend => "extend",
        }
    }
}

#[derive(Debug, Error)]
pub enum VirtualDisplayError {
    #[error("Failed to run compositor command: {0}")]
    Command(String),
    #[error("Failed to parse monitor JSON: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct HyprMonitorInfo {
    pub id: i64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    #[serde(rename = "refreshRate", default)]
    pub refresh_rate: f32,
    #[serde(rename = "mirrorOf", default)]
    pub mirror_of: String,
    #[serde(default)]
    pub disabled: bool,
}

pub fn parse_hypr_monitors_json(json: &str) -> Result<Vec<HyprMonitorInfo>, VirtualDisplayError> {
    serde_json::from_str::<Vec<HyprMonitorInfo>>(json)
        .map_err(|e| VirtualDisplayError::Parse(e.to_string()))
}

pub fn find_new_headless_monitor(
    before: &[HyprMonitorInfo],
    after: &[HyprMonitorInfo],
) -> Option<String> {
    after
        .iter()
        .find(|m| {
            m.name.starts_with("HEADLESS-")
                && !before.iter().any(|b| b.name == m.name)
        })
        .map(|m| m.name.clone())
}

pub fn pick_primary_monitor(
    monitors: &[HyprMonitorInfo],
    preferred: Option<&str>,
) -> Option<String> {
    if let Some(pref) = preferred {
        let trimmed = pref.trim();
        let clean = trimmed.strip_prefix("wayland_").unwrap_or(trimmed);
        if !clean.is_empty()
            && clean != "auto"
            && clean != "default"
            && let Some(found) = monitors
                .iter()
                .find(|m| m.name == clean && !m.disabled && !m.name.starts_with("HEADLESS-"))
        {
            return Some(found.name.clone());
        }
    }

    monitors
        .iter()
        .find(|m| !m.disabled && !m.name.starts_with("HEADLESS-"))
        .or_else(|| monitors.first())
        .map(|m| m.name.clone())
}

pub fn build_hypr_monitor_rule(
    virt_name: &str,
    width: u32,
    height: u32,
    fps: u8,
    mode: DisplayMode,
    primary_monitor: Option<&str>,
) -> String {
    let w = width.max(640);
    let h = height.max(480);
    let r = fps.clamp(24, 120);
    match mode {
        DisplayMode::Extend => format!("{},{}x{}@{},auto,1", virt_name, w, h, r),
        DisplayMode::Mirror => {
            if let Some(primary) = primary_monitor {
                format!("{},preferred,auto,1,mirror,{}", virt_name, primary)
            } else {
                format!("{},{}x{}@{},auto,1", virt_name, w, h, r)
            }
        }
    }
}

fn state_file_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("nayutacast-headless.state")
}

/// Manages an OS-level virtual monitor (e.g. Hyprland `HEADLESS-*`) so the
/// casted screen appears in system display settings for Mirror or Extend mode.
pub struct VirtualDisplayGuard {
    virt_output_name: Option<String>,
    primary_monitor: Option<String>,
    width: u32,
    height: u32,
    fps: u8,
    mode: DisplayMode,
}

impl VirtualDisplayGuard {
    fn cleanup_stale_output() {
        let path = state_file_path();
        if let Ok(stale) = std::fs::read_to_string(&path) {
            let trimmed = stale.trim();
            if trimmed.starts_with("HEADLESS-") {
                log::info!("Removing stale virtual display from previous crash: {}", trimmed);
                let _ = Command::new("hyprctl")
                    .args(["output", "remove", trimmed])
                    .output();
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    pub fn is_hyprland() -> bool {
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
    }

    pub fn list_hypr_monitors() -> Result<Vec<HyprMonitorInfo>, VirtualDisplayError> {
        let out = Command::new("hyprctl")
            .args(["monitors", "-j"])
            .output()
            .map_err(|e| VirtualDisplayError::Command(e.to_string()))?;
        if !out.status.success() {
            return Err(VirtualDisplayError::Command(
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        parse_hypr_monitors_json(&text)
    }

    /// Creates a virtual monitor on supported compositors (Hyprland) if `create_virtual` is true.
    /// Falls back gracefully to physical monitor selection on other desktops without failing.
    pub fn acquire(
        mode: DisplayMode,
        preferred_monitor: Option<&str>,
        width: u32,
        height: u32,
        fps: u8,
        create_virtual: bool,
    ) -> Self {
        let mut primary_monitor = preferred_monitor
            .and_then(|s| {
                let c = s.trim().strip_prefix("wayland_").unwrap_or(s.trim());
                if c.is_empty() || c == "auto" || c == "default" {
                    None
                } else {
                    Some(c.to_string())
                }
            });

        let mut virt_output_name = None;

        if Self::is_hyprland() {
            Self::cleanup_stale_output();
            if let Ok(before) = Self::list_hypr_monitors() {
                primary_monitor = pick_primary_monitor(&before, preferred_monitor);

                if create_virtual {
                    match Command::new("hyprctl")
                        .args(["output", "create", "headless"])
                        .output()
                    {
                        Ok(res) if res.status.success() => {
                            for _ in 0..10 {
                                thread::sleep(Duration::from_millis(50));
                                if let Ok(after) = Self::list_hypr_monitors() {
                                    if let Some(name) = find_new_headless_monitor(&before, &after) {
                                        log::info!(
                                            "Created virtual display {} (mode={:?}, primary={:?})",
                                            name,
                                            mode,
                                            primary_monitor
                                        );
                                        let _ = std::fs::write(state_file_path(), &name);
                                        virt_output_name = Some(name);
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(res) => {
                            log::warn!(
                                "hyprctl output create headless failed: {}",
                                String::from_utf8_lossy(&res.stderr)
                            );
                        }
                        Err(e) => {
                            log::warn!("Could not run hyprctl output create: {:?}", e);
                        }
                    }
                }
            }
        }

        let mut guard = Self {
            virt_output_name,
            primary_monitor,
            width,
            height,
            fps,
            mode,
        };
        let _ = guard.apply_mode(mode);
        guard
    }

    pub fn apply_mode(&mut self, mode: DisplayMode) -> Result<(), VirtualDisplayError> {
        self.mode = mode;
        if let Some(virt) = &self.virt_output_name {
            let rule = build_hypr_monitor_rule(
                virt,
                self.width,
                self.height,
                self.fps,
                mode,
                self.primary_monitor.as_deref(),
            );
            log::info!("Applying virtual display layout: {}", rule);
            let out = Command::new("hyprctl")
                .args(["keyword", "monitor", &rule])
                .output()
                .map_err(|e| VirtualDisplayError::Command(e.to_string()))?;
            if !out.status.success() {
                return Err(VirtualDisplayError::Command(
                    String::from_utf8_lossy(&out.stderr).to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn virtual_output_name(&self) -> Option<&str> {
        self.virt_output_name.as_deref()
    }

    pub fn mode(&self) -> DisplayMode {
        self.mode
    }

    /// Returns the preferred screen capture source ID (`wayland_<output>`).
    /// In Extend mode with a virtual output, captures the virtual display itself.
    /// In Mirror mode, captures the primary monitor directly so screen content stays live even if mirrored output is occluded.
    pub fn preferred_source_id(&self) -> Option<String> {
        match self.mode {
            DisplayMode::Extend => self
                .virt_output_name
                .as_deref()
                .or(self.primary_monitor.as_deref())
                .map(|n| format!("wayland_{}", n)),
            DisplayMode::Mirror => self
                .primary_monitor
                .as_deref()
                .or(self.virt_output_name.as_deref())
                .map(|n| format!("wayland_{}", n)),
        }
    }

    pub fn release(&mut self) {
        if let Some(name) = self.virt_output_name.take() {
            log::info!("Removing virtual display output: {}", name);
            let _ = Command::new("hyprctl")
                .args(["output", "remove", &name])
                .output();
            let _ = std::fs::remove_file(state_file_path());
        }
    }
}

impl Drop for VirtualDisplayGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_JSON: &str = r#"[
        {
            "id": 0,
            "name": "eDP-1",
            "width": 1366,
            "height": 768,
            "x": 0,
            "y": 0,
            "refreshRate": 60.012,
            "mirrorOf": "none",
            "disabled": false
        },
        {
            "id": 2,
            "name": "HDMI-A-1",
            "width": 1440,
            "height": 900,
            "x": 1366,
            "y": 0,
            "refreshRate": 59.887,
            "mirrorOf": "none",
            "disabled": false
        }
    ]"#;

    #[test]
    fn test_parse_and_detect_headless() {
        let before = parse_hypr_monitors_json(SAMPLE_JSON).unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(
            pick_primary_monitor(&before, Some("HDMI-A-1")).as_deref(),
            Some("HDMI-A-1")
        );
        assert_eq!(
            pick_primary_monitor(&before, Some("auto")).as_deref(),
            Some("eDP-1")
        );

        let after_json = r#"[
            { "id": 0, "name": "eDP-1", "width": 1366, "height": 768, "x": 0, "y": 0, "mirrorOf": "none", "disabled": false },
            { "id": 3, "name": "HEADLESS-2", "width": 1920, "height": 1080, "x": 1366, "y": 0, "mirrorOf": "none", "disabled": false }
        ]"#;
        let after = parse_hypr_monitors_json(after_json).unwrap();
        assert_eq!(
            find_new_headless_monitor(&before, &after).as_deref(),
            Some("HEADLESS-2")
        );
    }

    #[test]
    fn test_build_rules() {
        let extend = build_hypr_monitor_rule("HEADLESS-2", 1920, 1080, 60, DisplayMode::Extend, Some("eDP-1"));
        assert_eq!(extend, "HEADLESS-2,1920x1080@60,auto,1");

        let mirror = build_hypr_monitor_rule("HEADLESS-2", 1920, 1080, 60, DisplayMode::Mirror, Some("eDP-1"));
        assert_eq!(mirror, "HEADLESS-2,preferred,auto,1,mirror,eDP-1");
    }
}
