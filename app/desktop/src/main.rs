mod app;
mod config;
mod sdk;
mod theme;
mod views;

use app::NayutaCastApp;
use eframe::NativeOptions;

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Initialize Nayutacast SDK (FFmpeg codecs, transport runtime)
    if let Err(e) = nayutacast::startup() {
        log::warn!("Nayutacast SDK startup notice: {:?}", e);
    }

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NayutaCast")
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([800.0, 560.0])
            .with_decorations(false)
            .with_transparent(false),
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
