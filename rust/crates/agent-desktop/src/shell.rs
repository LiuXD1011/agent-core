//! Native desktop presentation. No application state or provider work lives here.
use eframe::egui::{self, Color32, FontId, RichText, Stroke};

pub const BACKGROUND: Color32 = Color32::from_rgb(31, 31, 31);
pub const SIDEBAR: Color32 = Color32::from_rgb(20, 20, 20);
pub const SURFACE: Color32 = Color32::from_rgb(41, 41, 41);
pub const BORDER: Color32 = Color32::from_rgb(58, 58, 58);
pub const TEXT: Color32 = Color32::from_rgb(236, 236, 236);
pub const MUTED: Color32 = Color32::from_rgb(160, 160, 160);
pub const SIDEBAR_WIDTH: f32 = 248.0;
pub const CONTENT_WIDTH: f32 = 800.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    input: Color32,
    hover: Color32,
    active: Color32,
    selection: Color32,
}

impl Palette {
    pub const DARK: Self = Self {
        background: BACKGROUND,
        sidebar: SIDEBAR,
        surface: SURFACE,
        border: BORDER,
        text: TEXT,
        muted: MUTED,
        accent: Color32::from_rgb(76, 240, 206),
        input: Color32::from_rgb(28, 28, 28),
        hover: Color32::from_rgb(58, 58, 58),
        active: Color32::from_rgb(70, 70, 70),
        selection: Color32::from_rgb(76, 76, 76),
    };
    pub const LIGHT: Self = Self {
        background: Color32::WHITE,
        sidebar: Color32::from_rgb(245, 245, 245),
        surface: Color32::from_rgb(241, 241, 241),
        border: Color32::from_rgb(215, 215, 215),
        text: Color32::from_rgb(32, 32, 32),
        muted: Color32::from_rgb(98, 98, 98),
        accent: Color32::from_rgb(0, 128, 103),
        input: Color32::from_rgb(250, 250, 250),
        hover: Color32::from_rgb(230, 230, 230),
        active: Color32::from_rgb(220, 220, 220),
        selection: Color32::from_rgb(200, 220, 250),
    };

    pub const fn for_theme(theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self::DARK,
            egui::Theme::Light => Self::LIGHT,
        }
    }
}

pub fn palette(ctx: &egui::Context) -> Palette {
    Palette::for_theme(ctx.theme())
}

pub fn configure(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let mut style = theme.default_style();
        style.visuals = themed_visuals(theme);
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(11.0));
        ctx.set_style_of(theme, style);
    }
    set_theme(ctx, egui::Theme::Dark);
}

pub fn set_theme(ctx: &egui::Context, theme: egui::Theme) {
    ctx.set_theme(theme);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(match theme {
        egui::Theme::Dark => egui::SystemTheme::Dark,
        egui::Theme::Light => egui::SystemTheme::Light,
    }));
    ctx.request_repaint();
}

fn themed_visuals(theme: egui::Theme) -> egui::Visuals {
    let palette = Palette::for_theme(theme);
    let mut visuals = theme.default_visuals();
    visuals.override_text_color = Some(palette.text);
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.sidebar;
    visuals.extreme_bg_color = palette.input;
    visuals.faint_bg_color = palette.surface;
    visuals.selection.bg_fill = palette.selection;
    visuals.selection.stroke = Stroke::new(1.0, palette.text);
    visuals.window_corner_radius = egui::CornerRadius::same(14);
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(8);
        widget.fg_stroke = Stroke::new(1.0, palette.text);
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.inactive.bg_fill = palette.surface;
    visuals.widgets.inactive.weak_bg_fill = palette.surface;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    visuals.widgets.hovered.bg_fill = palette.hover;
    visuals.widgets.hovered.weak_bg_fill = palette.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.active.bg_fill = palette.active;
    visuals.widgets.active.weak_bg_fill = palette.active;
    visuals.widgets.open.bg_fill = palette.active;
    visuals.widgets.open.weak_bg_fill = palette.active;
    visuals
}

/// Manual selection affects presentation only, even while a worker is running.
pub fn theme_controls(ui: &mut egui::Ui) {
    let current = ui.ctx().theme();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("外观")
                .size(11.0)
                .color(palette(ui.ctx()).muted),
        );
        for (theme, label) in [(egui::Theme::Light, "白天"), (egui::Theme::Dark, "黑夜")] {
            if ui.selectable_label(current == theme, label).clicked() {
                set_theme(ui.ctx(), theme);
            }
        }
    });
}

/// Shared by the transcript and composer, including at the minimum window size.
pub fn content_width(available: f32) -> f32 {
    available.clamp(0.0, CONTENT_WIDTH)
}

pub fn column<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let available = ui.available_width();
    let width = content_width(available);
    ui.horizontal(|ui| {
        ui.add_space(((available - width) * 0.5).max(0.0));
        ui.allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                content(ui)
            },
        )
        .inner
    })
    .inner
}

pub fn mark(ui: &mut egui::Ui, size: f32) {
    let palette = palette(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, size * 0.25, palette.accent);
    let stroke = Stroke::new((size * 0.07).max(1.5), palette.sidebar);
    let p = |x, y| rect.min + egui::vec2(x, y) * size;
    painter.line_segment([p(0.25, 0.36), p(0.4, 0.5)], stroke);
    painter.line_segment([p(0.4, 0.5), p(0.25, 0.64)], stroke);
    painter.line_segment([p(0.52, 0.64), p(0.75, 0.64)], stroke);
}

pub struct Suggestion {
    pub title: &'static str,
    pub description: &'static str,
    pub prompt: &'static str,
}

pub const SUGGESTIONS: [Suggestion; 3] = [
    Suggestion {
        title: "整理资料",
        description: "从工作区提炼清晰结论",
        prompt: "先查看工作区中的文档，列出资料目录并总结重点；暂不修改文件。",
    },
    Suggestion {
        title: "分析数据",
        description: "检查数据并提出分析步骤",
        prompt: "查找工作区内的数据文件，说明格式和质量问题，提出可验证的分析计划。",
    },
    Suggestion {
        title: "开发与自动化",
        description: "把想法变成可验证的成果",
        prompt: "先理解项目，再帮我实现一个最小功能。",
    },
];

/// Returns a draft only; choosing a suggestion never starts a model request.
pub fn welcome(ui: &mut egui::Ui) -> Option<&'static str> {
    let palette = palette(ui.ctx());
    let mut selected = None;
    ui.vertical_centered(|ui| {
        mark(ui, 42.0);
        ui.add_space(16.0);
        ui.label(RichText::new("把任务交给 Agent Core").size(28.0).strong());
        ui.label(
            RichText::new("连接模型，选择工作区，从想法到成果。")
                .size(13.0)
                .color(palette.muted),
        );
    });
    ui.add_space(28.0);
    let width = ((ui.available_width() - 16.0) / 3.0).max(0.0);
    ui.horizontal(|ui| {
        for suggestion in &SUGGESTIONS {
            let response = ui.add_sized(
                [width, 78.0],
                egui::Button::new(
                    RichText::new(format!("{}\n{}", suggestion.title, suggestion.description))
                        .size(13.0),
                )
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, palette.border))
                .corner_radius(12),
            );
            if response.on_hover_text(suggestion.prompt).clicked() {
                selected = Some(suggestion.prompt);
            }
        }
    });
    selected
}
