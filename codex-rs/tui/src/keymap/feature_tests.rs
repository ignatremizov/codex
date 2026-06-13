//! Runtime feature gates must not reserve each other's inactive shortcuts.

use super::RuntimeKeymap;
use super::RuntimeKeymapFeatures;
use crate::key_hint;
use codex_config::types::TuiKeymap;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

#[test]
fn workspace_and_dictation_reserve_keys_only_when_independently_enabled() {
    let keymap: TuiKeymap = toml::from_str(
        "[global]\nfocus_main_composer = 'f12'\n[composer]\ntoggle_dictation = 'f12'",
    )
    .expect("feature-specific keymap");
    for workspace_enabled in [false, true] {
        for voice_transcription_enabled in [false, true] {
            let result = RuntimeKeymap::from_config_with_features(
                &keymap,
                RuntimeKeymapFeatures {
                    workspace_enabled,
                    voice_transcription_enabled,
                },
            );
            if workspace_enabled && voice_transcription_enabled {
                let error = result.expect_err("simultaneous actions must not share a shortcut");
                assert!(error.contains("focus_main_composer"));
                assert!(error.contains("toggle_dictation"));
                continue;
            }
            let runtime = result.expect("inactive feature bindings must not reserve keys");
            let expected = vec![key_hint::plain(KeyCode::F(12))];
            assert_eq!(
                (
                    runtime.app.focus_main_composer,
                    runtime.composer.toggle_dictation
                ),
                (
                    if workspace_enabled {
                        expected.clone()
                    } else {
                        Vec::new()
                    },
                    if voice_transcription_enabled {
                        expected
                    } else {
                        Vec::new()
                    },
                ),
            );
        }
    }
}
