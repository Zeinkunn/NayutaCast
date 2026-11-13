use std::net::IpAddr;
use std::sync::Arc;

use capture::DisplayMode;
use egui::{Color32, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::sdk::discovery::{CastProtocol, DeviceInfo, DeviceKind, DeviceManager};
use crate::theme::*;

pub struct ScannerAction {
    pub start_casting: Option<DeviceInfo>,
}

pub struct ScannerState {
    pub active_tab: DeviceKind,
    pub selected_ip: String,
    pub show_ip_modal: bool,
    pub manual_ip: String,
    pub manual_name: String,
    pub manual_protocol: String,
    pub modal_error: Option<String>,
    cached_devices: Vec<DeviceInfo>,
    cached_scanning: bool,
    cached_wifi_up: bool,
    cached_lan_up: bool,
}

impl Default for ScannerState {
    fn default() -> Self {
        Self {
            active_tab: DeviceKind::Wifi,
            selected_ip: String::new(),
            show_ip_modal: false,
            manual_ip: String::new(),
            manual_name: String::new(),
            manual_protocol: "Auto".to_string(),
            modal_error: None,
            cached_devices: Vec::new(),
            cached_scanning: false,
            cached_wifi_up: true,
            cached_lan_up: false,
        }
    }
}

impl ScannerState {
    fn refresh_snapshot(&mut self, device_mgr: &DeviceManager) {
        if let Some(list) = device_mgr.list_nonblocking(self.active_tab) {
            self.cached_devices = list;
        }
        if let Some(sc) = device_mgr.is_scanning_nonblocking() {
            self.cached_scanning = sc;
        }
        if let Some(w) = device_mgr.has_active_interface_nonblocking(DeviceKind::Wifi) {
            self.cached_wifi_up = w;
        }
        if let Some(l) = device_mgr.has_active_interface_nonblocking(DeviceKind::Lan) {
            self.cached_lan_up = l;
        }
    }
}

fn draw_segmented_actions(
    ui: &mut Ui,
    act_height: f32,
    state: &mut ScannerState,
    device_mgr: &Arc<DeviceManager>,
    rt: &tokio::runtime::Handle,
) {
    let act_width = 240.0;
    let (act_rect, _) = ui.allocate_exact_size(vec2(act_width, act_height), Sense::hover());
    let painter = ui.painter_at(act_rect);

    painter.rect_filled(act_rect, 9.0, Color32::from_rgb(238, 241, 246));

    let half_w = act_width / 2.0;
    let rescan_rect = Rect::from_min_size(
        act_rect.min + vec2(2.0, 2.0),
        vec2(half_w - 3.0, act_height - 4.0),
    );
    let add_ip_rect = Rect::from_min_size(
        act_rect.min + vec2(half_w + 1.0, 2.0),
        vec2(half_w - 3.0, act_height - 4.0),
    );

    let rescan_hovered = ui.rect_contains_pointer(rescan_rect);
    let add_ip_hovered = ui.rect_contains_pointer(add_ip_rect);

    if rescan_hovered || add_ip_hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if !rescan_hovered && !add_ip_hovered {
        painter.line_segment(
            [
                pos2(act_rect.center().x, act_rect.min.y + 7.0),
                pos2(act_rect.center().x, act_rect.max.y - 7.0),
            ],
            Stroke::new(1.0, Color32::from_rgb(218, 224, 233)),
        );
    }

    if rescan_hovered {
        painter.rect_filled(rescan_rect, 7.5, Color32::WHITE);
    }
    if rescan_hovered && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
        device_mgr.trigger_scan(rt);
    }
    painter.text(
        rescan_rect.center(),
        egui::Align2::CENTER_CENTER,
        "Pindai Ulang",
        FontId::proportional(13.0),
        if rescan_hovered {
            PRIMARY_BLUE
        } else {
            TEXT_MAIN
        },
    );

    if add_ip_hovered {
        painter.rect_filled(add_ip_rect, 7.5, Color32::WHITE);
    }
    if add_ip_hovered && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
        state.modal_error = None;
        state.show_ip_modal = true;
    }
    painter.text(
        add_ip_rect.center(),
        egui::Align2::CENTER_CENTER,
        "Tambahkan IP",
        FontId::proportional(13.0),
        if add_ip_hovered {
            PRIMARY_BLUE
        } else {
            TEXT_MAIN
        },
    );
}

