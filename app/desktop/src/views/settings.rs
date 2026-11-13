use std::time::Instant;

use egui::{pos2, vec2, Color32, FontId, Rect, Sense, Stroke, Ui};
use nayutacast::SourceType;

use crate::config::AppSettings;
use crate::theme::*;

pub struct SettingsAction {
    pub back: bool,
    pub saved: bool,
}

pub struct SettingsState {
    pub local: AppSettings,
    pub saved_notice_time: Option<Instant>,
    pub screen_sources: Vec<(String, String)>,
    pub audio_sources: Vec<(String, String)>,
}

impl SettingsState {
    pub fn new(settings: AppSettings) -> Self {
        let mut s = Self {
            local: settings,
            saved_notice_time: None,
            screen_sources: Vec::new(),
            audio_sources: Vec::new(),
        };
        s.refresh_sources();
        s
    }

    pub fn refresh_sources(&mut self) {
        self.screen_sources = capture::Capture::get_sources(SourceType::Screen)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
        self.audio_sources = capture::Capture::get_sources(SourceType::Audio)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
    }
}

pub fn render_settings(ui: &mut Ui, state: &mut SettingsState) -> SettingsAction {
    let mut action = SettingsAction {
        back: false,
        saved: false,
    };

    let content_w = 860.0f32.min(ui.available_width() - 56.0);
    let left_x = ui.available_rect_before_wrap().min.x + 28.0;

    ui.scope_builder(
        egui::UiBuilder::new().max_rect(Rect::from_min_size(
            pos2(left_x, ui.cursor().min.y),
            vec2(content_w, ui.available_height()),
        )),
        |ui| {
            // 1. Back button
            let back_resp = ui
                .horizontal(|ui| {
                    let (btn_rect, resp) = ui.allocate_exact_size(vec2(80.0, 24.0), Sense::click());
                    let hovered = resp.hovered();
                    if hovered {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let painter = ui.painter_at(btn_rect);
                    let color = if hovered { TEXT_MAIN } else { TEXT_MUTED };

                    let arrow_x = btn_rect.min.x + 4.0;
                    let arrow_y = btn_rect.center().y;
                    painter.line_segment(
                        [pos2(arrow_x + 5.0, arrow_y - 5.0), pos2(arrow_x, arrow_y)],
                        Stroke::new(1.8, color),
                    );
                    painter.line_segment(
                        [pos2(arrow_x, arrow_y), pos2(arrow_x + 5.0, arrow_y + 5.0)],
                        Stroke::new(1.8, color),
                    );
                    painter.text(
                        pos2(arrow_x + 10.0, arrow_y),
                        egui::Align2::LEFT_CENTER,
                        "Kembali",
                        FontId::proportional(13.0),
                        color,
                    );
                    resp
                })
                .inner;

            if back_resp.clicked() {
                action.back = true;
            }

            ui.add_space(10.0);

            // 2. Title & Subtitle
            ui.label(
                egui::RichText::new("Pengaturan")
                    .size(24.0)
                    .strong()
                    .color(TEXT_MAIN),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "Konfigurasi protokol Smart TV, layar virtual Mirror/Extend, audio, dan codec.",
                )
                .size(13.0)
                .color(TEXT_MUTED),
            );

            ui.add_space(16.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                // Section 1: JARINGAN & PROTOKOL TV
                render_section_header(ui, "JARINGAN & PROTOKOL TV");

                render_setting_row(
                    ui,
                    "Nama Perangkat",
                    "Nama identitas laptop yang tampil di receiver atau perangkat tujuan",
                    |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut state.local.device_name)
                                .desired_width(220.0),
                        );
                    },
                );

                render_setting_row(
                    ui,
                    "Protokol Casting Utama",
                    "Prioritas protokol saat menyambung ke TV (otomatis fallback jika gagal)",
                    |ui| {
                        egui::ComboBox::from_id_salt("pref_proto_select")
                            .selected_text(&state.local.preferred_protocol)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                for opt in [
                                    "Auto",
                                    "Google Cast",
                                    "DLNA Smart TV",
                                    "AirPlay",
                                    "Roku TV",
                                    "NayutaCast",
                                    "Browser",
                                ] {
                                    ui.selectable_value(
                                        &mut state.local.preferred_protocol,
                                        opt.to_string(),
                                        opt,
                                    );
                                }
                            });
                    },
                );

                render_setting_row(
                    ui,
                    "Port Transmisi NayutaCast",
                    "Port SRT untuk komunikasi ke receiver NayutaCast native (default: 8080)",
                    |ui| {
                        let mut port_str = state.local.port.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut port_str).desired_width(100.0))
                            .changed()
                        {
                            if let Ok(val) = port_str.parse::<u16>() {
                                state.local.port = val;
                            }
                        }
                    },
                );

                render_setting_row(
                    ui,
                    "Ukuran MTU",
                    "Maximum Transmission Unit paket jaringan SRT (default: 1500)",
                    |ui| {
                        let mut mtu_str = state.local.mtu.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut mtu_str).desired_width(100.0))
                            .changed()
                        {
                            if let Ok(val) = mtu_str.parse::<u32>() {
                                state.local.mtu = val;
                            }
                        }
                    },
                );

                ui.add_space(16.0);

                // Section 2: TAMPILAN, MIRROR / EXTEND & VIDEO
                render_section_header(ui, "TAMPILAN, EXTEND & VIDEO");

                render_setting_row(
                    ui,
                    "Mode Tampilan Bawaan",
                    "Mirror menduplikasi layar utama; Extend menambah desktop kedua",
                    |ui| {
                        // Packed right_to_left: add Extend first so Mirror appears on the left.
                        let is_mirror = !state.local.display_mode.eq_ignore_ascii_case("extend");
                        if ui.selectable_label(!is_mirror, "Extend").clicked() {
                            state.local.display_mode = "extend".to_string();
                        }
                        if ui.selectable_label(is_mirror, "Mirror").clicked() {
                            state.local.display_mode = "mirror".to_string();
                        }
                    },
                );

                render_setting_row(
                    ui,
                    "Layar Virtual OS (Hyprland)",
                    "Buat output monitor virtual HEADLESS agar layar TV muncul di setting display OS",
                    |ui| {
                        ui.checkbox(&mut state.local.create_virtual_display, "Aktifkan");
                    },
                );

                render_setting_row(
                    ui,
                    "Monitor Sumber Utama",
                    "Layar yang dibagikan pada mode Mirror (atau jadikan acuan utama)",
                    |ui| {
                        let selected_label = if state.local.target_monitor.eq_ignore_ascii_case("auto") {
                            "Otomatis (Layar Utama)".to_string()
                        } else {
                            state
                                .screen_sources
                                .iter()
                                .find(|(id, _)| id == &state.local.target_monitor)
                                .map(|(_, name)| name.clone())
                                .unwrap_or_else(|| state.local.target_monitor.clone())
                        };
                        egui::ComboBox::from_id_salt("monitor_src_select")
                            .selected_text(selected_label)
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.local.target_monitor,
                                    "auto".to_string(),
                                    "Otomatis (Layar Utama)",
                                );
                                for (id, name) in &state.screen_sources {
                                    ui.selectable_value(
                                        &mut state.local.target_monitor,
                                        id.clone(),
                                        name,
                                    );
                                }
                            });
                    },
                );

                render_setting_row(
                    ui,
                    "Resolusi Layar",
                    "Dimensi output video casting (Lebar × Tinggi dalam piksel)",
                    |ui| {
                        // Packed right_to_left: height first -> [width] x [height]
                        let mut h_str = state.local.resolution_height.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut h_str).desired_width(70.0))
                            .changed()
                        {
                            if let Ok(val) = h_str.parse::<u32>() {
                                state.local.resolution_height = val;
                            }
                        }
                        ui.label(egui::RichText::new("×").color(TEXT_MUTED));
                        let mut w_str = state.local.resolution_width.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut w_str).desired_width(70.0))
                            .changed()
                        {
                            if let Ok(val) = w_str.parse::<u32>() {
                                state.local.resolution_width = val;
                            }
                        }
                    },
                );

                render_setting_row(
                    ui,
                    "Frame Rate (FPS)",
                    "Kecepatan bingkai per detik saat merekam dan mentransmisikan layar",
                    |ui| {
                        // Packed right_to_left: 60 first -> 24  30  60
                        for fps in [60u8, 30u8, 24u8] {
                            let is_active = state.local.frame_rate == fps;
                            let btn_text = egui::RichText::new(format!("{} FPS", fps))
                                .size(12.0)
                                .color(if is_active { TEXT_MAIN } else { TEXT_MUTED });
                            if ui.selectable_label(is_active, btn_text).clicked() {
                                state.local.frame_rate = fps;
                            }
                        }
                    },
                );

                render_setting_row(
                    ui,
                    "Video Bitrate",
                    "Alokasi bandwidth video untuk mengontrol kualitas gambar",
                    |ui| {
                        ui.label(egui::RichText::new("Mbps").size(12.0).color(TEXT_MUTED));
                        let mut mbps_str = state.local.video_bitrate_mbps.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut mbps_str).desired_width(70.0))
                            .changed()
                        {
                            if let Ok(val) = mbps_str.parse::<u32>() {
                                state.local.video_bitrate_mbps = val;
                            }
                        }
                    },
                );

                ui.add_space(16.0);

                // Section 3: CODEC & AKSELERASI
                render_section_header(ui, "CODEC & AKSELERASI");

                render_setting_row(
                    ui,
                    "Video Encoder",
                    "Modul encoding video (VAAPI GPU otomatis fallback ke x264 jika tidak tersedia)",
                    |ui| {
                        egui::ComboBox::from_id_salt("enc_select")
                            .selected_text(&state.local.video_encoder)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.local.video_encoder,
                                    "VAAPI (Hardware)".to_string(),
                                    "VAAPI (Hardware)",
                                );
                                ui.selectable_value(
                                    &mut state.local.video_encoder,
                                    "x264 (Software CPU)".to_string(),
                                    "x264 (Software CPU)",
                                );
                            });
                    },
                );

                render_setting_row(
                    ui,
                    "Video Decoder",
                    "Modul decoding video pada mode penerima",
                    |ui| {
                        egui::ComboBox::from_id_salt("dec_select")
                            .selected_text(&state.local.video_decoder)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.local.video_decoder,
                                    "VAAPI (Hardware)".to_string(),
                                    "VAAPI (Hardware)",
                                );
                                ui.selectable_value(
                                    &mut state.local.video_decoder,
                                    "H.264 (Software)".to_string(),
                                    "H.264 (Software)",
                                );
                            });
                    },
                );

                ui.add_space(16.0);

                // Section 4: AUDIO
                render_section_header(ui, "AUDIO");

                render_setting_row(
                    ui,
                    "Sertakan Audio ke TV",
                    "Tangkap audio sistem desktop dan kirimkan bersama aliran video ke Smart TV",
                    |ui| {
                        ui.checkbox(&mut state.local.capture_audio, "Aktifkan");
                    },
                );

                render_setting_row(
                    ui,
                    "Sumber Audio",
                    "Pilih PipeWire Desktop Monitor (audio sistem) atau mikrofon tertentu",
                    |ui| {
                        let selected_label = if state.local.audio_source.eq_ignore_ascii_case("auto") {
                            "Otomatis (Audio Sistem)".to_string()
                        } else {
                            state
                                .audio_sources
                                .iter()
                                .find(|(id, _)| id == &state.local.audio_source)
                                .map(|(_, name)| name.clone())
                                .unwrap_or_else(|| state.local.audio_source.clone())
                        };
                        egui::ComboBox::from_id_salt("audio_src_select")
                            .selected_text(selected_label)
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.local.audio_source,
                                    "auto".to_string(),
                                    "Otomatis (Audio Sistem)",
                                );
                                for (id, name) in &state.audio_sources {
                                    ui.selectable_value(
                                        &mut state.local.audio_source,
                                        id.clone(),
                                        name,
                                    );
                                }
                            });
                    },
                );

                render_setting_row(
                    ui,
                    "Sample Rate",
                    "Frekuensi sampling audio sistem untuk transmisi",
                    |ui| {
                        egui::ComboBox::from_id_salt("audio_sr_select")
                            .selected_text(format!("{} Hz", state.local.audio_sample_rate))
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut state.local.audio_sample_rate,
                                    48000,
                                    "48000 Hz",
                                );
                                ui.selectable_value(
                                    &mut state.local.audio_sample_rate,
                                    44100,
                                    "44100 Hz",
                                );
                            });
                    },
                );

                render_setting_row(
                    ui,
                    "Audio Bitrate",
                    "Bandwidth streaming audio AAC (default: 128 Kbps)",
                    |ui| {
                        ui.label(egui::RichText::new("Kbps").size(12.0).color(TEXT_MUTED));
                        let mut kbps_str = state.local.audio_bitrate_kbps.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut kbps_str).desired_width(70.0))
                            .changed()
                        {
                            if let Ok(val) = kbps_str.parse::<u32>() {
                                state.local.audio_bitrate_kbps = val;
                            }
                        }
                    },
                );

                ui.add_space(24.0);

                // 3. Save Button with Confirmation Toast
                ui.horizontal(|ui| {
                    let save_btn = ui.add_sized(
                        [160.0, 36.0],
                        egui::Button::new(
                            egui::RichText::new("Simpan Pengaturan")
                                .size(13.5)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(PRIMARY_BLUE)
                        .corner_radius(8.0),
                    );

                    if save_btn.clicked() {
                        let _ = state.local.save();
                        state.saved_notice_time = Some(Instant::now());
                        action.saved = true;
                    }

                    if let Some(t) = state.saved_notice_time {
                        if t.elapsed().as_secs_f32() < 2.5 {
                            ui.label(
                                egui::RichText::new("✓ Pengaturan berhasil disimpan")
                                    .size(13.0)
                                    .color(ONLINE_GREEN),
                            );
                        }
                    }
                });

                ui.add_space(32.0);
            });
        },
    );

    action
}

fn render_section_header(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(71, 85, 105)),
    );
    ui.add_space(2.0);
    let (sep_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [sep_rect.min, pos2(sep_rect.max.x, sep_rect.min.y)],
        Stroke::new(1.0, CARD_BORDER),
    );
    ui.add_space(8.0);
}

fn render_setting_row<F>(ui: &mut Ui, label: &str, desc: &str, render_control: F)
where
    F: FnOnce(&mut Ui),
{
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 250.0).max(160.0));
            ui.label(egui::RichText::new(label).size(13.5).strong().color(TEXT_MAIN));
            ui.label(egui::RichText::new(desc).size(11.5).color(TEXT_MUTED));
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            render_control(ui);
        });
    });

    let (sep_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [sep_rect.min, pos2(sep_rect.max.x, sep_rect.min.y)],
        Stroke::new(1.0, DIVIDER_COLOR),
    );
    ui.add_space(6.0);
}
