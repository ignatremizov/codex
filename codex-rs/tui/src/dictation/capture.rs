//! Direct capture owns no networking. Cancellation is acknowledged only after both threads exit.

use super::RecordedAudio;
use super::chunk_policy::ChunkBoundary;
use super::chunk_policy::ChunkPolicy;
use super::session::MicLease;
use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use cpal::traits::StreamTrait;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::sync::mpsc as ingress;
use std::thread;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc_vad::SampleRate;
use webrtc_vad::Vad;
use webrtc_vad::VadMode;

#[path = "capture_device.rs"]
mod device;
use device::build_input;

const BLOCK_SAMPLES: usize = 1024;
const INGRESS_BLOCKS: usize = 32;
const SERVICE_INTERVAL: Duration = Duration::from_millis(/*millis*/ 10);

pub(crate) enum CaptureEvent {
    Chunk {
        audio: RecordedAudio,
        reservation: OwnedSemaphorePermit,
    },
    /// Partial recording: accepted audio is still drained before `Finished`.
    Error(String),
    Finished,
}

pub(crate) struct CaptureHandle {
    cancel: CancellationToken,
    terminated: CancellationToken,
    peak: Arc<AtomicU16>,
}

impl CaptureHandle {
    pub(crate) fn start(
        stop: CancellationToken,
        cancel: CancellationToken,
        slots: Arc<Semaphore>,
        lease: MicLease,
    ) -> Result<(Self, mpsc::Receiver<CaptureEvent>), String> {
        let reservation = slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "No room for another dictation recording.".to_string())?;
        let (tx, rx) = mpsc::channel(/*buffer*/ 1);
        let peak = Arc::new(AtomicU16::new(/*v*/ 0));
        let control = Arc::new(Control {
            stop,
            cancel: cancel.clone(),
            failed: AtomicBool::new(/*v*/ false),
            pressured: AtomicBool::new(/*v*/ false),
            peak: peak.clone(),
        });
        let owner_control = control.clone();
        let owner_tx = tx.clone();
        let owner_lease = lease.clone();
        let owner = thread::Builder::new()
            .name("dictation-capture".to_string())
            .spawn(move || {
                let _lease = owner_lease;
                if let Err(error) = capture(owner_control.clone(), &owner_tx, slots, reservation) {
                    publish(&owner_tx, CaptureEvent::Error(error), &owner_control);
                }
                publish(&owner_tx, CaptureEvent::Finished, &owner_control);
            })
            .map_err(|error| format!("Could not start microphone capture: {error}"))?;
        let terminated = CancellationToken::new();
        // Never join a live thread on the UI/runtime. The lease also survives a dropped handle,
        // failed startup, cancellation during device discovery, and a panicking owner.
        tokio::spawn(monitor_termination(
            owner,
            lease,
            terminated.clone(),
            control,
            tx,
        ));
        Ok((
            Self {
                cancel,
                terminated,
                peak,
            },
            rx,
        ))
    }

    pub(crate) async fn terminated(&self) {
        self.terminated.cancelled().await;
    }

    pub(crate) fn meter(&self) -> u16 {
        self.peak.swap(/*val*/ 0, Ordering::Relaxed)
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Control {
    stop: CancellationToken,
    cancel: CancellationToken,
    failed: AtomicBool,
    pressured: AtomicBool,
    peak: Arc<AtomicU16>,
}

async fn monitor_termination(
    owner: thread::JoinHandle<()>,
    lease: MicLease,
    terminated: CancellationToken,
    control: Arc<Control>,
    tx: mpsc::Sender<CaptureEvent>,
) {
    while !owner.is_finished() {
        tokio::time::sleep(SERVICE_INTERVAL).await;
    }
    let failed = owner.join().is_err();
    drop(lease);
    terminated.cancel();
    if failed {
        tokio::select! {
            biased;
            _ = control.cancel.cancelled() => {}
            _ = tx.send(CaptureEvent::Error("Microphone capture terminated unexpectedly.".to_string())) => {}
        }
    }
}

impl Control {
    fn accepting(&self) -> bool {
        !self.stop.is_cancelled() && !self.cancel.is_cancelled()
    }

    fn fail(&self) {
        self.failed.store(/*val*/ true, Ordering::Release);
        self.stop.cancel();
    }
}

struct Block {
    samples: [i16; BLOCK_SAMPLES],
    len: usize,
}

fn publish(tx: &mpsc::Sender<CaptureEvent>, mut event: CaptureEvent, control: &Control) -> bool {
    loop {
        if control.cancel.is_cancelled() {
            return false;
        }
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(pending)) => {
                if matches!(&pending, CaptureEvent::Chunk { .. }) && control.accepting() {
                    control.pressured.store(/*val*/ true, Ordering::Release);
                    control.stop.cancel();
                }
                event = pending;
            }
        }
        thread::sleep(SERVICE_INTERVAL);
    }
}

