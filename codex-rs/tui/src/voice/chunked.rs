use super::RecordedAudio;
use super::convert_pcm16;
use super::transcription::MinimumDurationPolicy;
use super::transcription::transcribe_chunk_async;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::legacy_core::config::Config;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use webrtc_vad::SampleRate;
use webrtc_vad::Vad;
use webrtc_vad::VadMode;

const VAD_SAMPLE_RATE: u32 = 16_000;
const VAD_CHANNELS: u16 = 1;
const VAD_FRAME_MS: u64 = 20;

const DEFAULT_MIN_CHUNK_MS: u64 = 15_000;
const DEFAULT_SILENCE_SPLIT_MS: u64 = 1_000;
const DEFAULT_MAX_CHUNK_MS: u64 = 60_000;
const SIGNAL_PEAK_THRESHOLD: i16 = 256;
const SIGNAL_RMS_THRESHOLD: f64 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkPolicy {
    min_chunk_ms: u64,
    silence_split_ms: u64,
    max_chunk_ms: u64,
    vad_frame_ms: u64,
}

impl Default for ChunkPolicy {
    fn default() -> Self {
        Self {
            min_chunk_ms: DEFAULT_MIN_CHUNK_MS,
            silence_split_ms: DEFAULT_SILENCE_SPLIT_MS,
            max_chunk_ms: DEFAULT_MAX_CHUNK_MS,
            vad_frame_ms: VAD_FRAME_MS,
        }
    }
}

