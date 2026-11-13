mod app;
mod config;
mod sdk;
mod theme;
mod views;

use std::sync::Arc;

use app::NayutaCastApp;
use eframe::NativeOptions;

fn load_window_icon() -> Option<egui::IconData> {
    let icon_bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(icon_bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Initialize Nayutacast SDK (FFmpeg codecs, transport runtime)
    if let Err(e) = nayutacast::startup() {
        log::warn!("Nayutacast SDK startup notice: {:?}", e);
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("NayutaCast")
        .with_inner_size([960.0, 680.0])
        .with_min_inner_size([800.0, 560.0])
        .with_decorations(false)
        .with_transparent(false);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }

    let native_options = NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "NayutaCast",
        native_options,
        Box::new(|cc| Ok(Box::new(NayutaCastApp::new(cc)))),
    );

    // Clean up Nayutacast SDK on exit
    let _ = nayutacast::shutdown();

    result
}

#[cfg(test)]
mod tests {
    use super::load_window_icon;

    #[test]
    fn window_icon_asset_decodes_cleanly() {
        let icon = load_window_icon().expect("assets/icon.png must decode cleanly");
        assert!(icon.width >= 256 && icon.height >= 256);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }
}
