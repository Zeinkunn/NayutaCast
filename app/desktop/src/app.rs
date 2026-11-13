use std::sync::Arc;
use std::time::Instant;

use capture::DisplayMode;
use egui::{CentralPanel, TextureHandle, TextureOptions};

use crate::config::AppSettings;
use crate::sdk::{CastingSession, DeviceManager};
use crate::theme::*;
use crate::views::*;

pub enum AppView {
    Splash { start_time: Instant },
    Scanner,
    Casting,
    Settings,
}

pub struct NayutaCastApp {
    pub view: AppView,
    pub rt: tokio::runtime::Runtime,
    pub settings: AppSettings,
    pub display_mode: DisplayMode,
    pub scanner_state: ScannerState,
    pub casting_ui_state: CastingUiState,
    pub settings_state: SettingsState,
    pub device_mgr: Arc<DeviceManager>,
    pub casting_session: Option<CastingSession>,
    pub app_icon_tex: Option<TextureHandle>,
    pub preview_tex: Option<TextureHandle>,
    pub launch_time: Instant,
    pub auto_exit_secs: Option<f32>,
}

impl NayutaCastApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("create Tokio runtime");

        let icon_bytes = include_bytes!("../assets/icon.png");
        let app_icon_tex = if let Ok(img) = image::load_from_memory(icon_bytes) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                rgba.as_raw(),
            );
            Some(cc.egui_ctx.load_texture("app_icon", color_image, TextureOptions::LINEAR))
        } else {
            None
        };

        let settings = AppSettings::load();
        let display_mode = DisplayMode::from_str_lossy(&settings.display_mode);
        let settings_state = SettingsState::new(settings.clone());

        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = BG_WINDOW;
        visuals.window_fill = BG_WINDOW;
        visuals.selection.bg_fill = PRIMARY_BLUE;
        visuals.override_text_color = Some(TEXT_MAIN);
        cc.egui_ctx.set_visuals(visuals);

        let device_mgr = Arc::new(DeviceManager::new(&settings.device_name));
        let dm_init = device_mgr.clone();
        rt.spawn(async move {
            dm_init.ensure_p2p_started().await;
            dm_init.start_scan().await;
        });

        let mut casting_session = None;
        let view = if let Some(arg) = std::env::args().nth(1) {
            if arg.contains("scanner") {
                AppView::Scanner
            } else if arg.contains("casting") {
                let target = std::env::args()
                    .nth(2)
                    .unwrap_or_else(|| "Smart TV".to_string());
                if let Ok(sess) = CastingSession::start_for_cli(
                    target,
                    "127.0.0.1".to_string(),
                    device_mgr.clone(),
                    &settings,
                    rt.handle().clone(),
                ) {
                    casting_session = Some(sess);
                }
                AppView::Casting
            } else if arg.contains("settings") {
                AppView::Settings
            } else {
                AppView::Splash {
                    start_time: Instant::now(),
                }
            }
        } else {
            AppView::Splash {
                start_time: Instant::now(),
            }
        };

        let auto_exit_secs = std::env::args()
            .position(|a| a == "--auto-exit-secs")
            .and_then(|idx| std::env::args().nth(idx + 1))
            .and_then(|v| v.parse::<f32>().ok());

        Self {
            view,
            rt,
            settings,
            display_mode,
            scanner_state: ScannerState::default(),
            casting_ui_state: CastingUiState::default(),
            settings_state,
            device_mgr,
            casting_session,
            app_icon_tex,
            preview_tex: None,
            launch_time: Instant::now(),
            auto_exit_secs,
        }
    }
}

impl eframe::App for NayutaCastApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(limit) = self.auto_exit_secs {
            if self.launch_time.elapsed().as_secs_f32() >= limit {
                if let Some(session) = &mut self.casting_session {
                    session.stop();
                }
                self.casting_session = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            ctx.request_repaint();
        }
        CentralPanel::default()
            .frame(egui::Frame::default().fill(BG_WINDOW))
            .show(ui, |ui| {
                let show_gear = matches!(self.view, AppView::Scanner | AppView::Casting);
                let header_action = render_header(ui, show_gear);

                if header_action.open_settings {
                    self.settings_state.local = self.settings.clone();
                    self.settings_state.refresh_sources();
                    self.view = AppView::Settings;
                }
                if header_action.close_app {
                    if let Some(session) = &mut self.casting_session {
                        session.stop();
                    }
                    self.casting_session = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if header_action.minimize {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }

                match &mut self.view {
                    AppView::Splash { start_time } => {
                        let elapsed = start_time.elapsed().as_secs_f32();
                        let finished = render_splash(ui, self.app_icon_tex.as_ref(), elapsed);
                        if finished {
                            self.view = AppView::Scanner;
                        }
                    }
                    AppView::Scanner => {
                        let action = render_scanner(
                            ui,
                            &mut self.scanner_state,
                            &self.device_mgr,
                            self.rt.handle(),
                            &mut self.display_mode,
                            &self.settings.preferred_protocol,
                        );
                        if let Some(target) = action.start_casting {
                            self.settings.display_mode = self.display_mode.as_str().to_string();
                            match CastingSession::start_device(
                                target,
                                self.device_mgr.clone(),
                                &self.settings,
                                self.display_mode,
                                self.rt.handle().clone(),
                            ) {
                                Ok(session) => {
                                    self.casting_ui_state = CastingUiState::default();
                                    self.preview_tex = None;
                                    self.casting_session = Some(session);
                                    self.view = AppView::Casting;
                                }
                                Err(e) => {
                                    log::error!("Failed to start casting session: {:?}", e);
                                }
                            }
                        }
                    }
                    AppView::Casting => {
                        if let Some(session) = &mut self.casting_session {
                            let action = render_casting(
                                ui,
                                session,
                                &mut self.casting_ui_state,
                                &mut self.preview_tex,
                            );
                            if let Some(new_mode) = action.switch_display_mode {
                                if let Err(e) = session.set_display_mode(new_mode) {
                                    log::warn!("Live Mirror/Extend switch failed: {:?}", e);
                                } else {
                                    self.display_mode = new_mode;
                                }
                            }
                            if action.use_browser_fallback {
                                session.switch_to_browser_mode();
                            }
                            if let Some(excluded) = action.retry_other_protocols {
                                session.retry_other_protocols(excluded);
                            }
                            if action.stop_casting {
                                session.stop();
                                self.casting_session = None;
                                self.preview_tex = None;
                                self.view = AppView::Scanner;
                            }
                        } else {
                            self.view = AppView::Scanner;
                        }
                    }
                    AppView::Settings => {
                        let action = render_settings(ui, &mut self.settings_state);
                        if action.saved {
                            self.settings = self.settings_state.local.clone();
                            self.display_mode = DisplayMode::from_str_lossy(&self.settings.display_mode);
                            let dm = self.device_mgr.clone();
                            let new_name = self.settings.device_name.clone();
                            self.rt.spawn(async move {
                                dm.set_local_name(&new_name).await;
                            });
                        }
                        if action.back {
                            self.settings = self.settings_state.local.clone();
                            self.display_mode = DisplayMode::from_str_lossy(&self.settings.display_mode);
                            self.view = AppView::Scanner;
                        }
                    }
                }
            });
    }

    fn on_exit(&mut self) {
        if let Some(session) = &mut self.casting_session {
            session.stop();
        }
        self.casting_session = None;
    }
}
