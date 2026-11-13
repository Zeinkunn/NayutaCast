use egui::{pos2, vec2, Color32, FontId, Rect, TextureHandle, Ui};
use crate::theme::*;

pub fn render_splash(
    ui: &mut Ui,
    icon_tex: Option<&TextureHandle>,
    elapsed: f32,
) -> bool {
    // Continuously request repaints for smooth animation
    ui.ctx().request_repaint();

    let full_rect = ui.available_rect_before_wrap();
    let center = full_rect.center();

    let painter = ui.painter();

    // 1. Draw centered avatar squircle icon with ambient glow
    let icon_size = 112.0;
    let icon_rect = Rect::from_center_size(
        pos2(center.x, center.y - 48.0),
        vec2(icon_size, icon_size),
    );

    // Ambient glow shadow
    let glow_radius = (elapsed * 2.0).sin() * 4.0 + 20.0;
    painter.rect_filled(
        icon_rect.expand(glow_radius),
        32.0,
        Color32::from_rgba_premultiplied(79, 107, 255, 20),
    );

    if let Some(tex) = icon_tex {
        painter.image(
            tex.id(),
            icon_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        painter.rect_filled(icon_rect, 24.0, PRIMARY_BLUE);
    }

    // 2. Animated bouncy staggered letters for "NayutaCast"
    let title = "NayutaCast";
    let font_id = FontId::proportional(32.0);
    let letter_spacing = 20.0;
    let total_width = (title.len() as f32) * letter_spacing;
    let start_x = center.x - total_width / 2.0 + letter_spacing / 2.0;
    let base_y = center.y + 44.0;

    let fade_alpha = (elapsed * 2.5).clamp(0.0, 1.0);

    for (i, ch) in title.chars().enumerate() {
        let phase = elapsed * 5.0 - (i as f32) * 0.42;
        let sin_val = phase.sin();
        let bounce_y = if sin_val > 0.0 { -sin_val * 11.0 } else { 0.0 };

        let char_pos = pos2(start_x + (i as f32) * letter_spacing, base_y + bounce_y);

        let color = Color32::from_rgba_premultiplied(
            (15.0 * fade_alpha) as u8,
            (23.0 * fade_alpha) as u8,
            (42.0 * fade_alpha) as u8,
            (255.0 * fade_alpha) as u8,
        );

        painter.text(
            char_pos,
            egui::Align2::CENTER_CENTER,
            ch.to_string(),
            font_id.clone(),
            color,
        );
    }

    // 3. Subtitle
    painter.text(
        pos2(center.x, base_y + 36.0),
        egui::Align2::CENTER_CENTER,
        "Initializing screen mirroring engine...",
        FontId::proportional(13.0),
        TEXT_MUTED,
    );

    // Return true when 2.0s elapsed (ready to transition)
    elapsed >= 2.0
}
