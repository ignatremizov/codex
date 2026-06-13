//! Recording lifecycle on the shared runtime. Sequence reservations survive HTTP completion
//! until ordered text is handed to the composer, so a stalled first request bounds later work.

#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use super::capture::CaptureEvent;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use super::capture::CaptureHandle;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use super::ordered::OrderedTranscript;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use super::transcription::PreparedTranscription;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use super::transcription::TranscriptionFailure;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::legacy_core::config::Config;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use futures::StreamExt;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use futures::stream::FuturesUnordered;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
use std::sync::atomic::AtomicBool;
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
use std::sync::atomic::Ordering;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct MicReservation(Arc<Semaphore>);

impl Default for MicReservation {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(/*permits*/ 1)))
    }
}

impl MicReservation {
    pub(crate) fn shared() -> &'static Self {
        static RESERVATION: OnceLock<MicReservation> = OnceLock::new();
        RESERVATION.get_or_init(Self::default)
    }

    pub(crate) fn acquire(&self) -> Option<MicLease> {
        self.0.clone().try_acquire_owned().ok().map(Arc::new)
    }

    pub(crate) async fn acquire_after_release(&self) -> Result<MicLease, String> {
        tokio::time::timeout(
            std::time::Duration::from_secs(/*secs*/ 40),
            self.0.clone().acquire_owned(),
        )
        .await
        .map_err(|_| "Previous microphone owner did not finish shutting down.".to_string())?
        .map(Arc::new)
        .map_err(|_| "Microphone reservation closed.".to_string())
    }
}

/// Last-owner release is the acknowledgment; cancellation alone never frees the microphone.
pub(crate) type MicLease = Arc<OwnedSemaphorePermit>;

#[derive(Debug)]
pub(crate) enum Update {
    #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
    Recording,
    #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
    Meter {
        peak: u16,
        stopping: bool,
        pending: Arc<AtomicBool>,
    },
    #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
    Chunk {
        result: Result<String, String>,
        reservation: OwnedSemaphorePermit,
    },
    #[cfg(any(test, not(all(target_os = "linux", target_env = "musl"))))]
    Error(String),
    Finished,
}

pub(crate) struct Session {
    pub(crate) generation: u64,
    pub(crate) element: u64,
    pub(crate) thread: Option<codex_protocol::ThreadId>,
    pub(crate) stop: CancellationToken,
    pub(crate) cancel: CancellationToken,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(not(all(target_os = "linux", target_env = "musl")))]
pub(crate) async fn run(
    config: Arc<Config>,
    generation: u64,
    element: u64,
    stop: CancellationToken,
    cancel: CancellationToken,
    lease: MicLease,
    tx: AppEventSender,
) {
    let send = |update| {
        tx.send(AppEvent::DictationUpdate {
            generation,
            element,
            update,
        })
    };
    let prepared = tokio::select! {
        biased;
        _ = cancel.cancelled() => return,
        _ = stop.cancelled() => {
            send(Update::Finished);
            return;
        }
        result = PreparedTranscription::prepare(&config) => match result {
            Ok(prepared) => Arc::new(prepared),
            Err(error) => {
                send(Update::Error(error));
                send(Update::Finished);
                return;
            }
        }
    };
    if cancel.is_cancelled() {
        return;
    }
    let slots = Arc::new(Semaphore::new(/*permits*/ 5));
    let (capture, mut events) =
        match CaptureHandle::start(stop.clone(), cancel.clone(), slots, lease.clone()) {
            Ok(capture) => capture,
            Err(error) => {
                send(Update::Error(error));
                send(Update::Finished);
                return;
            }
        };
    send(Update::Recording);
    let mut uploads = FuturesUnordered::new();
    let mut reservations: BTreeMap<u64, OwnedSemaphorePermit> = BTreeMap::new();
    let mut ordered = OrderedTranscript::default();
    let mut next_sequence = 0_u64;
    let mut capturing = true;
    let mut meter_tick =
        tokio::time::interval(std::time::Duration::from_millis(/*millis*/ 100));
    let meter_pending = Arc::new(AtomicBool::new(/*v*/ false));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = meter_tick.tick(), if capturing => {
                if !meter_pending.swap(/*val*/ true, Ordering::AcqRel) {
                    send(Update::Meter { peak: capture.meter(), stopping: stop.is_cancelled(), pending: meter_pending.clone() });
                }
            }
            result = uploads.next(), if !uploads.is_empty() => {
                if let Some((sequence, result)) = result {
                    for resolved in ordered.resolve(sequence, result) {
                        if let Some(reservation) = reservations.remove(&resolved.sequence) {
                            send(Update::Chunk { result: resolved.result, reservation });
                        }
                    }
                }
            }
            event = events.recv(), if capturing && uploads.len() < 2 => {
                match event {
                    Some(CaptureEvent::Chunk { audio, reservation }) => {
                        let sequence = next_sequence;
                        next_sequence += 1;
                        reservations.insert(sequence, reservation);
                        let prepared = prepared.clone();
                        let cancel = cancel.clone();
                        uploads.push(async move {
                            let result = match prepared.transcribe(audio, cancel).await {
                                Ok(text) => Ok(text),
                                Err(TranscriptionFailure::Canceled) => Err("Dictation canceled.".into()),
                                Err(TranscriptionFailure::TimedOut) => Err("Dictation transcription timed out.".into()),
                                Err(TranscriptionFailure::Failed(error)) => Err(error),
                            };
                            (sequence, result)
                        });
                    }
                    Some(CaptureEvent::Error(error)) => send(Update::Error(error)),
                    Some(CaptureEvent::Finished) | None => capturing = false,
                }
            }
        }
        if !capturing && uploads.is_empty() {
            break;
        }
    }
    // Drop requests immediately on cancellation, but keep the lease until native shutdown.
    drop(uploads);
    drop(events);
    capture.terminated().await;
    drop(lease);
    send(Update::Finished);
}

#[cfg(all(target_os = "linux", target_env = "musl"))]
pub(crate) async fn run(
    _config: Arc<Config>,
    generation: u64,
    element: u64,
    _stop: CancellationToken,
    _cancel: CancellationToken,
    _lease: MicLease,
    tx: AppEventSender,
) {
    tx.send(AppEvent::DictationUpdate {
        generation,
        element,
        update: Update::Finished,
    });
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
