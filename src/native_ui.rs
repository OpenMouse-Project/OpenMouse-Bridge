use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Sense, Shape,
    Stroke, TextStyle, Vec2,
};

pub const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/DMSans-Variable.ttf");
pub const CHROME: Color32 = Color32::from_rgb(8, 9, 10);
pub const BACKGROUND: Color32 = Color32::from_rgb(12, 13, 15);
pub const SURFACE: Color32 = Color32::from_rgb(20, 22, 24);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(27, 30, 33);
pub const DIVIDER: Color32 = Color32::from_rgb(38, 42, 45);
pub const TEXT: Color32 = Color32::from_rgb(238, 240, 242);
pub const MUTED: Color32 = Color32::from_rgb(170, 176, 183);
pub const QUIET: Color32 = Color32::from_rgb(144, 151, 160);
pub const ACCENT: Color32 = Color32::from_rgb(93, 222, 137);
pub const ON_ACCENT: Color32 = Color32::from_rgb(7, 22, 14);
pub const WARNING: Color32 = Color32::from_rgb(242, 184, 75);
pub const DANGER: Color32 = Color32::from_rgb(255, 107, 107);
pub const CONTROL_RADIUS: u8 = 6;
pub const PANEL_RADIUS: u8 = 10;
pub const ROW_HEIGHT: f32 = 28.0;

#[derive(Clone, Copy)]
pub enum Icon {
    Gear,
    Back,
    Minimize,
    Close,
    External,
    Info,
    Mouse,
    Game,
    Logs,
}

pub fn icon(painter: &egui::Painter, rect: Rect, kind: Icon, color: Color32) {
    let r = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    let point = |x: f32, y: f32| Pos2::new(r.left() + x, r.top() + y);
    let stroke = Stroke::new(1.45, color);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([point(a.0, a.1), point(b.0, b.1)], stroke)
    };
    match kind {
        Icon::Gear => {
            painter.circle_stroke(point(8.0, 8.0), 2.3, stroke);
            painter.circle_stroke(point(8.0, 8.0), 5.3, stroke);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let d = Vec2::angled(a);
                painter.line_segment(
                    [point(8.0, 8.0) + d * 5.4, point(8.0, 8.0) + d * 7.0],
                    stroke,
                );
            }
        }
        Icon::Back => {
            line((10.0, 3.0), (5.0, 8.0));
            line((5.0, 8.0), (10.0, 13.0));
        }
        Icon::Minimize => {
            line((3.0, 9.0), (13.0, 9.0));
        }
        Icon::Close => {
            line((4.0, 4.0), (12.0, 12.0));
            line((12.0, 4.0), (4.0, 12.0));
        }
        Icon::External => {
            line((6.0, 3.0), (13.0, 3.0));
            line((13.0, 3.0), (13.0, 10.0));
            line((13.0, 3.0), (6.0, 10.0));
            line((3.0, 6.0), (3.0, 13.0));
            line((3.0, 13.0), (10.0, 13.0));
        }
        Icon::Info => {
            painter.circle_stroke(point(8.0, 8.0), 6.0, stroke);
            line((8.0, 7.0), (8.0, 11.0));
            painter.circle_filled(point(8.0, 4.5), 0.9, color);
        }
        Icon::Mouse => {
            painter.rect_stroke(
                Rect::from_min_max(point(3.5, 1.0), point(12.5, 15.0)),
                4,
                stroke,
                egui::StrokeKind::Inside,
            );
            line((8.0, 4.0), (8.0, 7.0));
        }
        Icon::Game => {
            painter.rect_stroke(
                Rect::from_min_max(point(1.0, 4.0), point(15.0, 12.0)),
                3,
                stroke,
                egui::StrokeKind::Inside,
            );
            line((4.0, 6.0), (4.0, 10.0));
            line((2.0, 8.0), (6.0, 8.0));
            painter.circle_filled(point(11.0, 7.0), 0.9, color);
            painter.circle_filled(point(13.0, 9.0), 0.9, color);
        }
        Icon::Logs => {
            for y in [4.0, 8.0, 12.0] {
                line((5.0, y), (13.0, y));
                painter.circle_filled(point(2.0, y), 0.8, color);
            }
        }
    }
}

