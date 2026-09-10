//! Stable nickname hues shared by agent identity presentations.

use crate::terminal_palette::StdoutColorLevel;
use ratatui::style::Color;

/// Select a nickname's color from its unescaped bytes, independent of agent metadata.
pub(crate) fn nickname_color(nickname: &str) -> Color {
    // Fixed-width arithmetic keeps colors stable across runs and platforms.
    let hash = nickname.bytes().fold(0_u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    let level = crate::terminal_palette::effective_stdout_color_level();
    if matches!(level, StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown) {
        let palette = [Color::Cyan, Color::Green, Color::Magenta];
        return palette[(hash % palette.len() as u32) as usize];
    }
    // Paired bright/dark variants keep the nickname's hue recognizable across themes.
    // Identity colors are decorative, not status indicators.
    let palette = [
        ((95, 215, 255), (0, 95, 135)),
        ((95, 215, 135), (0, 95, 0)),
        ((215, 135, 255), (135, 0, 175)),
        ((255, 175, 95), (175, 95, 0)),
        ((255, 135, 175), (175, 0, 95)),
        ((175, 175, 255), (95, 95, 175)),
        ((95, 215, 215), (0, 135, 135)),
        ((215, 215, 95), (95, 95, 0)),
        ((255, 135, 95), (175, 0, 0)),
        ((135, 175, 255), (0, 95, 175)),
        ((175, 215, 95), (95, 135, 0)),
        ((255, 135, 215), (175, 0, 135)),
        ((95, 175, 175), (0, 95, 95)),
        ((215, 175, 135), (135, 95, 0)),
        ((175, 135, 215), (95, 0, 135)),
        ((135, 215, 175), (0, 135, 95)),
        ((215, 135, 135), (135, 0, 0)),
        ((175, 215, 255), (95, 135, 175)),
        ((215, 175, 215), (135, 95, 135)),
        ((255, 215, 135), (175, 135, 0)),
    ];
    let (dark_background, light_background) = palette[(hash % palette.len() as u32) as usize];
    let target = if crate::terminal_palette::default_bg().is_some_and(crate::color::is_light) {
        light_background
    } else {
        dark_background
    };
    crate::terminal_palette::best_color_for_level(target, level)
}

#[cfg(test)]
#[path = "agent_color_tests.rs"]
mod tests;
