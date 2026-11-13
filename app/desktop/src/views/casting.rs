use std::time::Instant;

use capture::DisplayMode;
use egui::{pos2, vec2, Color32, FontId, Rect, Sense, Stroke, TextureHandle, TextureOptions, Ui};

use crate::sdk::cast::CastingState;
use crate::sdk::casting::CastingSession;
use crate::theme::*;

pub struct CastingAction {
    pub stop_casting: bool,
    pub switch_display_mode: Option<DisplayMode>,
    pub use_browser_fallback: bool,
    pub retry_other_protocols: Option<Vec<String>>,
}

#[derive(Default)]
pub struct CastingUiState {
    pub copied_at: Option<Instant>,
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else if bytes > 0 {
        format!("{} B", bytes)
    } else {
        "Live".to_string()
    }
}

pub fn render_casting(
    ui: &mut Ui,
    session: &CastingSession,
    ui_state: &mut CastingUiState,
    preview_tex: &mut Option<TextureHandle>,
) -> CastingAction {
    // Continuously repaint for pulsing status dot and live screen preview frames
    ui.ctx().request_repaint();

    let mut action = CastingAction {
        stop_casting: false,
        switch_display_mode: None,
        use_browser_fallback: false,
        retry_other_protocols: None,
    };

    let cast_state = session.get_state();
    let is_failed = matches!(cast_state, CastingState::Failed { .. });
    let is_waiting = matches!(cast_state, CastingState::WaitingForReceiver { .. });

    let available_w = ui.available_width();
    let available_h = ui.available_height();

    // Reserve extra space when error / browser URL actions are shown below the monitor
    let reserve_h = if is_failed {
        195.0
    } else if is_waiting {
        175.0
    } else {
        150.0
    };
    let max_monitor_h = (available_h - reserve_h).clamp(115.0, 260.0);
    let max_monitor_w = (available_w - 48.0).max(200.0);

    let aspect = 16.0 / 10.0;
    let mut monitor_h = max_monitor_h;
    let mut monitor_w = monitor_h * aspect;

    if monitor_w > max_monitor_w {
        monitor_w = max_monitor_w;
        monitor_h = monitor_w / aspect;
    }

    // Update texture from live capture frame buffer
    {
        let frame_guard = session.frame_buffer.lock();
        if let Some(color_image) = frame_guard.as_ref() {
            if let Some(tex) = preview_tex {
                tex.set(color_image.clone(), TextureOptions::LINEAR);
            } else {
                *preview_tex = Some(ui.ctx().load_texture(
                    "live_preview",
                    color_image.clone(),
                    TextureOptions::LINEAR,
                ));
            }
        }
    }

    let (dot_color, bezel_glow, border_color) = match &cast_state {
        CastingState::Connected { .. } => (
            ONLINE_GREEN,
            Color32::from_rgba_premultiplied(16, 185, 129, 28),
            BEZEL_BORDER,
        ),
        CastingState::Connecting { .. } | CastingState::Idle => (
            Color32::from_rgb(245, 158, 11),
            Color32::from_rgba_premultiplied(245, 158, 11, 24),
            Color32::from_rgb(245, 158, 11),
        ),
        CastingState::WaitingForReceiver { .. } => (
            PRIMARY_BLUE,
            Color32::from_rgba_premultiplied(79, 104, 255, 28),
            PRIMARY_BLUE,
        ),
        CastingState::Failed { .. } => (
            STOP_RED,
            Color32::from_rgba_premultiplied(239, 68, 68, 22),
            STOP_RED,
        ),
    };

    ui.vertical_centered(|ui| {
        ui.add_space(12.0);

        // 1. Monitor Bezel Frame
        let (bezel_rect, _) = ui.allocate_exact_size(vec2(monitor_w, monitor_h), Sense::hover());
        let painter = ui.painter_at(bezel_rect);

        painter.rect_filled(bezel_rect.expand(6.0), 22.0, bezel_glow);
        painter.rect(
            bezel_rect,
            20.0,
            BEZEL_BG,
            Stroke::new(2.5, border_color),
            egui::StrokeKind::Inside,
        );

        let screen_rect = bezel_rect.shrink(6.0);
        painter.rect_filled(screen_rect, 14.0, Color32::from_rgb(15, 23, 42));

        if let Some(tex) = preview_tex {
            painter.image(
                tex.id(),
                screen_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let center = screen_rect.center();
            let elapsed = session.start_time.elapsed().as_secs_f32();
            let wave = (elapsed * 3.0).sin().abs();

            painter.circle_stroke(
                center,
                24.0 + wave * 12.0,
                Stroke::new(
                    1.8,
                    Color32::from_rgba_premultiplied(dot_color.r(), dot_color.g(), dot_color.b(), 120),
                ),
            );
            painter.circle_filled(center, 7.0, dot_color);
            painter.text(
                pos2(center.x, center.y + 42.0),
                egui::Align2::CENTER_CENTER,
                "Memulai capture layar realtime...",
                FontId::proportional(13.5),
                Color32::from_rgb(148, 163, 184),
            );
        }

        ui.add_space(12.0);

        // 2. Honest Status Line based on CastingState (concise so it never clips)
        let elapsed = session.start_time.elapsed().as_secs_f32();
        let pulse_r = if is_failed {
            5.0
        } else {
            4.5 + (elapsed * 3.5).sin().abs() * 1.8
        };

        let main_line = match &cast_state {
            CastingState::Idle => format!("Menyiapkan koneksi ke {}...", session.target_name),
            CastingState::Connecting { protocol, message } => {
                format!("{} · {}", protocol, message)
            }
            CastingState::WaitingForReceiver { protocol, message, .. } => {
                if protocol == "Browser TV" {
                    "Browser TV · Menunggu Smart TV membuka tautan".to_string()
                } else {
                    format!("{} · {}", protocol, message)
                }
            }
            CastingState::Connected {
                protocol,
                target_name,
                ..
            } => format!("Casting to {} · {}", target_name, protocol),
            CastingState::Failed { .. } => {
                format!("Gagal menyambung ke {}", session.target_name)
            }
        };

        let font_id = FontId::proportional(16.0);
        let galley = ui
            .painter()
            .layout_no_wrap(main_line.clone(), font_id.clone(), TEXT_MAIN);
        let max_line_w = (available_w - 40.0).max(200.0);
        let row_w = (22.0 + galley.size().x).min(max_line_w);
        let row_h = 26.0;

        let (row_rect, _) = ui.allocate_exact_size(vec2(row_w, row_h), Sense::hover());
        let painter = ui.painter_at(row_rect);

        let dot_center = pos2(row_rect.min.x + 6.0, row_rect.center().y);
        if !is_failed {
            painter.circle_filled(
                dot_center,
                pulse_r + 2.0,
                Color32::from_rgba_premultiplied(dot_color.r(), dot_color.g(), dot_color.b(), 45),
            );
        }
        painter.circle_filled(dot_center, 5.0, dot_color);
        painter.text(
            pos2(row_rect.min.x + 22.0, row_rect.center().y),
            egui::Align2::LEFT_CENTER,
            main_line,
            font_id,
            if is_failed { STOP_RED } else { TEXT_MAIN },
        );

        ui.add_space(4.0);

        // 3. Detailed Subline & Recovery / Quick-Mode Actions (Centered)
        match &cast_state {
            CastingState::Connected {
                bytes_sent,
                active_clients,
                browser_url,
                ..
            } => {
                let mut detail_parts = vec![format!("{} penerima aktif", active_clients.max(&1))];
                if *bytes_sent > 0 {
                    detail_parts.push(format_bytes(*bytes_sent));
                }
                if let Some(virt) = session.virtual_output_name() {
                    let mode_label = match session.display_mode() {
                        DisplayMode::Mirror => "Mirror",
                        DisplayMode::Extend => "Extend",
                    };
                    detail_parts.push(format!("Layar OS: {} ({})", virt, mode_label));
                }
                ui.label(
                    egui::RichText::new(detail_parts.join(" · "))
                        .size(12.5)
                        .color(TEXT_MUTED),
                );
                let _ = browser_url;
            }
            CastingState::WaitingForReceiver { browser_url, .. } => {
                if !browser_url.is_empty() {
                    let url_label = format!("Tautan Browser TV: {}", browser_url);
                    let url_galley = ui.painter().layout_no_wrap(
                        url_label.clone(),
                        FontId::proportional(12.5),
                        PRIMARY_BLUE,
                    );
                    let box_w = (url_galley.size().x + 92.0).min(available_w - 32.0);
                    ui.allocate_ui(vec2(box_w, 26.0), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(url_label).size(12.5).color(PRIMARY_BLUE));
                            if ui.button("Salin Link").clicked() {
                                ui.ctx().copy_text(browser_url.clone());
                                ui_state.copied_at = Some(Instant::now());
                            }
                        });
                    });
                }
            }
            CastingState::Failed {
                error,
                tried_protocols,
                browser_url,
            } => {
                let tried_txt = if tried_protocols.is_empty() {
                    String::new()
                } else {
                    format!("Dicoba: {} · ", tried_protocols.join(", "))
                };
                ui.label(
                    egui::RichText::new(format!("{}{}", tried_txt, error))
                        .size(12.0)
                        .color(TEXT_MUTED),
                );
                ui.add_space(8.0);
                let retry_box_w = if browser_url.is_empty() { 340.0 } else { 430.0f32.min(available_w - 32.0) };
                ui.allocate_ui(vec2(retry_box_w, 32.0), |ui| {
                    ui.horizontal(|ui| {
                        let retry_btn = egui::Button::new(
                            egui::RichText::new("Coba Protokol Lain")
                                .size(12.5)
                                .color(Color32::WHITE),
                        )
                        .fill(PRIMARY_BLUE)
                        .corner_radius(8.0);
                        if ui.add(retry_btn).clicked() {
                            action.retry_other_protocols = Some(tried_protocols.clone());
                        }

                        let browser_btn = egui::Button::new(
                            egui::RichText::new("Buka Mode Browser (/watch)")
                                .size(12.5)
                                .color(TEXT_MAIN),
                        )
                        .fill(Color32::from_rgb(238, 241, 246))
                        .corner_radius(8.0);
                        if ui.add(browser_btn).clicked() {
                            action.use_browser_fallback = true;
                        }

                        if !browser_url.is_empty() && ui.button("Salin Link").clicked() {
                            ui.ctx().copy_text(browser_url.clone());
                            ui_state.copied_at = Some(Instant::now());
                        }
                    });
                });
            }
            CastingState::Connecting { .. } | CastingState::Idle => {
                if let Some(virt) = session.virtual_output_name() {
                    let mode_label = match session.display_mode() {
                        DisplayMode::Mirror => "Mirror",
                        DisplayMode::Extend => "Extend",
                    };
                    ui.label(
                        egui::RichText::new(format!("Layar OS: {} ({})", virt, mode_label))
                            .size(12.0)
                            .color(TEXT_MUTED),
                    );
                }
            }
        }

        if let Some(copied) = ui_state.copied_at {
            if copied.elapsed().as_secs_f32() < 2.0 {
                ui.label(
                    egui::RichText::new("✓ Link disalin ke clipboard")
                        .size(11.5)
                        .color(ONLINE_GREEN),
                );
            }
        }

        ui.add_space(10.0);

        // 4. Bottom Action Bar (Centered)
        let bar_w = if is_failed { 160.0 } else { 324.0 };
        ui.allocate_ui(vec2(bar_w, 38.0), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let cur_mode = session.display_mode();
                if !is_failed {
                    let sw_w = 152.0;
                    let sw_h = 38.0;
                    let (sw_rect, _) = ui.allocate_exact_size(vec2(sw_w, sw_h), Sense::hover());
                    let painter = ui.painter_at(sw_rect);
                    painter.rect_filled(sw_rect, 10.0, Color32::from_rgb(238, 241, 246));

                    let hw = sw_w / 2.0;
                    let m_rect = Rect::from_min_size(
                        sw_rect.min + vec2(2.0, 2.0),
                        vec2(hw - 3.0, sw_h - 4.0),
                    );
                    let e_rect = Rect::from_min_size(
                        sw_rect.min + vec2(hw + 1.0, 2.0),
                        vec2(hw - 3.0, sw_h - 4.0),
                    );

                    let is_mirror = cur_mode == DisplayMode::Mirror;
                    painter.rect_filled(if is_mirror { m_rect } else { e_rect }, 8.0, Color32::WHITE);

                    let mh = ui.rect_contains_pointer(m_rect);
                    let eh = ui.rect_contains_pointer(e_rect);
                    if mh || eh {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if mh
                        && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
                        && !is_mirror
                    {
                        action.switch_display_mode = Some(DisplayMode::Mirror);
                    }
                    if eh
                        && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary))
                        && is_mirror
                    {
                        action.switch_display_mode = Some(DisplayMode::Extend);
                    }

                    painter.text(
                        m_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Mirror",
                        FontId::proportional(12.5),
                        if is_mirror { PRIMARY_BLUE } else { TEXT_MUTED },
                    );
                    painter.text(
                        e_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Extend",
                        FontId::proportional(12.5),
                        if !is_mirror { PRIMARY_BLUE } else { TEXT_MUTED },
                    );
                }

                let btn_w = 160.0;
                let btn_h = 38.0;
                let (btn_rect, btn_resp) =
                    ui.allocate_exact_size(vec2(btn_w, btn_h), Sense::click());
                let btn_hovered = btn_resp.hovered();
                if btn_hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let bg_color = if btn_hovered {
                    STOP_RED_HOVER
                } else {
                    STOP_RED
                };
                let painter = ui.painter_at(btn_rect);

                if btn_hovered {
                    painter.rect_filled(
                        btn_rect.translate(vec2(0.0, 2.0)),
                        12.0,
                        Color32::from_rgba_premultiplied(239, 68, 68, 55),
                    );
                }
                painter.rect_filled(btn_rect, 10.0, bg_color);

                if is_failed {
                    painter.text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Kembali",
                        FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                } else {
                    let icon_center = pos2(btn_rect.center().x - 44.0, btn_rect.center().y);
                    painter.circle_stroke(icon_center, 6.5, Stroke::new(1.6, Color32::WHITE));
                    painter.rect_filled(
                        Rect::from_center_size(icon_center, vec2(5.5, 5.5)),
                        1.0,
                        Color32::WHITE,
                    );
                    painter.text(
                        pos2(btn_rect.center().x + 8.0, btn_rect.center().y),
                        egui::Align2::CENTER_CENTER,
                        "Stop Casting",
                        FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                }

                if btn_resp.clicked() {
                    action.stop_casting = true;
                }
            });
        });
    });

    action
}
