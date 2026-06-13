use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[path = "dictation_transcript.rs"]
mod dictation_transcript;

use dictation_transcript::OrderedDictationTranscript;

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
        session: DictationSession,
    },
    Transcribing {
        session: DictationSession,
        spinner_stop: Arc<AtomicBool>,
        cancellation_token: CancellationToken,
    },
}

struct DictationSession {
    placeholder_id: String,
    transcript: OrderedDictationTranscript,
    meter_text: String,
}

struct DictationSessionUpdate {
    rendered_placeholder: String,
    empty_recording: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum DictationFinishAction {
    Replace {
        placeholder_id: String,
        text: String,
    },
    Remove {
        placeholder_id: String,
    },
}

impl DictationSession {
    fn new(placeholder_id: String, meter_text: String) -> Self {
        Self {
            placeholder_id,
            transcript: OrderedDictationTranscript::default(),
            meter_text,
        }
    }

    fn render_placeholder(&self) -> String {
        self.transcript.render_with_meter(&self.meter_text)
    }

    fn complete_chunk(&mut self, sequence: u64, text: String) -> DictationSessionUpdate {
        self.transcript.complete(sequence, text);
        DictationSessionUpdate {
            rendered_placeholder: self.render_placeholder(),
            empty_recording: false,
        }
    }

    fn fail_chunk(&mut self, sequence: u64) -> DictationSessionUpdate {
        self.transcript.fail(sequence);
        DictationSessionUpdate {
            rendered_placeholder: self.render_placeholder(),
            empty_recording: false,
        }
    }

    fn flush_chunks(&mut self, final_sequence: Option<u64>) -> DictationSessionUpdate {
        self.transcript.set_final_sequence(final_sequence);
        DictationSessionUpdate {
            rendered_placeholder: self.render_placeholder(),
            empty_recording: final_sequence.is_none() && self.transcript.is_empty(),
        }
    }

    fn finish_action(&self) -> Option<DictationFinishAction> {
        if !self.transcript.is_finished() {
            return None;
        }

        let placeholder_id = self.placeholder_id.clone();
        let text = self.transcript.text().to_string();
        if text.is_empty() {
            Some(DictationFinishAction::Remove { placeholder_id })
        } else {
            Some(DictationFinishAction::Replace {
                placeholder_id,
                text,
            })
        }
    }
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
            DictationState::Transcribing {
                spinner_stop,
                cancellation_token,
                ..
            } => {
                cancellation_token.cancel();
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

        let initial_meter_text = "⠤⠤⠤⠤".to_string();
        let placeholder_id = self
            .bottom_pane
            .insert_recording_meter_placeholder(&initial_meter_text);
        let capture = match crate::voice::VoiceCapture::start_chunked_recording(
            &self.config,
            placeholder_id.clone(),
            self.app_event_tx.clone(),
        ) {
            Ok(capture) => capture,
            Err(err) => {
                self.remove_recording_meter_placeholder(&placeholder_id);
                self.add_error_message(format!("Failed to start dictation: {err}"));
                return;
            }
        };

        let stop_flag = capture.stopped_flag();
        let peak = capture.last_peak_arc();
        self.dictation.state = DictationState::Recording {
            capture,
            session: DictationSession::new(placeholder_id.clone(), initial_meter_text),
        };
        self.set_footer_hint_override(Some(self.dictation_footer_hint_items()));
        start_dictation_meter_task(placeholder_id, self.app_event_tx.clone(), stop_flag, peak);
        self.request_redraw();
    }

    fn stop_dictation_and_transcribe(&mut self) {
        let DictationState::Recording {
            capture,
            mut session,
        } = std::mem::take(&mut self.dictation.state)
        else {
            return;
        };
        self.set_footer_hint_override(/*items*/ None);
        let Some(cancellation_token) = capture.chunked_recording_cancellation_token() else {
            capture.stop();
            self.remove_recording_meter_placeholder(&session.placeholder_id);
            self.add_error_message(
                "Failed to stop dictation: missing cancellation token".to_string(),
            );
            return;
        };

        match capture.stop_chunked_recording() {
            Ok(()) => {}
            Err(err) => {
                self.remove_recording_meter_placeholder(&session.placeholder_id);
                self.add_error_message(format!("Failed to stop dictation: {err}"));
                return;
            }
        }

        session.meter_text = "⠋".to_string();
        self.update_recording_meter_in_place(
            &session.placeholder_id,
            &session.render_placeholder(),
        );
        let spinner_stop = self.start_dictation_spinner(session.placeholder_id.clone());
        self.dictation.state = DictationState::Transcribing {
            session,
            spinner_stop,
            cancellation_token,
        };
        self.finish_dictation_if_ready();
    }