fn draw_display_mode_toggle(ui: &mut Ui, mode: &mut DisplayMode, col_width: f32) {
    let pill_w = 240.0f32.min(col_width);
    let pill_h = 34.0;
    let (rect, _) = ui.allocate_exact_size(vec2(pill_w, pill_h), Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 9.0, Color32::from_rgb(238, 241, 246));

    let half_w = pill_w / 2.0;
    let mirror_rect =
        Rect::from_min_size(rect.min + vec2(2.0, 2.0), vec2(half_w - 3.0, pill_h - 4.0));
    let extend_rect = Rect::from_min_size(
        rect.min + vec2(half_w + 1.0, 2.0),
        vec2(half_w - 3.0, pill_h - 4.0),
    );

    let is_mirror = *mode == DisplayMode::Mirror;
    painter.rect_filled(
        if is_mirror { mirror_rect } else { extend_rect },
        7.5,
        Color32::WHITE,
    );

    let mirror_hovered = ui.rect_contains_pointer(mirror_rect);
    let extend_hovered = ui.rect_contains_pointer(extend_rect);
    if mirror_hovered || extend_hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if mirror_hovered && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
        *mode = DisplayMode::Mirror;
    }
    if extend_hovered && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
        *mode = DisplayMode::Extend;
    }

    painter.text(
        mirror_rect.center(),
        egui::Align2::CENTER_CENTER,
        "Mirror Layar",
        FontId::proportional(12.5),
        if is_mirror { PRIMARY_BLUE } else { TEXT_MUTED },
    );
    painter.text(
        extend_rect.center(),
        egui::Align2::CENTER_CENTER,
        "Extend Layar",
        FontId::proportional(12.5),
        if !is_mirror { PRIMARY_BLUE } else { TEXT_MUTED },
    );

    ui.add_space(4.0);
    let hint = if is_mirror {
        "Duplikasi layar utama ke TV"
    } else {
        "Layar kedua terpisah (muncul di setting display OS)"
    };
    ui.label(egui::RichText::new(hint).size(11.5).color(TEXT_MUTED));
}

