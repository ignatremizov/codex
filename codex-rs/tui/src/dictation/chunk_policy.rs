const FRAME_MILLISECONDS: u64 = 20;
const MINIMUM_CHUNK_MILLISECONDS: u64 = 15_000;
const SILENCE_SPLIT_MILLISECONDS: u64 = 1_000;
const MAXIMUM_CHUNK_MILLISECONDS: u64 = 60_000;

/// Whether the capture owner should continue or begin a new audio chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkBoundary {
    Continue,
    Split,
}

/// Tracks complete 20 ms frames; PCM tails remain owned by the capture layer.
#[derive(Debug, Default)]
pub(crate) struct ChunkPolicy {
    full_frames: u64,
    trailing_silence_frames: u64,
}

impl ChunkPolicy {
    /// Observes one full 16 kHz mono frame and applies the duration/silence boundaries.
    pub(crate) fn observe_frame(&mut self, has_speech: bool) -> ChunkBoundary {
        self.full_frames = self.full_frames.saturating_add(1);
        if has_speech {
            self.trailing_silence_frames = 0;
        } else {
            self.trailing_silence_frames = self.trailing_silence_frames.saturating_add(1);
        }

        let duration_ms = self.full_frames.saturating_mul(FRAME_MILLISECONDS);
        let silence_ms = self
            .trailing_silence_frames
            .saturating_mul(FRAME_MILLISECONDS);
        if duration_ms >= MAXIMUM_CHUNK_MILLISECONDS
            || (duration_ms >= MINIMUM_CHUNK_MILLISECONDS
                && silence_ms >= SILENCE_SPLIT_MILLISECONDS)
        {
            self.reset();
            ChunkBoundary::Split
        } else {
            ChunkBoundary::Continue
        }
    }

    /// Reports and clears whether this chunk contains any complete frames.
    pub(crate) fn finish(&mut self) -> bool {
        let had_full_frames = self.full_frames > 0;
        self.reset();
        had_full_frames
    }

    fn reset(&mut self) {
        self.full_frames = 0;
        self.trailing_silence_frames = 0;
    }
}

#[cfg(test)]
#[path = "chunk_policy_tests.rs"]
mod tests;