fn gradient(rect: Rect, radius: f32, top: Color32, bottom: Color32) -> Shape {
    let mut mesh = egui::Mesh::default();
    let color = |p: Pos2| {
        let t = ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
        Color32::from(egui::lerp(
            egui::Rgba::from(top)..=egui::Rgba::from(bottom),
            t,
        ))
    };
    mesh.colored_vertex(rect.center(), color(rect.center()));
    let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    for (center, angle) in [
        (
            Pos2::new(rect.left() + radius, rect.top() + radius),
            std::f32::consts::PI,
        ),
        (
            Pos2::new(rect.right() - radius, rect.top() + radius),
            std::f32::consts::PI * 1.5,
        ),
        (
            Pos2::new(rect.right() - radius, rect.bottom() - radius),
            0.0,
        ),
        (
            Pos2::new(rect.left() + radius, rect.bottom() - radius),
            std::f32::consts::FRAC_PI_2,
        ),
    ] {
        for i in 0..=8 {
            let a = angle + i as f32 / 8.0 * std::f32::consts::FRAC_PI_2;
            let p = center + Vec2::angled(a) * radius;
            mesh.colored_vertex(p, color(p));
        }
    }
    let n = mesh.vertices.len() as u32 - 1;
    for i in 1..=n {
        mesh.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    Shape::mesh(mesh)
}

pub fn focus(ui: &egui::Ui, response: &egui::Response, radius: u8) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(2.0),
            radius,
            Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Outside,
        );
    }
}

pub fn panel<R>(
    ui: &mut egui::Ui,
    lit: bool,
    margin: i8,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let layer = ui.painter().add(Shape::Noop);
    let result = egui::Frame::NONE.inner_margin(margin).show(ui, content);
    let rect = result.response.rect;
    let top = if lit {
        Color32::from_rgb(28, 37, 32)
    } else {
        Color32::from_rgb(28, 30, 32)
    };
    let shapes = vec![
        gradient(rect, 10.0, top, SURFACE),
        Shape::rect_stroke(
            rect,
            10,
            Stroke::new(1.0, DIVIDER),
            egui::StrokeKind::Inside,
        ),
        Shape::line_segment(
            [
                Pos2::new(rect.left() + 10.0, rect.top() + 0.5),
                Pos2::new(rect.right() - 10.0, rect.top() + 0.5),
            ],
            Stroke::new(1.0, Color32::from_rgb(48, 53, 52)),
        ),
    ];
    ui.painter().set(layer, Shape::Vec(shapes));
    result
}

fn control(ui: &egui::Ui, response: &egui::Response, primary: bool) {
    let rect = response.rect;
    let (top, bottom) = if primary {
        (ACCENT, Color32::from_rgb(75, 204, 120))
    } else if response.hovered() {
        (Color32::from_rgb(46, 50, 53), Color32::from_rgb(32, 35, 38))
    } else {
        (Color32::from_rgb(39, 42, 45), Color32::from_rgb(28, 31, 34))
    };
    ui.painter().add(gradient(rect, 6.0, top, bottom));
    ui.painter().rect_stroke(
        rect,
        6,
        Stroke::new(
            1.0,
            if primary {
                Color32::from_rgb(105, 231, 151)
            } else {
                Color32::from_rgb(49, 53, 56)
            },
        ),
        egui::StrokeKind::Inside,
    );
    focus(ui, response, 6);
}

pub fn icon_button(ui: &mut egui::Ui, kind: Icon, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 6, SURFACE_HOVER);
    }
    icon(
        ui.painter(),
        rect,
        kind,
        if matches!(kind, Icon::Close) && response.hovered() {
            DANGER
        } else {
            MUTED
        },
    );
    focus(ui, &response, 6);
    response.on_hover_text(label)
}

pub fn button(
    ui: &mut egui::Ui,
    label: &str,
    kind: Option<Icon>,
    primary: bool,
    width: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, if primary { 34.0 } else { 26.0 }),
        Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    control(ui, &response, primary);
    let ink = if primary { ON_ACCENT } else { TEXT };
    let text = ui
        .painter()
        .layout_no_wrap(label.into(), FontId::proportional(12.5), ink);
    let total = text.size().x + if kind.is_some() { 23.0 } else { 0.0 };
    let left = rect.center().x - total / 2.0;
    ui.painter().galley(
        Pos2::new(left, rect.center().y - text.size().y / 2.0),
        text,
        ink,
    );
    if let Some(kind) = kind {
        icon(
            ui.painter(),
            Rect::from_center_size(
                Pos2::new(left + total - 8.0, rect.center().y),
                Vec2::splat(16.0),
            ),
            kind,
            ink,
        );
    }
    response
}

