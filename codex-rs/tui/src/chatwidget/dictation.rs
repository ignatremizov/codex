//! Generation-owned plain-text dictation. It never routes through realtime conversation input.

use super::ChatWidget;
use crate::dictation::session::MicReservation;
use crate::dictation::session::Session;
use crate::dictation::session::Update;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(/*v*/ 1);

impl ChatWidget {
    pub(crate) fn dictation_enabled(&self) -> bool {
        crate::dictation::keymap_features(&self.local_settings).voice_transcription_enabled
    }

    pub(crate) fn cancel_dictation(&mut self) {
        if let Some(session) = self.dictation.take() {
            self.bottom_pane.finish_dictation(session.element);
            // Dropping Session signals cancellation; native ownership is released asynchronously.
        }
    }

    pub(super) fn toggle_dictation(&mut self) {
        if let Some(session) = &self.dictation {
            session.stop.cancel();
            self.bottom_pane
                .update_dictation(session.element, "", "[Dictation: transcribing]");
            return;
        }
        if !self.dictation_enabled()
            || self.external_writer_view
            || self.has_misalignment_policy_violation()
            || self.input_queue.startup_submission.is_some()
            || self.startup_submission_has_protected_input()
            || !self.bottom_pane.composer_input_enabled()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }
        if self.realtime_conversation_is_running() {
            self.add_error_message("Stop the voice conversation before dictating.".into());
            return;
        }
        let Some(lease) = MicReservation::shared().acquire() else {
            self.add_error_message(
                "Microphone cleanup is still in progress. Try again shortly.".into(),
            );
            return;
        };
        let generation = NEXT_GENERATION.fetch_add(/*val*/ 1, Ordering::Relaxed);
        let element = self.bottom_pane.begin_dictation();
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        self.dictation = Some(Session {
            generation,
            element,
            thread: self.thread_id(),
            stop: stop.clone(),
            cancel: cancel.clone(),
        });
        tokio::spawn(crate::dictation::session::run(
            Arc::new(self.config.clone()),
            generation,
            element,
            stop,
            cancel,
            lease,
            self.app_event_tx.clone(),
        ));
        self.request_redraw();
    }

    pub(crate) fn on_dictation_update(&mut self, generation: u64, element: u64, update: Update) {
        if !self
            .dictation
            .as_ref()
            .is_some_and(|session| session.generation == generation && session.element == element)
        {
            return;
        }
        if self
            .dictation
            .as_ref()
            .is_some_and(|session| session.thread != self.thread_id())
            || !self.dictation_enabled()
            || self.external_writer_view
            || !self.bottom_pane.composer_input_enabled()
        {
            self.cancel_dictation();
            return;
        }
        if !self.bottom_pane.has_dictation_element(element) {
            self.cancel_dictation();
            return;
        }
        match update {
            #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
            Update::Recording => {
                self.bottom_pane
                    .update_dictation(element, "", "[Dictation: recording]");
            }
            #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
            Update::Meter {
                peak,
                stopping,
                pending,
            } => {
                pending.store(/*val*/ false, Ordering::Release);
                let label = if stopping {
                    "[Dictation: transcribing]".to_string()
                } else {
                    let bars = usize::from(peak).saturating_mul(5) / 32768;
                    format!("[Dictation: recording {}]", "|".repeat(bars.min(5)))
                };
                self.bottom_pane.update_dictation(element, "", &label);
            }
            #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
            Update::Chunk {
                result,
                reservation,
            } => {
                match result {
                    Ok(text) => {
                        // Separators are presentation only; preserve every returned byte.
                        let text = if text.is_empty() {
                            text
                        } else {
                            format!("{text} ")
                        };
                        self.bottom_pane.update_dictation(
                            element,
                            &text,
                            "[Dictation: transcribing]",
                        );
                    }
                    Err(error) => self.add_error_message(error),
                }
                drop(reservation);
            }
            #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
            Update::Error(error) => self.add_error_message(error),
            Update::Finished => self.cancel_dictation(),
        }
        self.request_redraw();
    }
}

#[cfg(test)]
#[path = "dictation_tests.rs"]
mod tests;
