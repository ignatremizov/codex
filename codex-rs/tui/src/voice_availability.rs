use crate::legacy_core::config::Config;
use codex_features::Feature;

pub(crate) fn input_available_in_this_build() -> bool {
    cfg!(not(all(target_os = "linux", target_env = "musl")))
}

pub(crate) fn transcription_enabled(config: &Config) -> bool {
    config.features.enabled(Feature::VoiceTranscription) && input_available_in_this_build()
}
