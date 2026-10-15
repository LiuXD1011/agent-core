use agent_desktop::shell::{self, BACKGROUND, CONTENT_WIDTH, SIDEBAR_WIDTH, SURFACE, TEXT};
use eframe::egui;

#[test]
fn neutral_dark_style_is_independent_of_the_windows_theme() {
    let ctx = egui::Context::default();
    shell::configure(&ctx);
    let _ = ctx.run(
        egui::RawInput {
            system_theme: Some(egui::Theme::Light),
            ..Default::default()
        },
        |_| {},
    );
    let visuals = ctx.style().visuals.clone();
    assert!(visuals.dark_mode);
    assert_eq!(visuals.panel_fill, BACKGROUND);
    assert_eq!(visuals.widgets.inactive.bg_fill, SURFACE);
    assert_eq!(visuals.override_text_color, Some(TEXT));
    assert_eq!(BACKGROUND.r(), BACKGROUND.g());
    assert_eq!(BACKGROUND.g(), BACKGROUND.b());
    assert!(u16::from(TEXT.r()) > u16::from(BACKGROUND.r()) + 160);
}

#[test]
fn transcript_and_composer_columns_are_centered_and_bounded() {
    for size in [
        egui::vec2(900.0, 640.0),
        egui::vec2(1280.0, 840.0),
        egui::vec2(1920.0, 1080.0),
    ] {
        let ctx = egui::Context::default();
        shell::configure(&ctx);
        let mut column = egui::Rect::NOTHING;
        let mut available = egui::Rect::NOTHING;
        for _ in 0..2 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ctx| {
                    egui::SidePanel::left("sidebar")
                        .exact_width(SIDEBAR_WIDTH)
                        .show(ctx, |_| {});
                    egui::CentralPanel::default().show(ctx, |ui| {
                        available = ui.available_rect_before_wrap();
                        shell::column(ui, |ui| {
                            column = ui.available_rect_before_wrap();
                            let _ = shell::welcome(ui);
                        });
                    });
                },
            );
        }
        assert!(
            column.width() <= CONTENT_WIDTH + 1.0,
            "{size:?}: {column:?}"
        );
        assert!(column.width() <= available.width() + 1.0);
        assert!((column.center().x - available.center().x).abs() <= 1.0);
    }
}

#[test]
fn suggestion_content_is_offline_and_does_not_submit_without_a_click() {
    let ctx = egui::Context::default();
    shell::configure(&ctx);
    let mut draft = None;
    let output = ctx.run(egui::RawInput::default(), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            draft = shell::welcome(ui);
        });
    });
    assert!(draft.is_none());
    assert!(!output.shapes.is_empty());
    assert_eq!(shell::SUGGESTIONS.len(), 3);
    assert!(shell::SUGGESTIONS
        .iter()
        .all(|item| !item.prompt.is_empty()));
    assert_eq!(shell::content_width(4000.0), CONTENT_WIDTH);
    assert_eq!(shell::content_width(320.0), 320.0);
    assert_eq!(shell::content_width(-1.0), 0.0);
}

#[test]
fn manual_theme_selection_uses_complete_palettes_and_preserves_layout() {
    let ctx = egui::Context::default();
    shell::configure(&ctx);
    for theme in [egui::Theme::Light, egui::Theme::Dark, egui::Theme::Light] {
        shell::set_theme(&ctx, theme);
        let expected = shell::Palette::for_theme(theme);
        let before = ctx.style().spacing.clone();
        let _ = ctx.run(
            egui::RawInput {
                // The explicit user choice must not be reset by the OS theme.
                system_theme: Some(if theme == egui::Theme::Light {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                }),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = shell::welcome(ui);
                });
            },
        );
        assert_eq!(ctx.theme(), theme);
        assert_eq!(shell::palette(&ctx), expected);
        let visuals = ctx.style().visuals.clone();
        assert_eq!(visuals.panel_fill, expected.background);
        assert_eq!(visuals.window_fill, expected.sidebar);
        assert_eq!(visuals.widgets.inactive.bg_fill, expected.surface);
        assert_eq!(visuals.override_text_color, Some(expected.text));
        assert_eq!(visuals.dark_mode, theme == egui::Theme::Dark);
        assert_eq!(ctx.style().spacing.item_spacing, before.item_spacing);
        assert!(expected.text.r().abs_diff(expected.background.r()) > 160);
    }
}

#[test]
fn clicking_day_and_night_controls_switches_the_active_theme() {
    let ctx = egui::Context::default();
    shell::configure(&ctx);
    for (label, theme) in [("白天", egui::Theme::Light), ("黑夜", egui::Theme::Dark)] {
        let mut position = None;
        for _ in 0..2 {
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, shell::theme_controls);
            });
            for clipped in output.shapes {
                if let egui::Shape::Text(text) = clipped.shape {
                    if text.galley.text() == label {
                        position = Some(text.pos + text.galley.size() * 0.5);
                    }
                }
            }
        }
        let pos = position.expect("theme control must be visible");
        for pressed in [true, false] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, shell::theme_controls);
                },
            );
        }
        assert_eq!(ctx.theme(), theme);
        assert_eq!(shell::palette(&ctx), shell::Palette::for_theme(theme));
    }
}
