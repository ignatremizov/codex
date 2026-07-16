use super::*;
use crate::diff_render::DiffLineType;
use crate::diff_render::DiffRenderStyleContext;
use crate::diff_render::render_wrapped_diff_line;
use assert_matches::assert_matches;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

#[test]
fn parses_custom_colors_without_slicing_invalid_utf8() {
    let inputs = [
        None,
        Some("#€abc"),
        Some("#12345"),
        Some("#1234567"),
        Some("#GG0000"),
        Some("123456"),
        Some(" #aBcDeF "),
    ];
    assert_eq!(
        inputs.map(parse_hex_rgb),
        [None, None, None, None, None, None, Some((171, 205, 239))]
    );
}

#[test]
fn custom_colors_quantize_and_ansi16_omits_all_content_fills() {
    let settings = DiffBackgroundSettings::from_tui(&Tui {
        diff_background: DiffBackgroundMode::Custom,
        diff_add_bg: Some("#005f00".to_string()),
        diff_del_bg: Some("#870000".to_string()),
        ..Tui::default()
    });
    assert_matches!(
        resolve_for(
            DiffTheme::Dark,
            DiffColorLevel::Ansi256,
            DiffScopeBackgrounds::default(),
            settings.clone(),
        ),
        ResolvedDiffBackgrounds {
            add: Some(Color::Indexed(22)),
            del: Some(Color::Indexed(88)),
        }
    );
    for mode in [
        DiffBackgroundMode::Auto,
        DiffBackgroundMode::Theme,
        DiffBackgroundMode::Custom,
        DiffBackgroundMode::Off,
    ] {
        assert_eq!(
            resolve_for(
                DiffTheme::Light,
                DiffColorLevel::Ansi16,
                DiffScopeBackgrounds {
                    inserted: Some(DiffScopeBackground::Rgb((1, 2, 3))),
                    deleted: Some(DiffScopeBackground::Rgb((4, 5, 6))),
                },
                DiffBackgroundSettings {
                    mode,
                    ..settings.clone()
                },
            ),
            ResolvedDiffBackgrounds::default()
        );
    }
}

#[test]
fn automatic_modes_preserve_explicit_terminal_default_scope() {
    for mode in [DiffBackgroundMode::Auto, DiffBackgroundMode::Theme] {
        assert_matches!(
            resolve_for(
                DiffTheme::Dark,
                DiffColorLevel::TrueColor,
                DiffScopeBackgrounds {
                    inserted: Some(DiffScopeBackground::TerminalDefault),
                    deleted: Some(DiffScopeBackground::Rgb((4, 5, 6))),
                },
                DiffBackgroundSettings {
                    mode,
                    ..DiffBackgroundSettings::default()
                },
            ),
            ResolvedDiffBackgrounds {
                add: None,
                del: Some(Color::Rgb(4, 5, 6)),
            }
        );
    }
}

#[test]
fn configured_backgrounds_reach_rendered_diff_cells() {
    let mut rows = Vec::new();
    for (name, mode, delete_color) in [
        ("custom", DiffBackgroundMode::Custom, "#5a2d1e"),
        ("off", DiffBackgroundMode::Off, "#5a2d1e"),
        ("partial", DiffBackgroundMode::Custom, "#€abc"),
    ] {
        let settings = DiffBackgroundSettings::from_tui(&Tui {
            diff_background: mode,
            diff_add_bg: Some("#123456".to_string()),
            diff_del_bg: Some(delete_color.to_string()),
            ..Tui::default()
        });
        let style = DiffRenderStyleContext {
            theme: DiffTheme::Dark,
            color_level: DiffColorLevel::TrueColor,
            diff_backgrounds: resolve_for(
                DiffTheme::Dark,
                DiffColorLevel::TrueColor,
                DiffScopeBackgrounds::default(),
                settings,
            ),
        };
        let lines = [DiffLineType::Insert, DiffLineType::Delete]
            .into_iter()
            .flat_map(|kind| {
                render_wrapped_diff_line(
                    /*line_number*/ 1, kind, "x", /*width*/ 4,
                    /*line_number_width*/ 1, /*syntax_spans*/ None, style,
                )
            })
            .map(|line| line.line)
            .collect::<Vec<_>>();
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 4, /*height*/ 2,
        );
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buffer);
        // Snapshot every painted glyph and background. Foreground contrast is
        // independently covered by the existing syntax-color buffer snapshot.
        for y in 0..area.height {
            let text: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
            let backgrounds: Vec<_> = (0..area.width).map(|x| buffer[(x, y)].bg).collect();
            rows.push(format!("{name}: {text:?}; backgrounds={backgrounds:?}"));
        }
    }
    insta::assert_snapshot!(rows.join("\n"), @r#"
    custom: "1 +x"; backgrounds=[Rgb(18, 52, 86), Rgb(18, 52, 86), Rgb(18, 52, 86), Rgb(18, 52, 86)]
    custom: "1 -x"; backgrounds=[Rgb(90, 45, 30), Rgb(90, 45, 30), Rgb(90, 45, 30), Rgb(90, 45, 30)]
    off: "1 +x"; backgrounds=[Reset, Reset, Reset, Reset]
    off: "1 -x"; backgrounds=[Reset, Reset, Reset, Reset]
    partial: "1 +x"; backgrounds=[Rgb(18, 52, 86), Rgb(18, 52, 86), Rgb(18, 52, 86), Rgb(18, 52, 86)]
    partial: "1 -x"; backgrounds=[Rgb(74, 34, 29), Rgb(74, 34, 29), Rgb(74, 34, 29), Rgb(74, 34, 29)]
    "#);
}
