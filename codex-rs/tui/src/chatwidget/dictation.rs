use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Default)]
pub(super) struct DictationUiState {
    state: DictationState,
}

#[derive(Default)]
enum DictationState {
    #[default]
    Idle,
    Recording {
        capture: crate::voice::VoiceCapture,
        placeholder_id: String,
    },
    Transcribing {
        placeholder_id: String,
        spinner_stop: Arc<AtomicBool>,
    },
}

impl DictationUiState {
    pub(super) fn is_recording(&self) -> bool {
        matches!(self.state, DictationState::Recording { .. })
    }

    pub(super) fn is_active(&self) -> bool {
        !matches!(self.state, DictationState::Idle)
    }
}

impl Drop for DictationUiState {
    fn drop(&mut self) {
        match std::mem::take(&mut self.state) {
            DictationState::Idle => {}
            DictationState::Recording { capture, .. } => capture.stop(),
            DictationState::Transcribing { spinner_stop, .. } => {
                spinner_stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl ChatWidget {
    fn dictation_footer_hint_items(&self) -> Vec<(String, String)> {
        crate::keymap::primary_binding(&self.dictation_keymap)
            .map(|binding| vec![(binding.display_label(), "stop dictation".to_string())])
            .unwrap_or_default()
    }

    pub(super) fn dictation_enabled(&self) -> bool {
        crate::voice_availability::transcription_enabled(&self.config)
    }

    pub(super) fn handle_dictation_shortcut(&mut self, key_event: KeyEvent) -> bool {
        if key_event.kind != KeyEventKind::Press {
            return false;
        }
        if !self.dictation_keymap.is_pressed(key_event) {
            return false;
        }
        if !self.dictation_enabled() {
            return false;
        }
        if !self.bottom_pane.no_modal_or_popup_active() {
            return false;
        }

        if self.dictation.is_recording() {
            self.stop_dictation_and_transcribe();
        } else {
            self.start_dictation();
        }
        true
    }

    pub(super) fn handle_dictation_submit_key(&mut self, key_event: KeyEvent) -> bool {
        if !self.bottom_pane.composer_submit_or_queue_pressed(key_event) {
            return false;
        }

        let warning = match &self.dictation.state {
            DictationState::Idle => return false,
            DictationState::Recording { .. } => "Stop dictation before submitting.",
            DictationState::Transcribing { .. } => {
                "Wait for dictation transcription to finish before submitting."
            }
        };
        self.add_warning_message(warning.to_string());
        true
    }

    fn start_dictation(&mut self) {
        if !self.has_chatgpt_account {
            self.add_warning_message(
                "Dictation requires ChatGPT login auth. Run `codex login`.".to_string(),
            );
            return;
        }
        if let Err(error) = crate::voice::validate_transcription_auth(&self.config) {
            self.add_warning_message(error);
            return;
        }
        if self.realtime_conversation.is_live() {
            self.add_warning_message(
                "Stop realtime voice mode before starting dictation.".to_string(),
            );
            return;
        }
        if matches!(self.dictation.state, DictationState::Transcribing { .. }) {
            self.add_warning_message(
                "Wait for the current dictation transcription to finish.".to_string(),
            );
            return;
        }

        let capture = match crate::voice::VoiceCapture::start_recording(&self.config) {
            Ok(capture) => capture,
            Err(err) => {
                self.add_error_message(format!("Failed to start dictation: {err}"));
                return;
            }
        };

        let placeholder_id = self.bottom_pane.insert_recording_meter_placeholder("⠤⠤⠤⠤");
        let stop_flag = capture.stopped_flag();
        let peak = capture.last_peak_arc();
        self.dictation.state = DictationState::Recording {
            capture,
            placeholder_id: placeholder_id.clone(),
        };
        self.set_footer_hint_override(Some(self.dictation_footer_hint_items()));
        start_dictation_meter_task(placeholder_id, self.app_event_tx.clone(), stop_flag, peak);
        self.request_redraw();
    }

    fn stop_dictation_and_transcribe(&mut self) {
        let DictationState::Recording {
            capture,
            placeholder_id,
        } = std::mem::take(&mut self.dictation.state)
        else {
            return;
        };
        self.set_footer_hint_override(/*items*/ None);

        let audio = match capture.stop_recording() {
            Ok(audio) => audio,
            Err(err) => {
                self.remove_recording_meter_placeholder(&placeholder_id);
                self.add_error_message(format!("Failed to stop dictation: {err}"));
                return;
            }
        };

        self.update_recording_meter_in_place(&placeholder_id, "⠋");
        let spinner_stop = self.start_dictation_spinner(placeholder_id.clone());
        self.dictation.state = DictationState::Transcribing {
            placeholder_id: placeholder_id.clone(),
            spinner_stop,
        };
        crate::voice::transcribe_async(
            placeholder_id,
            audio,
            self.config.clone(),
            self.app_event_tx.clone(),
        );
    }

    pub(crate) fn on_transcription_complete(&mut self, id: &str, text: &str) {
        if !matches!(
            &self.dictation.state,
            DictationState::Transcribing { placeholder_id, .. } if placeholder_id == id
        ) {
            return;
        }
        self.stop_dictation_spinner_for_current_state();
        self.dictation.state = DictationState::Idle;
        self.replace_recording_meter_placeholder(id, text);
    }

    pub(crate) fn on_transcription_failed(&mut self, id: &str, error: String) {
        if !matches!(
            &self.dictation.state,
            DictationState::Transcribing { placeholder_id, .. } if placeholder_id == id
        ) {
            return;
        }
        self.stop_dictation_spinner_for_current_state();
        self.dictation.state = DictationState::Idle;
        self.remove_recording_meter_placeholder(id);
        self.add_warning_message(format!("Dictation failed: {error}"));
    }

    pub(crate) fn stop_dictation_for_deleted_meter(&mut self, id: &str) -> bool {
        match std::mem::take(&mut self.dictation.state) {
            DictationState::Recording {
                capture,
                placeholder_id,
            } => {
                if placeholder_id == id {
                    capture.stop();
                    self.set_footer_hint_override(/*items*/ None);
                    true
                } else {
                    self.dictation.state = DictationState::Recording {
                        capture,
                        placeholder_id,
                    };
                    false
                }
            }
            DictationState::Transcribing {
                placeholder_id,
                spinner_stop,
            } => {
                if placeholder_id == id {
                    spinner_stop.store(true, Ordering::Relaxed);
                    true
                } else {
                    self.dictation.state = DictationState::Transcribing {
                        placeholder_id,
                        spinner_stop,
                    };
                    false
                }
            }
            DictationState::Idle => false,
        }
    }

    fn start_dictation_spinner(&mut self, id: String) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let tx = self.app_event_tx.clone();
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0usize;
            while !thread_stop.load(Ordering::Relaxed) {
                tx.send(AppEvent::UpdateRecordingMeter {
                    id: id.clone(),
                    text: FRAMES[frame % FRAMES.len()].to_string(),
                });
                frame = frame.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        stop
    }

    fn stop_dictation_spinner_for_current_state(&mut self) {
        if let DictationState::Transcribing { spinner_stop, .. } = &self.dictation.state {
            spinner_stop.store(true, Ordering::Relaxed);
        }
    }
}

fn start_dictation_meter_task(
    meter_placeholder_id: String,
    app_event_tx: AppEventSender,
    stop_flag: Arc<AtomicBool>,
    peak: Arc<AtomicU16>,
) {
    std::thread::spawn(move || {
        let mut meter = crate::voice::RecordingMeterState::new();

        loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }

            let meter_text = meter.next_text(peak.load(Ordering::Relaxed));
            app_event_tx.send(AppEvent::UpdateRecordingMeter {
                id: meter_placeholder_id.clone(),
                text: meter_text,
            });

            std::thread::sleep(Duration::from_millis(60));
        }
    });
}
