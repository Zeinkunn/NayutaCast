use crate::theme::*;
use egui::{Color32, Rect, Sense, Stroke, Ui, pos2, vec2};

pub struct HeaderAction {
    pub open_settings: bool,
    pub close_app: bool,
    pub minimize: bool,
    pub maximize: bool,
}

pub fn render_header(ui: &mut Ui, show_gear: bool) -> HeaderAction {
    let mut action = HeaderAction {
        open_settings: false,
        close_app: false,
        minimize: false,
        maximize: false,
    };

    let header_height = 46.0;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), header_height),
        Sense::click_and_drag(),
    );

    // Allow dragging window from header
    if response.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let painter = ui.painter_at(rect);

    // 1. Traffic Lights on top left (x = 24.0)
    let left_x = rect.min.x + 28.0;
    let center_y = rect.center().y;
    let radius = 6.0;
    let spacing = 18.0;

    // Traffic Red
    let red_pos = pos2(left_x, center_y);
    let red_rect = Rect::from_center_size(red_pos, vec2(16.0, 16.0));
    let red_hovered = ui.rect_contains_pointer(red_rect);
    painter.circle_filled(
        red_pos,
        if red_hovered { radius * 1.12 } else { radius },
        TRAFFIC_RED,
    );
    if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) && red_hovered {
        action.close_app = true;
    }

    // Traffic Yellow
    let yellow_pos = pos2(left_x + spacing, center_y);
    let yellow_rect = Rect::from_center_size(yellow_pos, vec2(16.0, 16.0));
    let yellow_hovered = ui.rect_contains_pointer(yellow_rect);
    painter.circle_filled(
        yellow_pos,
        if yellow_hovered {
            radius * 1.12
        } else {
            radius
        },
        TRAFFIC_YELLOW,
    );
    if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) && yellow_hovered {
        action.minimize = true;
    }

    // Traffic Green
    let green_pos = pos2(left_x + spacing * 2.0, center_y);
    let green_rect = Rect::from_center_size(green_pos, vec2(16.0, 16.0));
    let green_hovered = ui.rect_contains_pointer(green_rect);
    painter.circle_filled(
        green_pos,
        if green_hovered { radius * 1.12 } else { radius },
        TRAFFIC_GREEN,
    );
    if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) && green_hovered {
        action.maximize = true;
    }

    // 2. Centered Title "NayutaCast"
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "NayutaCast",
        egui::FontId::proportional(13.0),
        Color32::from_rgb(140, 149, 166),
    );

    // 3. Custom Gear Button on top right
    if show_gear {
        let gear_size = 28.0;
        let gear_center = pos2(rect.max.x - 28.0 - gear_size / 2.0, center_y);
        let gear_rect = Rect::from_center_size(gear_center, vec2(gear_size, gear_size));
        let gear_hovered = ui.rect_contains_pointer(gear_rect);

        if gear_hovered {
            painter.rect_filled(
                gear_rect,
                6.0,
                Color32::from_rgba_premultiplied(100, 116, 139, 25),
            );
        }

        // Draw gear icon
        let gear_color = if gear_hovered { TEXT_MAIN } else { TEXT_MUTED };
        let r_inner = 3.5;
        let r_outer = 6.5;

        // Central circle
        painter.circle_stroke(gear_center, r_inner, Stroke::new(1.8, gear_color));

        // 6 cogs
        for i in 0..6 {
            let angle = (i as f32) * (std::f32::consts::PI / 3.0);
            let dir = vec2(angle.cos(), angle.sin());
            let p1 = gear_center + dir * (r_inner + 1.0);
            let p2 = gear_center + dir * (r_outer + 1.5);
            painter.line_segment([p1, p2], Stroke::new(2.0, gear_color));
        }

        if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) && gear_hovered {
            action.open_settings = true;
        }
    }

    action
}
