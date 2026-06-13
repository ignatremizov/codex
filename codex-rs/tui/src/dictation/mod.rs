//! Opt-in editable dictation, independent of the helper-backed realtime conversation.

#[cfg(not(all(target_os = "linux", target_env = "musl")))]
mod capture;
#[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
mod chunk_policy;
#[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
mod ordered;
pub(crate) mod session;
#[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
pub(crate) mod transcription;

#[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
pub(crate) struct RecordedAudio {
    pub(crate) data: Vec<i16>,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
}

pub(crate) const fn is_supported() -> bool {
    !cfg!(all(target_os = "linux", target_env = "musl"))
}

pub(crate) fn keymap_features(
    settings: &crate::local_settings::LocalSettings,
) -> crate::keymap::RuntimeKeymapFeatures {
    crate::keymap::RuntimeKeymapFeatures {
        voice_transcription_enabled: settings.voice_transcription_enabled && is_supported(),
    }
}