impl ChunkPolicy {
    #[cfg(test)]
    pub(crate) fn for_test(min_chunk_ms: u64, silence_split_ms: u64, max_chunk_ms: u64) -> Self {
        Self {
            min_chunk_ms,
            silence_split_ms,
            max_chunk_ms,
            vad_frame_ms: VAD_FRAME_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceActivity {
    Voice,
    Silence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplitReason {
    Silence,
    MaxDuration,
    FinalFlush,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecordedAudioChunk {
    pub(crate) sequence: u64,
    pub(crate) audio: RecordedAudio,
    pub(crate) reason: SplitReason,
}

#[derive(Debug)]
pub(crate) struct ChunkedRecordingSink {
    tx: Sender<ChunkedRecordingMessage>,
    worker: Option<JoinHandle<()>>,
    cancellation_token: CancellationToken,
}

impl ChunkedRecordingSink {
    pub(crate) fn start(
        sample_rate: u32,
        channels: u16,
        placeholder_id: String,
        config: Config,
        app_event_tx: AppEventSender,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancellation_token = CancellationToken::new();
        let worker_cancellation_token = cancellation_token.clone();
        let worker = std::thread::spawn(move || {
            let mut worker = ChunkedRecordingWorker::new(
                sample_rate,
                channels,
                placeholder_id,
                config,
                app_event_tx,
                worker_cancellation_token,
            );
            worker.run(rx);
        });
        Self {
            tx,
            worker: Some(worker),
            cancellation_token,
        }
    }

    pub(crate) fn sender(&self) -> Sender<ChunkedRecordingMessage> {
        self.tx.clone()
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    pub(crate) fn finish(mut self) {
        let _ = self.tx.send(ChunkedRecordingMessage::Finish);
        self.join();
    }

    pub(crate) fn cancel(mut self) {
        self.cancellation_token.cancel();
        let _ = self.tx.send(ChunkedRecordingMessage::Cancel);
        self.join();
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            error!("dictation chunk worker panicked");
        }
    }
}

impl Drop for ChunkedRecordingSink {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.cancellation_token.cancel();
            let _ = self.tx.send(ChunkedRecordingMessage::Cancel);
            self.join();
        }
    }
}

#[derive(Debug)]
pub(crate) enum ChunkedRecordingMessage {
    Samples(Vec<i16>),
    Finish,
    Cancel,
}

struct ChunkedRecordingWorker {
    chunker: AudioChunker,
    placeholder_id: String,
    config: Config,
    app_event_tx: AppEventSender,
    cancellation_token: CancellationToken,
}

#[derive(Debug, PartialEq, Eq)]
struct DictationFlushPlan {
    final_sequence: Option<u64>,
    final_chunk_minimum_duration_policy: Option<MinimumDurationPolicy>,
}

fn plan_dictation_flush(
    final_chunk: Option<&RecordedAudioChunk>,
    last_sequence: Option<u64>,
) -> DictationFlushPlan {
    DictationFlushPlan {
        final_sequence: final_chunk.map(|chunk| chunk.sequence).or(last_sequence),
        final_chunk_minimum_duration_policy: final_chunk.map(|chunk| {
            if chunk.sequence == 0 {
                MinimumDurationPolicy::Enforce
            } else {
                MinimumDurationPolicy::AllowShort
            }
        }),
    }
}

impl ChunkedRecordingWorker {
    fn new(
        sample_rate: u32,
        channels: u16,
        placeholder_id: String,
        config: Config,
        app_event_tx: AppEventSender,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            chunker: AudioChunker::new(sample_rate, channels, ChunkPolicy::default()),
            placeholder_id,
            config,
            app_event_tx,
            cancellation_token,
        }
    }

    fn run(&mut self, rx: Receiver<ChunkedRecordingMessage>) {
        while let Ok(message) = rx.recv() {
            match message {
                ChunkedRecordingMessage::Samples(samples) => self.process_samples(&samples),
                ChunkedRecordingMessage::Finish => {
                    self.flush();
                    return;
                }
                ChunkedRecordingMessage::Cancel => return,
            }
        }
    }

    fn process_samples(&mut self, samples: &[i16]) {
        for chunk in self.chunker.push(samples) {
            self.transcribe_chunk(chunk, MinimumDurationPolicy::Enforce);
        }
    }

    fn flush(&mut self) {
        let final_chunk = self.chunker.finish();
        let flush_plan = plan_dictation_flush(final_chunk.as_ref(), self.chunker.last_sequence());

        if let (Some(chunk), Some(minimum_duration_policy)) =
            (final_chunk, flush_plan.final_chunk_minimum_duration_policy)
        {
            self.transcribe_chunk(chunk, minimum_duration_policy);
        }

        self.app_event_tx.send(AppEvent::DictationChunksFlushed {
            id: self.placeholder_id.clone(),
            final_sequence: flush_plan.final_sequence,
        });
    }

    fn transcribe_chunk(
        &self,
        chunk: RecordedAudioChunk,
        minimum_duration_policy: MinimumDurationPolicy,
    ) {
        info!(
            sequence = chunk.sequence,
            reason = ?chunk.reason,
            "transcribing dictation chunk"
        );
        transcribe_chunk_async(
            self.placeholder_id.clone(),
            chunk.sequence,
            chunk.audio,
            self.config.clone(),
            self.app_event_tx.clone(),
            minimum_duration_policy,
            self.cancellation_token.clone(),
        );
    }
}

struct AudioChunker {
    sample_rate: u32,
    channels: u16,
    policy: ChunkPolicy,
    vad: Vad,
    boundary_detector: ChunkBoundaryDetector,
    current_audio: Vec<i16>,
    vad_buffer: Vec<i16>,
    next_sequence: u64,
    current_has_voice: bool,
}

impl AudioChunker {
    fn new(sample_rate: u32, channels: u16, policy: ChunkPolicy) -> Self {
        Self {
            sample_rate,
            channels,
            policy,
            vad: Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VadMode::Aggressive),
            boundary_detector: ChunkBoundaryDetector::new(policy),
            current_audio: Vec::new(),
            vad_buffer: Vec::new(),
            next_sequence: 0,
            current_has_voice: false,
        }
    }

    fn push(&mut self, samples: &[i16]) -> Vec<RecordedAudioChunk> {
        let mut chunks = Vec::new();
        if samples.is_empty() || self.sample_rate == 0 || self.channels == 0 {
            return chunks;
        }

        let mut offset = 0usize;
        let max_samples = self.max_samples_per_chunk();
        while offset < samples.len() {
            let space = max_samples.saturating_sub(self.current_audio.len());
            let frame_aligned_space = space - (space % usize::from(self.channels));
            if frame_aligned_space == 0 {
                if let Some(chunk) = self.split(SplitReason::MaxDuration) {
                    chunks.push(chunk);
                }
                continue;
            }

            let take = frame_aligned_space.min(samples.len() - offset);
            let next_offset = offset + take;
            let part = &samples[offset..next_offset];
            self.current_audio.extend_from_slice(part);
            self.append_vad_samples(part);

            let reason = self.process_vad_frames().or_else(|| {
                (self.current_duration_ms() >= self.policy.max_chunk_ms)
                    .then_some(SplitReason::MaxDuration)
            });
            if let Some(reason) = reason
                && let Some(chunk) = self.split(reason)
            {
                chunks.push(chunk);
            }

            offset = next_offset;
        }

        chunks
    }

    fn finish(&mut self) -> Option<RecordedAudioChunk> {
        if self.current_audio.is_empty() {
            return None;
        }
        self.split(SplitReason::FinalFlush)
    }

    fn last_sequence(&self) -> Option<u64> {
        self.next_sequence.checked_sub(/*rhs*/ 1)
    }

    fn split(&mut self, reason: SplitReason) -> Option<RecordedAudioChunk> {
        let sequence = self.next_sequence;
        let keep_audio = self.current_has_voice || has_meaningful_signal(&self.current_audio);
        self.boundary_detector.reset();
        self.vad_buffer.clear();
        self.current_has_voice = false;
        let audio = RecordedAudio {
            data: std::mem::take(&mut self.current_audio),
            sample_rate: self.sample_rate,
            channels: self.channels,
        };
        if !keep_audio {
            info!(?reason, "dropping dictation chunk without voice or signal");
            return None;
        }
        self.next_sequence = self.next_sequence.saturating_add(/*rhs*/ 1);
        Some(RecordedAudioChunk {
            sequence,
            audio,
            reason,
        })
    }

    fn append_vad_samples(&mut self, samples: &[i16]) {
        let vad_samples = if self.sample_rate == VAD_SAMPLE_RATE && self.channels == VAD_CHANNELS {
            samples.to_vec()
        } else {
            convert_pcm16(
                samples,
                self.sample_rate,
                self.channels,
                VAD_SAMPLE_RATE,
                VAD_CHANNELS,
            )
        };
        self.vad_buffer.extend(vad_samples);
    }

    fn process_vad_frames(&mut self) -> Option<SplitReason> {
        let frame_samples = self.vad_frame_samples();
        while self.vad_buffer.len() >= frame_samples {
            let frame = self.vad_buffer[..frame_samples].to_vec();
            self.vad_buffer.drain(..frame_samples);
            let activity = match self.vad.is_voice_segment(&frame) {
                Ok(true) => VoiceActivity::Voice,
                Ok(false) => VoiceActivity::Silence,
                Err(()) => {
                    error!("WebRTC VAD rejected a dictation audio frame");
                    VoiceActivity::Voice
                }
            };
            if activity == VoiceActivity::Voice {
                self.current_has_voice = true;
            }
            if let Some(reason) = self
                .boundary_detector
                .observe(activity, self.current_duration_ms())
            {
                return Some(reason);
            }
        }
        None
    }

    fn max_samples_per_chunk(&self) -> usize {
        samples_for_duration(self.sample_rate, self.channels, self.policy.max_chunk_ms)
            .max(usize::from(self.channels))
    }

    fn vad_frame_samples(&self) -> usize {
        samples_for_duration(VAD_SAMPLE_RATE, VAD_CHANNELS, self.policy.vad_frame_ms)
            .max(usize::from(VAD_CHANNELS))
    }

    fn current_duration_ms(&self) -> u64 {
        duration_ms(self.current_audio.len(), self.sample_rate, self.channels)
    }
}

#[derive(Debug)]
pub(crate) struct ChunkBoundaryDetector {
    policy: ChunkPolicy,
    consecutive_silence_ms: u64,
}

impl ChunkBoundaryDetector {
    pub(crate) fn new(policy: ChunkPolicy) -> Self {
        Self {
            policy,
            consecutive_silence_ms: 0,
        }
    }

