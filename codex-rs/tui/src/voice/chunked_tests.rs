use super::*;
use pretty_assertions::assert_eq;

#[test]
fn silence_split_after_minimum_duration() {
    let policy = ChunkPolicy::for_test(
        /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
    );
    let mut detector = ChunkBoundaryDetector::new(policy);

    assert_eq!(
        detector.observe(VoiceActivity::Silence, /*current_duration_ms*/ 80),
        None
    );
    assert_eq!(
        detector.observe(VoiceActivity::Silence, /*current_duration_ms*/ 100),
        Some(SplitReason::Silence)
    );
}

#[test]
fn voice_resets_accumulated_silence_before_split() {
    let policy = ChunkPolicy::for_test(
        /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
    );
    let mut detector = ChunkBoundaryDetector::new(policy);

    assert_eq!(
        detector.observe(VoiceActivity::Silence, /*current_duration_ms*/ 100),
        None
    );
    assert_eq!(
        detector.observe(VoiceActivity::Voice, /*current_duration_ms*/ 120),
        None
    );
    assert_eq!(
        detector.observe(VoiceActivity::Silence, /*current_duration_ms*/ 140),
        None
    );
    assert_eq!(
        detector.observe(VoiceActivity::Silence, /*current_duration_ms*/ 160),
        Some(SplitReason::Silence)
    );
}

#[test]
fn forced_max_duration_split_with_no_silence() {
    let policy = ChunkPolicy::for_test(
        /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 200,
    );
    let mut detector = ChunkBoundaryDetector::new(policy);

    assert_eq!(
        detector.observe(VoiceActivity::Voice, /*current_duration_ms*/ 200),
        Some(SplitReason::MaxDuration)
    );
}

#[test]
fn audio_chunker_splits_continuous_samples_at_max_duration() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 16_000,
        /*channels*/ 1,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 1_000, /*silence_split_ms*/ 1_000, /*max_chunk_ms*/ 40,
        ),
    );
    let first_chunk_samples = vec![1; 640];
    let remaining_samples = vec![2; 320];
    let mut samples = first_chunk_samples.clone();
    samples.extend_from_slice(&remaining_samples);
    chunker.current_has_voice = true;

    assert_eq!(
        chunker.push(&samples),
        vec![RecordedAudioChunk {
            sequence: 0,
            audio: RecordedAudio {
                data: first_chunk_samples,
                sample_rate: 16_000,
                channels: 1,
            },
            reason: SplitReason::MaxDuration,
        }]
    );
    assert_eq!(chunker.last_sequence(), Some(0));
    chunker.current_has_voice = true;
    assert_eq!(
        chunker.finish(),
        Some(RecordedAudioChunk {
            sequence: 1,
            audio: RecordedAudio {
                data: remaining_samples,
                sample_rate: 16_000,
                channels: 1,
            },
            reason: SplitReason::FinalFlush,
        })
    );
}

#[test]
fn final_flush_emits_remaining_chunk() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 16_000,
        /*channels*/ 1,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
        ),
    );
    let samples = vec![1; 160];

    assert!(chunker.push(&samples).is_empty());
    chunker.current_has_voice = true;
    let chunk = chunker.finish().expect("remaining audio should flush");

    assert_eq!(
        chunk,
        RecordedAudioChunk {
            sequence: 0,
            audio: RecordedAudio {
                data: samples,
                sample_rate: 16_000,
                channels: 1,
            },
            reason: SplitReason::FinalFlush,
        }
    );
}

#[test]
fn flush_plan_reports_no_final_sequence_for_empty_recording() {
    assert_eq!(
        plan_dictation_flush(/*final_chunk*/ None, /*last_sequence*/ None),
        DictationFlushPlan {
            final_sequence: None,
            final_chunk_minimum_duration_policy: None,
        }
    );
}

#[test]
fn flush_plan_keeps_last_sequence_when_prior_chunks_have_no_final_chunk() {
    assert_eq!(
        plan_dictation_flush(/*final_chunk*/ None, Some(3)),
        DictationFlushPlan {
            final_sequence: Some(3),
            final_chunk_minimum_duration_policy: None,
        }
    );
}

#[test]
fn flush_plan_enforces_minimum_duration_for_first_final_chunk() {
    let final_chunk = RecordedAudioChunk {
        sequence: 0,
        audio: RecordedAudio {
            data: vec![1; 160],
            sample_rate: 16_000,
            channels: 1,
        },
        reason: SplitReason::FinalFlush,
    };

    assert_eq!(
        plan_dictation_flush(Some(&final_chunk), /*last_sequence*/ None),
        DictationFlushPlan {
            final_sequence: Some(0),
            final_chunk_minimum_duration_policy: Some(MinimumDurationPolicy::Enforce),
        }
    );
}

#[test]
fn flush_plan_allows_short_audio_for_non_first_final_chunk() {
    let final_chunk = RecordedAudioChunk {
        sequence: 2,
        audio: RecordedAudio {
            data: vec![1; 160],
            sample_rate: 16_000,
            channels: 1,
        },
        reason: SplitReason::FinalFlush,
    };

    assert_eq!(
        plan_dictation_flush(Some(&final_chunk), Some(1)),
        DictationFlushPlan {
            final_sequence: Some(2),
            final_chunk_minimum_duration_policy: Some(MinimumDurationPolicy::AllowShort),
        }
    );
}

#[test]
fn audio_chunker_drops_silence_from_actual_vad_input() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 16_000,
        /*channels*/ 1,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 40, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
        ),
    );
    let samples = vec![0; samples_for_duration(16_000, /*channels*/ 1, /*duration_ms*/ 60)];

    assert!(chunker.push(&samples).is_empty());
    assert_eq!(chunker.last_sequence(), None);
    assert_eq!(chunker.finish(), None);
}

#[test]
fn audio_chunker_drops_resampled_stereo_silence_from_actual_vad_input() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 48_000,
        /*channels*/ 2,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 40, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
        ),
    );
    let samples = vec![0; samples_for_duration(48_000, /*channels*/ 2, /*duration_ms*/ 60)];

    assert!(chunker.push(&samples).is_empty());
    assert_eq!(chunker.last_sequence(), None);
    assert_eq!(chunker.finish(), None);
}

#[test]
fn final_flush_keeps_signal_even_without_vad_voice_detection() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 16_000,
        /*channels*/ 1,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
        ),
    );
    let samples = vec![300; 160];

    assert!(chunker.push(&samples).is_empty());

    assert_eq!(
        chunker.finish(),
        Some(RecordedAudioChunk {
            sequence: 0,
            audio: RecordedAudio {
                data: samples,
                sample_rate: 16_000,
                channels: 1,
            },
            reason: SplitReason::FinalFlush,
        })
    );
}

#[test]
fn final_flush_drops_near_zero_noise_without_vad_voice_detection() {
    let mut chunker = AudioChunker::new(
        /*sample_rate*/ 16_000,
        /*channels*/ 1,
        ChunkPolicy::for_test(
            /*min_chunk_ms*/ 100, /*silence_split_ms*/ 40, /*max_chunk_ms*/ 1_000,
        ),
    );

    assert!(chunker.push(&vec![1; 160]).is_empty());

    assert_eq!(chunker.finish(), None);
}