pub fn switch_row(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    let ink = if ui.is_enabled() { TEXT } else { QUIET };
    ui.painter().text(
        Pos2::new(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        ink,
    );
    let track = Rect::from_center_size(
        Pos2::new(rect.right() - 19.0, rect.center().y),
        Vec2::new(38.0, 22.0),
    );
    ui.painter()
        .rect_filled(track, 11, if *value { ACCENT } else { CHROME });
    if !*value {
        ui.painter().rect_stroke(
            track,
            11,
            Stroke::new(1.0, DIVIDER),
            egui::StrokeKind::Inside,
        );
    }
    let center = Pos2::new(
        track.left() + 11.0 + if *value { 16.0 } else { 0.0 },
        track.center().y,
    );
    ui.painter().circle_filled(
        center,
        9.0,
        if *value {
            ON_ACCENT
        } else {
            Color32::from_rgb(49, 53, 55)
        },
    );
    if *value {
        let p = ui.painter();
        p.line_segment(
            [center + Vec2::new(-3.0, 0.0), center + Vec2::new(-0.6, 2.5)],
            Stroke::new(1.3, ACCENT),
        );
        p.line_segment(
            [center + Vec2::new(-0.6, 2.5), center + Vec2::new(3.5, -2.5)],
            Stroke::new(1.3, ACCENT),
        );
    }
    focus(ui, &response, 6);
    response
}

pub fn install(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("DM Sans".into(), FontData::from_static(FONT_BYTES).into());
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "DM Sans".into());
    context.set_fonts(fonts);
    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    for (kind, size) in [
        (TextStyle::Heading, 18.0),
        (TextStyle::Body, 13.0),
        (TextStyle::Button, 12.5),
        (TextStyle::Small, 11.5),
    ] {
        style.text_styles.insert(kind, FontId::proportional(size));
    }
    style.spacing.item_spacing = Vec2::new(6.0, 4.0);
    style.spacing.button_padding = Vec2::new(9.0, 4.0);
    style.spacing.interact_size.y = 26.0;
    style.spacing.slider_width = 108.0;
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE_HOVER;
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.window_stroke = Stroke::new(1.0, DIVIDER);
    visuals.window_corner_radius = PANEL_RADIUS.into();
    visuals.menu_corner_radius = CONTROL_RADIUS.into();
    visuals.selection.bg_fill = Color32::from_rgb(32, 59, 42);
    visuals.selection.stroke = Stroke::new(1.5, TEXT);
    for (widget, fill) in [
        (&mut visuals.widgets.noninteractive, BACKGROUND),
        (&mut visuals.widgets.inactive, SURFACE_HOVER),
        (&mut visuals.widgets.hovered, Color32::from_rgb(43, 47, 50)),
        (&mut visuals.widgets.active, Color32::from_rgb(32, 59, 42)),
        (&mut visuals.widgets.open, SURFACE_HOVER),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, DIVIDER);
        widget.fg_stroke = Stroke::new(1.0, TEXT);
        widget.corner_radius = CONTROL_RADIUS.into();
        widget.expansion = 0.0;
    }
    visuals.widgets.active.bg_stroke = Stroke::new(1.5, ACCENT);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, QUIET);
    style.visuals = visuals;
    context.set_style_of(egui::Theme::Dark, style);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_responds_to_the_right_aligned_control() {
        let context = egui::Context::default();
        let mut value = false;
        let position = egui::pos2(300.0, 17.0);
        for events in [
            vec![],
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        ] {
            context
                .run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(320.0, 80.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        switch_row(ui, "Launch at login", &mut value);
                    },
                )
                .drop_without_applying_deltas();
        }
        assert!(value);
    }

    #[test]
    fn focused_switch_is_keyboard_operable() {
        let context = egui::Context::default();
        let mut value = false;
        for events in [
            vec![],
            vec![egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
        ] {
            context
                .run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(320.0, 80.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        switch_row(ui, "Sound", &mut value).request_focus();
                    },
                )
                .drop_without_applying_deltas();
        }
        assert!(value);
    }

    #[test]
    fn rounded_material_mesh_stays_inside_its_panel() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(336.0, 70.0));
        let Shape::Mesh(mesh) = gradient(rect, 10.0, SURFACE_HOVER, SURFACE) else {
            panic!("expected material mesh")
        };
        assert!(
            mesh.vertices
                .iter()
                .all(|vertex| rect.expand(0.01).contains(vertex.pos))
        );
        assert!(
            mesh.indices
                .iter()
                .all(|index| (*index as usize) < mesh.vertices.len())
        );
    }

    #[test]
    fn selected_options_keep_readable_text() {
        let context = egui::Context::default();
        install(&context);
        let style = context.style_of(egui::Theme::Dark);
        let luminance = |color: Color32| {
            let linear = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
        };
        let foreground = luminance(TEXT);
        let background = luminance(style.visuals.selection.bg_fill);
        assert!((foreground.max(background) + 0.05) / (foreground.min(background) + 0.05) >= 4.5);
    }

    #[test]
    fn emphasis_uses_ink_not_the_action_accent() {
        let context = egui::Context::default();
        install(&context);
        assert_eq!(
            context
                .style_of(egui::Theme::Dark)
                .visuals
                .strong_text_color(),
            TEXT
        );
    }
}