    fn observe(
        &mut self,
        activity: VoiceActivity,
        current_duration_ms: u64,
    ) -> Option<SplitReason> {
        if current_duration_ms >= self.policy.max_chunk_ms {
            return Some(SplitReason::MaxDuration);
        }

        match activity {
            VoiceActivity::Voice => {
                self.consecutive_silence_ms = 0;
            }
            VoiceActivity::Silence => {
                self.consecutive_silence_ms = self
                    .consecutive_silence_ms
                    .saturating_add(self.policy.vad_frame_ms);
            }
        }

        if current_duration_ms >= self.policy.min_chunk_ms
            && self.consecutive_silence_ms >= self.policy.silence_split_ms
        {
            Some(SplitReason::Silence)
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.consecutive_silence_ms = 0;
    }
}

fn samples_for_duration(sample_rate: u32, channels: u16, duration_ms: u64) -> usize {
    let frames = u64::from(sample_rate).saturating_mul(duration_ms) / 1_000;
    let samples = frames.saturating_mul(u64::from(channels));
    usize::try_from(samples).unwrap_or(usize::MAX)
}

fn duration_ms(sample_count: usize, sample_rate: u32, channels: u16) -> u64 {
    if sample_rate == 0 || channels == 0 {
        return 0;
    }
    let frames = (sample_count as u64) / u64::from(channels);
    frames.saturating_mul(/*rhs*/ 1_000) / u64::from(sample_rate)
}

fn has_meaningful_signal(samples: &[i16]) -> bool {
    if samples.is_empty() {
        return false;
    }

    let mut sum_squares = 0.0;
    for sample in samples {
        let amplitude = sample.unsigned_abs();
        if amplitude >= SIGNAL_PEAK_THRESHOLD as u16 {
            return true;
        }
        let amplitude = f64::from(amplitude);
        sum_squares += amplitude * amplitude;
    }
    (sum_squares / samples.len() as f64).sqrt() >= SIGNAL_RMS_THRESHOLD
}

#[cfg(test)]
#[path = "chunked_tests.rs"]
mod tests;
