#![allow(dead_code)]

// Theme & styling definitions matching the NayutaCast mockups

use egui::{Color32, Stroke};

pub const BG_WINDOW: Color32 = Color32::from_rgb(248, 249, 254);
pub const BG_HEADER: Color32 = Color32::from_rgb(248, 249, 254);

pub const PRIMARY_BLUE: Color32 = Color32::from_rgb(79, 104, 255);
pub const PRIMARY_BLUE_HOVER: Color32 = Color32::from_rgb(67, 90, 240);
pub const PRIMARY_BLUE_LIGHT: Color32 = Color32::from_rgb(240, 243, 255);

pub const STOP_RED: Color32 = Color32::from_rgb(239, 68, 68);
pub const STOP_RED_HOVER: Color32 = Color32::from_rgb(220, 38, 38);

pub const ONLINE_GREEN: Color32 = Color32::from_rgb(16, 185, 129);
pub const STANDBY_GRAY: Color32 = Color32::from_rgb(148, 163, 184);

pub const TEXT_MAIN: Color32 = Color32::from_rgb(15, 23, 42);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(100, 116, 139);
pub const TEXT_LIGHT: Color32 = Color32::from_rgb(148, 163, 184);

pub const CARD_BG: Color32 = Color32::WHITE;
pub const CARD_BORDER: Color32 = Color32::from_rgb(226, 232, 240);
pub const DIVIDER_COLOR: Color32 = Color32::from_rgb(238, 242, 246);

pub const TRAFFIC_RED: Color32 = Color32::from_rgb(255, 95, 86);
pub const TRAFFIC_YELLOW: Color32 = Color32::from_rgb(255, 189, 46);
pub const TRAFFIC_GREEN: Color32 = Color32::from_rgb(39, 201, 63);

pub const BEZEL_BG: Color32 = Color32::from_rgb(15, 23, 42);
pub const BEZEL_BORDER: Color32 = Color32::from_rgb(16, 185, 129);

pub fn card_stroke() -> Stroke {
    Stroke::new(1.0, CARD_BORDER)
}

pub fn card_selected_stroke() -> Stroke {
    Stroke::new(1.8, PRIMARY_BLUE)
}

pub fn divider_stroke() -> Stroke {
    Stroke::new(1.0, DIVIDER_COLOR)
}
