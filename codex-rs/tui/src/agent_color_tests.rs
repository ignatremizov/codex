use super::*;
use crate::terminal_palette::with_test_default_colors;
use crate::terminal_probe::DefaultColors;

#[test]
fn nickname_colors_follow_background_without_changing_identity() {
    let names = ["Hume", "Darwin", "Mill", "Leibniz"];
    let render = |bg| {
        with_test_default_colors(
            DefaultColors {
                fg: (128, 128, 128),
                bg,
            },
            || {
                names
                    .iter()
                    .map(|name| {
                        let color = nickname_color(name);
                        format!("{name}: {color:?}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        )
    };
    insta::assert_snapshot!(render((0, 0, 0)), @r"
    Hume: Rgb(135, 175, 255)
    Darwin: Rgb(255, 175, 95)
    Mill: Rgb(255, 135, 95)
    Leibniz: Rgb(95, 215, 135)
    ");
    insta::assert_snapshot!(render((255, 255, 255)), @r"
    Hume: Rgb(0, 95, 175)
    Darwin: Rgb(175, 95, 0)
    Mill: Rgb(175, 0, 0)
    Leibniz: Rgb(0, 95, 0)
    ");
}
