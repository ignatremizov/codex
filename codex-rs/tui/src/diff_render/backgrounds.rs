use super::DiffColorLevel;
use super::DiffTheme;
use super::ResolvedDiffBackgrounds;
use super::RichDiffColorLevel;
use super::color_from_rgb_for_level;
use super::fallback_diff_backgrounds;
use crate::render::highlight::DiffScopeBackground;
use crate::render::highlight::DiffScopeBackgrounds;
use crate::render::highlight::diff_scope_backgrounds;
use codex_config::types::DiffBackgroundMode;
use codex_config::types::Tui;
#[cfg(not(test))]
use std::sync::OnceLock;
#[cfg(not(test))]
use std::sync::RwLock;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DiffBackgroundSettings {
    pub(super) mode: DiffBackgroundMode,
    pub(super) add_bg: Option<(u8, u8, u8)>,
    pub(super) del_bg: Option<(u8, u8, u8)>,
}

impl DiffBackgroundSettings {
    fn from_tui(tui: &Tui) -> Self {
        Self {
            mode: tui.diff_background,
            add_bg: parse_hex_rgb(tui.diff_add_bg.as_deref()),
            del_bg: parse_hex_rgb(tui.diff_del_bg.as_deref()),
        }
    }
}

#[cfg(not(test))]
static SETTINGS: OnceLock<RwLock<DiffBackgroundSettings>> = OnceLock::new();
#[cfg(test)]
thread_local! {
    static SETTINGS: std::cell::RefCell<DiffBackgroundSettings> =
        std::cell::RefCell::new(DiffBackgroundSettings::default());
}

pub(super) fn set(tui: &Tui) {
    let parsed = DiffBackgroundSettings::from_tui(tui);
    #[cfg(test)]
    SETTINGS.with(|settings| {
        let changed = *settings.borrow() != parsed;
        *settings.borrow_mut() = parsed;
        if changed {
            crate::render::highlight::invalidate_render_cache();
        }
    });
    #[cfg(not(test))]
    {
        let lock = SETTINGS.get_or_init(|| RwLock::new(DiffBackgroundSettings::default()));
        let mut settings = lock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = *settings != parsed;
        *settings = parsed;
        if changed {
            crate::render::highlight::invalidate_render_cache();
        }
    }
}

fn current() -> DiffBackgroundSettings {
    #[cfg(test)]
    return SETTINGS.with(|settings| settings.borrow().clone());
    #[cfg(not(test))]
    SETTINGS
        .get_or_init(|| RwLock::new(DiffBackgroundSettings::default()))
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(super) fn resolve(theme: DiffTheme, color_level: DiffColorLevel) -> ResolvedDiffBackgrounds {
    resolve_for(theme, color_level, diff_scope_backgrounds(), current())
}

/// Resolve content fills without changing gutter styling or the active syntax theme.
///
/// Auto and Theme share the adaptive baseline and theme-scope overrides. Custom
/// replaces valid sides independently, while Off and ANSI-16 omit content fills.
pub(super) fn resolve_for(
    theme: DiffTheme,
    color_level: DiffColorLevel,
    scope_backgrounds: DiffScopeBackgrounds,
    settings: DiffBackgroundSettings,
) -> ResolvedDiffBackgrounds {
    let mut resolved = match settings.mode {
        DiffBackgroundMode::Off => ResolvedDiffBackgrounds::default(),
        DiffBackgroundMode::Auto | DiffBackgroundMode::Theme | DiffBackgroundMode::Custom => {
            fallback_diff_backgrounds(theme, color_level)
        }
    };
    let Some(level) = RichDiffColorLevel::from_diff_color_level(color_level) else {
        return resolved;
    };
    if matches!(
        settings.mode,
        DiffBackgroundMode::Auto | DiffBackgroundMode::Theme
    ) {
        for (target, background) in [
            (&mut resolved.add, scope_backgrounds.inserted),
            (&mut resolved.del, scope_backgrounds.deleted),
        ] {
            match background {
                Some(DiffScopeBackground::Rgb(rgb)) => {
                    *target = Some(color_from_rgb_for_level(rgb, level))
                }
                Some(DiffScopeBackground::TerminalDefault) => *target = None,
                None => {}
            }
        }
    } else if settings.mode == DiffBackgroundMode::Custom {
        if let Some(rgb) = settings.add_bg {
            resolved.add = Some(color_from_rgb_for_level(rgb, level));
        }
        if let Some(rgb) = settings.del_bg {
            resolved.del = Some(color_from_rgb_for_level(rgb, level));
        }
    }
    resolved
}

fn parse_hex_rgb(input: Option<&str>) -> Option<(u8, u8, u8)> {
    let hex = input?.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
#[path = "backgrounds_tests.rs"]
mod tests;