    pub(crate) fn on_dictation_chunk_transcription_complete(
        &mut self,
        id: &str,
        sequence: u64,
        text: String,
    ) {
        let Some(rendered) = self.update_dictation_session(id, |session| {
            session.complete_chunk(sequence, text).rendered_placeholder
        }) else {
            return;
        };
        if !self.update_recording_meter_in_place(id, &rendered) {
            self.stop_dictation_for_deleted_meter(id);
            return;
        }
        self.finish_dictation_if_ready();
    }

    pub(crate) fn on_dictation_chunk_transcription_failed(
        &mut self,
        id: &str,
        sequence: u64,
        error: String,
    ) {
        let Some(rendered) = self.update_dictation_session(id, |session| {
            session.fail_chunk(sequence).rendered_placeholder
        }) else {
            return;
        };
        if !self.update_recording_meter_in_place(id, &rendered) {
            self.stop_dictation_for_deleted_meter(id);
            return;
        }
        self.add_warning_message(format!(
            "Dictation segment {} failed: {error}",
            sequence.saturating_add(/*rhs*/ 1)
        ));
        self.finish_dictation_if_ready();
    }

    pub(crate) fn on_dictation_chunks_flushed(&mut self, id: &str, final_sequence: Option<u64>) {
        let Some((rendered, empty_recording)) = self.update_dictation_session(id, |session| {
            let update = session.flush_chunks(final_sequence);
            (update.rendered_placeholder, update.empty_recording)
        }) else {
            return;
        };
        if !self.update_recording_meter_in_place(id, &rendered) {
            self.stop_dictation_for_deleted_meter(id);
            return;
        }
        if empty_recording {
            self.add_warning_message(
                "Dictation failed: recording did not contain audio samples.".to_string(),
            );
        }
        self.finish_dictation_if_ready();
    }

    pub(crate) fn on_dictation_meter_update(&mut self, id: &str, text: &str) -> bool {
        let Some(rendered) = self.update_dictation_session(id, |session| {
            session.meter_text = text.to_string();
            session.render_placeholder()
        }) else {
            return false;
        };
        self.update_recording_meter_in_place(id, &rendered)
    }

    pub(crate) fn stop_dictation_for_deleted_meter(&mut self, id: &str) -> bool {
        match std::mem::take(&mut self.dictation.state) {
            DictationState::Recording { capture, session } => {
                if session.placeholder_id == id {
                    capture.stop();
                    self.set_footer_hint_override(/*items*/ None);
                    true
                } else {
                    self.dictation.state = DictationState::Recording { capture, session };
                    false
                }
            }
            DictationState::Transcribing {
                session,
                spinner_stop,
                cancellation_token,
            } => {
                if session.placeholder_id == id {
                    cancellation_token.cancel();
                    spinner_stop.store(true, Ordering::Relaxed);
                    true
                } else {
                    self.dictation.state = DictationState::Transcribing {
                        session,
                        spinner_stop,
                        cancellation_token,
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
                std::thread::sleep(Duration::from_millis(/*millis*/ 100));
            }
        });
        stop
    }

    fn update_dictation_session<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut DictationSession) -> T,
    ) -> Option<T> {
        match &mut self.dictation.state {
            DictationState::Recording { session, .. }
            | DictationState::Transcribing { session, .. }
                if session.placeholder_id == id =>
            {
                Some(f(session))
            }
            DictationState::Idle
            | DictationState::Recording { .. }
            | DictationState::Transcribing { .. } => None,
        }
    }

    fn finish_dictation_if_ready(&mut self) {
        let DictationState::Transcribing {
            session,
            spinner_stop,
            cancellation_token: _,
        } = &self.dictation.state
        else {
            return;
        };
        let Some(action) = session.finish_action() else {
            return;
        };

        spinner_stop.store(true, Ordering::Relaxed);
        self.dictation.state = DictationState::Idle;
        match action {
            DictationFinishAction::Replace {
                placeholder_id,
                text,
            } => {
                self.replace_recording_meter_placeholder(&placeholder_id, &text);
            }
            DictationFinishAction::Remove { placeholder_id } => {
                self.remove_recording_meter_placeholder(&placeholder_id);
            }
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

            std::thread::sleep(Duration::from_millis(/*millis*/ 60));
        }
    });
}