pub fn render_scanner(
    ui: &mut Ui,
    state: &mut ScannerState,
    device_mgr: &Arc<DeviceManager>,
    rt: &tokio::runtime::Handle,
    display_mode: &mut DisplayMode,
    preferred_protocol: &str,
) -> ScannerAction {
    state.refresh_snapshot(device_mgr);

    let mut action = ScannerAction {
        start_casting: None,
    };

    let preferred_proto = CastProtocol::from_setting_str(preferred_protocol);
    let available_width = ui.available_width();
    let col_width = 460.0f32.min(available_width - 40.0);

    ui.vertical_centered(|ui| {
        ui.add_space(14.0);

        // 1. Heading
        ui.label(
            egui::RichText::new("Cast your screen")
                .size(28.0)
                .strong()
                .color(TEXT_MAIN),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Choose a TV or receiver to start.")
                .size(14.0)
                .color(TEXT_MUTED),
        );

        ui.add_space(14.0);

        // 2. Segmented Tabs: [ Wi-Fi | LAN ]
        let tab_width = 180.0;
        let tab_height = 36.0;
        let (tab_rect, _) = ui.allocate_exact_size(vec2(tab_width, tab_height), Sense::hover());
        let painter = ui.painter_at(tab_rect);

        painter.rect_filled(tab_rect, 10.0, Color32::from_rgb(238, 241, 246));

        let half_w = tab_width / 2.0;
        let wifi_rect = Rect::from_min_size(
            tab_rect.min + vec2(2.0, 2.0),
            vec2(half_w - 4.0, tab_height - 4.0),
        );
        let lan_rect = Rect::from_min_size(
            tab_rect.min + vec2(half_w + 2.0, 2.0),
            vec2(half_w - 4.0, tab_height - 4.0),
        );

        let wifi_active = state.active_tab == DeviceKind::Wifi;
        painter.rect_filled(
            if wifi_active { wifi_rect } else { lan_rect },
            8.0,
            Color32::WHITE,
        );

        if ui.rect_contains_pointer(wifi_rect) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
                state.active_tab = DeviceKind::Wifi;
                state.refresh_snapshot(device_mgr);
            }
        }
        painter.text(
            wifi_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Wi-Fi",
            FontId::proportional(13.5),
            if wifi_active { TEXT_MAIN } else { TEXT_MUTED },
        );

        if ui.rect_contains_pointer(lan_rect) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
                state.active_tab = DeviceKind::Lan;
                state.refresh_snapshot(device_mgr);
            }
        }
        painter.text(
            lan_rect.center(),
            egui::Align2::CENTER_CENTER,
            "LAN",
            FontId::proportional(13.5),
            if !wifi_active { TEXT_MAIN } else { TEXT_MUTED },
        );

        ui.add_space(16.0);

        // 3. Device List
        let devices = state.cached_devices.clone();

        if !devices
            .iter()
            .any(|d| d.ip.to_string() == state.selected_ip)
        {
            if let Some(first) = devices.first() {
                state.selected_ip = first.ip.to_string();
            } else {
                state.selected_ip.clear();
            }
        }

        if devices.is_empty() {
            if state.cached_scanning {
                ui.ctx().request_repaint();
            }

            let radar_h = 170.0;
            let (radar_rect, _) = ui.allocate_exact_size(vec2(col_width, radar_h), Sense::hover());
            let painter = ui.painter_at(radar_rect);
            let center = pos2(radar_rect.center().x, radar_rect.min.y + 60.0);

            let time = ui.input(|i| i.time) as f32;
            for i in 0..3 {
                let phase = (time * 0.9 + i as f32 * 0.45) % 1.35;
                let radius = 16.0 + phase * 42.0;
                let alpha = ((1.35 - phase) / 1.35 * 110.0).clamp(0.0, 110.0) as u8;
                painter.circle_stroke(
                    center,
                    radius,
                    Stroke::new(1.8, Color32::from_rgba_premultiplied(79, 104, 255, alpha)),
                );
            }

            painter.circle_filled(center, 22.0, PRIMARY_BLUE_LIGHT);
            let icon_screen = Rect::from_center_size(center, vec2(18.0, 13.0));
            painter.rect_stroke(
                icon_screen,
                2.5,
                Stroke::new(1.8, PRIMARY_BLUE),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    pos2(center.x - 4.5, center.y + 10.0),
                    pos2(center.x + 4.5, center.y + 10.0),
                ],
                Stroke::new(1.8, PRIMARY_BLUE),
            );

            let (status_title, status_desc) =
                if state.active_tab == DeviceKind::Lan && !state.cached_lan_up {
                    (
                        "Tidak ada antarmuka LAN (Ethernet) aktif",
                        "Hubungkan kabel LAN atau beralih ke tab Wi-Fi untuk memindai Smart TV.",
                    )
                } else if state.cached_scanning {
                    (
                        "Memindai Smart TV di jaringan...",
                        "Mencari Google Cast, DLNA/UPnP, AirPlay, Roku & NayutaCast receiver...",
                    )
                } else {
                    (
                        "Belum ada TV atau receiver terdeteksi",
                        "Pastikan TV dan laptop terhubung ke jaringan Wi-Fi / LAN yang sama.",
                    )
                };

            painter.text(
                pos2(center.x, radar_rect.min.y + 125.0),
                egui::Align2::CENTER_CENTER,
                status_title,
                FontId::proportional(14.5),
                TEXT_MAIN,
            );
            painter.text(
                pos2(center.x, radar_rect.min.y + 148.0),
                egui::Align2::CENTER_CENTER,
                status_desc,
                FontId::proportional(12.0),
                TEXT_MUTED,
            );

            ui.add_space(18.0);
            draw_segmented_actions(ui, 36.0, state, device_mgr, rt);
        } else {
            egui::ScrollArea::vertical()
                .max_height(215.0)
                .show(ui, |ui| {
                    for device in &devices {
                        let ip_str = device.ip.to_string();
                        let is_selected = ip_str == state.selected_ip;
                        let card_height = 58.0;

                        let (card_rect, card_resp) =
                            ui.allocate_exact_size(vec2(col_width, card_height), Sense::click());

                        if card_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if card_resp.clicked() {
                            state.selected_ip = ip_str.clone();
                        }

                        let painter = ui.painter_at(card_rect);
                        let bg_color = if is_selected {
                            PRIMARY_BLUE_LIGHT
                        } else {
                            CARD_BG
                        };
                        let stroke = if is_selected {
                            card_selected_stroke()
                        } else {
                            card_stroke()
                        };
                        painter.rect(card_rect, 14.0, bg_color, stroke, egui::StrokeKind::Inside);

                        let icon_center = pos2(card_rect.min.x + 30.0, card_rect.center().y);
                        let icon_color = if is_selected {
                            PRIMARY_BLUE
                        } else {
                            TEXT_MUTED
                        };
                        let tv_screen = Rect::from_center_size(icon_center, vec2(18.0, 13.0));
                        painter.rect_stroke(
                            tv_screen,
                            2.5,
                            Stroke::new(1.8, icon_color),
                            egui::StrokeKind::Inside,
                        );
                        painter.line_segment(
                            [
                                pos2(icon_center.x - 5.0, icon_center.y + 10.0),
                                pos2(icon_center.x + 5.0, icon_center.y + 10.0),
                            ],
                            Stroke::new(1.8, icon_color),
                        );

                        painter.text(
                            pos2(card_rect.min.x + 56.0, card_rect.center().y - 9.0),
                            egui::Align2::LEFT_CENTER,
                            &device.name,
                            FontId::proportional(14.5),
                            if is_selected {
                                TEXT_MAIN
                            } else {
                                Color32::from_rgb(51, 65, 85)
                            },
                        );

                        painter.text(
                            pos2(card_rect.min.x + 56.0, card_rect.center().y + 10.0),
                            egui::Align2::LEFT_CENTER,
                            device.subtitle(preferred_proto),
                            FontId::proportional(12.0),
                            TEXT_MUTED,
                        );

                        let dot_x = card_rect.max.x - 48.0;
                        let has_native_proto = device
                            .protocols
                            .iter()
                            .any(|p| *p != CastProtocol::BrowserFallback);
                        let dot_color = if has_native_proto {
                            ONLINE_GREEN
                        } else {
                            Color32::from_rgb(245, 158, 11)
                        };
                        painter.circle_filled(pos2(dot_x, card_rect.center().y), 4.5, dot_color);

                        if is_selected {
                            let check_x = card_rect.max.x - 22.0;
                            let check_y = card_rect.center().y;
                            painter.line_segment(
                                [
                                    pos2(check_x - 7.0, check_y),
                                    pos2(check_x - 2.0, check_y + 4.5),
                                ],
                                Stroke::new(2.2, PRIMARY_BLUE),
                            );
                            painter.line_segment(
                                [
                                    pos2(check_x - 2.0, check_y + 4.5),
                                    pos2(check_x + 6.0, check_y - 5.0),
                                ],
                                Stroke::new(2.2, PRIMARY_BLUE),
                            );
                        }

                        ui.add_space(8.0);
                    }
                });

            ui.add_space(6.0);
            draw_segmented_actions(ui, 34.0, state, device_mgr, rt);

            ui.add_space(12.0);
            draw_display_mode_toggle(ui, display_mode, col_width);

            ui.add_space(14.0);

            let btn_height = 46.0;
            let (btn_rect, btn_resp) =
                ui.allocate_exact_size(vec2(col_width, btn_height), Sense::click());

            let btn_hovered = btn_resp.hovered();
            if btn_hovered {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let btn_color = if btn_hovered {
                PRIMARY_BLUE_HOVER
            } else {
                PRIMARY_BLUE
            };
            let painter = ui.painter_at(btn_rect);

            if btn_hovered {
                painter.rect_filled(
                    btn_rect.translate(vec2(0.0, 3.0)),
                    14.0,
                    Color32::from_rgba_premultiplied(79, 104, 255, 60),
                );
            }

            painter.rect_filled(btn_rect, 12.0, btn_color);
            painter.text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Start Casting",
                FontId::proportional(15.5),
                Color32::WHITE,
            );

            if btn_resp.clicked()
                && let Some(dev) = devices
                    .iter()
                    .find(|d| d.ip.to_string() == state.selected_ip)
                    .cloned()
            {
                action.start_casting = Some(dev);
            }
        }
    });

    // Modal dialog for manual IP input with protocol probing
    if state.show_ip_modal {
        egui::Window::new("Tambah Target Layar (TV / Monitor)")
            .collapsible(false)
            .resizable(false)
            .fixed_size(vec2(360.0, 235.0))
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Alamat IP Target TV:")
                        .size(13.0)
                        .color(TEXT_MAIN),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut state.manual_ip)
                        .hint_text("contoh: 192.168.1.50"),
                );

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Nama Perangkat (Opsional):")
                        .size(13.0)
                        .color(TEXT_MAIN),
                );
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut state.manual_name)
                        .hint_text("contoh: TV Ruang Keluarga"),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Protokol:").size(13.0).color(TEXT_MAIN));
                    egui::ComboBox::from_id_salt("modal_proto_select")
                        .selected_text(&state.manual_protocol)
                        .width(190.0)
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
                                    &mut state.manual_protocol,
                                    opt.to_string(),
                                    opt,
                                );
                            }
                        });
                });

                if let Some(err) = &state.modal_error {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(err).size(12.0).color(STOP_RED));
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Batal").clicked() {
                        state.modal_error = None;
                        state.show_ip_modal = false;
                    }
                    ui.add_space(8.0);
                    let add_btn = egui::Button::new(
                        egui::RichText::new("Pindai & Pilih")
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(PRIMARY_BLUE);
                    if ui.add(add_btn).clicked() {
                        let raw = state.manual_ip.trim();
                        match raw.parse::<IpAddr>() {
                            Ok(ip) => {
                                let name = if state.manual_name.trim().is_empty() {
                                    None
                                } else {
                                    Some(state.manual_name.trim().to_string())
                                };
                                let proto = CastProtocol::from_setting_str(&state.manual_protocol);
                                device_mgr.trigger_probe_custom(
                                    rt,
                                    ip,
                                    name,
                                    proto,
                                    state.active_tab,
                                );
                                state.selected_ip = ip.to_string();
                                state.manual_ip.clear();
                                state.manual_name.clear();
                                state.modal_error = None;
                                state.show_ip_modal = false;
                            }
                            Err(_) => {
                                state.modal_error =
                                    Some("Format IP tidak valid (contoh: 192.168.1.50)".into());
                            }
                        }
                    }
                });
            });
    }

    action
}
