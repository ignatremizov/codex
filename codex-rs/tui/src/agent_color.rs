//! Stable terminal-theme colors shared by agent identity presentations.

use ratatui::style::Color;

/// Select a nickname's color from its unescaped bytes, independent of agent metadata.
pub(crate) fn nickname_color(nickname: &str) -> Color {
    // Fixed-width arithmetic keeps colors stable across runs and platforms.
    let hash = nickname.bytes().fold(0_u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    let palette = [Color::Cyan, Color::Green, Color::Magenta];
    palette[(hash % palette.len() as u32) as usize]
}