fn capture(
    control: Arc<Control>,
    tx: &mpsc::Sender<CaptureEvent>,
    slots: Arc<Semaphore>,
    reservation: OwnedSemaphorePermit,
) -> Result<(), String> {
    if !control.accepting() {
        return Ok(());
    }
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| "No microphone input device is available.".to_string())?;
    let config = device
        .default_input_config()
        .map_err(|error| error.to_string())?;
    if !control.accepting() {
        return Ok(());
    }
    let sample_rate = config.sample_rate();
    if sample_rate == 0 || config.channels() == 0 {
        return Err("The microphone returned an invalid audio format.".to_string());
    }
    let (input, output) = ingress::sync_channel(INGRESS_BLOCKS);
    let stream_config = config.config();
    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => build_input::<i8>(&device, stream_config, input, control.clone()),
        cpal::SampleFormat::I16 => {
            build_input::<i16>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::I32 => {
            build_input::<i32>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::I64 => {
            build_input::<i64>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::U8 => build_input::<u8>(&device, stream_config, input, control.clone()),
        cpal::SampleFormat::U16 => {
            build_input::<u16>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::U32 => {
            build_input::<u32>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::U64 => {
            build_input::<u64>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::F32 => {
            build_input::<f32>(&device, stream_config, input, control.clone())
        }
        cpal::SampleFormat::F64 => {
            build_input::<f64>(&device, stream_config, input, control.clone())
        }
        // CPAL's sample format enum is non-exhaustive.
        format => return Err(format!("Unsupported microphone sample format: {format:?}")),
    }
    .map_err(|error| format!("Could not open microphone: {error}"))?;
    let worker_control = control.clone();
    let worker_tx = tx.clone();
    let worker = thread::Builder::new()
        .name("dictation-chunks".to_string())
        .spawn(move || {
            chunk_worker(
                output,
                sample_rate,
                worker_control,
                worker_tx,
                slots,
                reservation,
            )
        })
        .map_err(|error| format!("Could not start audio processing: {error}"))?;
    let mut running = RunningCapture {
        stream: Some(stream),
        worker: Some(worker),
        control: control.clone(),
    };
    let started = if control.accepting() {
        running
            .stream
            .as_ref()
            .map(StreamTrait::play)
            .transpose()
            .map(|_| ())
            .map_err(|error| format!("Could not start microphone: {error}"))
    } else {
        Ok(())
    };
    if started.is_ok() {
        while control.accepting()
            && running
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            thread::sleep(SERVICE_INTERVAL);
        }
    }
    // Closing the producer makes the worker drain exactly the blocks already accepted.
    drop(running.stream.take());
    if let Some(worker) = running.worker.take() {
        worker
            .join()
            .map_err(|_| "Audio processing terminated unexpectedly.".to_string())?;
    }
    started
}

/// Unwinding must not detach processing and prematurely release the microphone reservation.
struct RunningCapture {
    stream: Option<cpal::Stream>,
    worker: Option<thread::JoinHandle<()>>,
    control: Arc<Control>,
}

impl Drop for RunningCapture {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.control.cancel.cancel();
        }
        drop(self.stream.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn enqueue(input: &ingress::SyncSender<Block>, block: Block, control: &Control) -> bool {
    if !control.accepting() {
        return false;
    }
    if input.try_send(block).is_err() {
        control.fail();
        return false;
    }
    true
}

fn chunk_worker(
    output: ingress::Receiver<Block>,
    sample_rate: u32,
    control: Arc<Control>,
    tx: mpsc::Sender<CaptureEvent>,
    slots: Arc<Semaphore>,
    mut reservation: OwnedSemaphorePermit,
) {
    let mut chunker = Chunker::new(sample_rate);
    let mut capacity_reported = false;
    while !control.cancel.is_cancelled() {
        let block = match output.recv_timeout(SERVICE_INTERVAL) {
            Ok(block) => block,
            Err(ingress::RecvTimeoutError::Timeout) => continue,
            Err(ingress::RecvTimeoutError::Disconnected) => break,
        };
        for sample in &block.samples[..block.len] {
            if control.cancel.is_cancelled() {
                return;
            }
            if chunker.push(*sample) {
                if !publish(
                    &tx,
                    CaptureEvent::Chunk {
                        audio: chunker.take(),
                        reservation,
                    },
                    &control,
                ) {
                    return;
                }
                reservation = loop {
                    if control.cancel.is_cancelled() || tx.is_closed() {
                        return;
                    }
                    if let Ok(permit) = slots.clone().try_acquire_owned() {
                        break permit;
                    }
                    let was_accepting = control.accepting();
                    control.stop.cancel();
                    if was_accepting && !capacity_reported {
                        capacity_reported = true;
                        if !publish(&tx, CaptureEvent::Error(
                            "Dictation stopped because transcription could not keep up. Accepted audio is still being transcribed.".to_string()
                        ), &control) {
                            return;
                        }
                    }
                    thread::sleep(SERVICE_INTERVAL);
                };
            }
        }
    }
    if control.cancel.is_cancelled() {
        return;
    }
    // Full-frame metadata is reset, but native PCM (including sub-frame tails) owns final flush.
    chunker.policy.finish();
    if !chunker.audio.is_empty()
        && !publish(
            &tx,
            CaptureEvent::Chunk {
                audio: chunker.take(),
                reservation,
            },
            &control,
        )
    {
        return;
    }
    if control.failed.load(Ordering::Acquire) {
        publish(&tx, CaptureEvent::Error(
            "Microphone audio was interrupted or exceeded the recording buffer. Only the accepted portion was transcribed.".to_string()
        ), &control);
    }
    if control.pressured.load(Ordering::Acquire) && !capacity_reported {
        publish(&tx, CaptureEvent::Error(
            "Dictation stopped because transcription could not keep up. Accepted audio is still being transcribed.".to_string()
        ), &control);
    }
}

struct Chunker {
    sample_rate: u32,
    audio: Vec<i16>,
    vad: Vad,
    policy: ChunkPolicy,
    frame: Vec<i16>,
    phase: u64,
    previous: i16,
}

impl Chunker {
    fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            audio: Vec::new(),
            vad: Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VadMode::Aggressive),
            policy: ChunkPolicy::default(),
            frame: Vec::with_capacity(/*capacity*/ 320),
            phase: 0,
            previous: 0,
        }
    }

    fn push(&mut self, sample: i16) -> bool {
        self.audio.push(sample);
        self.phase += 16_000;
        let mut split = false;
        // Rational streaming resampling: preserve phase across callbacks and chunks, and keep
        // every native-rate sample for upload, including any incomplete VAD frame.
        while self.phase >= u64::from(self.sample_rate) {
            self.phase -= u64::from(self.sample_rate);
            let fraction = 1.0 - self.phase as f64 / 16_000.0;
            let interpolated = f64::from(self.previous)
                + (f64::from(sample) - f64::from(self.previous)) * fraction;
            self.frame.push(interpolated as i16);
            if self.frame.len() == 320 {
                // Invalid VAD frames are conservatively speech, never permission to discard PCM.
                let speech = self
                    .vad
                    .is_voice_segment(&self.frame)
                    .unwrap_or(/*default*/ true);
                split |= self.policy.observe_frame(speech) == ChunkBoundary::Split;
                self.frame.clear();
            }
        }
        self.previous = sample;
        split
    }

    fn take(&mut self) -> RecordedAudio {
        RecordedAudio {
            data: std::mem::take(&mut self.audio),
            sample_rate: self.sample_rate,
            channels: 1,
        }
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
