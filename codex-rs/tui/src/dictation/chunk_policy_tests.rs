use super::ChunkBoundary;
use super::ChunkPolicy;
use pretty_assertions::assert_eq;

fn observe_continue(policy: &mut ChunkPolicy, frames: u64, has_speech: bool) {
    for _ in 0..frames {
        assert_eq!(
            policy.observe_frame(has_speech),
            ChunkBoundary::Continue,
            "unexpected early split"
        );
    }
}

#[test]
fn splits_after_minimum_duration_and_trailing_silence() {
    let mut policy = ChunkPolicy::default();
    observe_continue(&mut policy, /*frames*/ 700, /*has_speech*/ true);
    observe_continue(&mut policy, /*frames*/ 49, /*has_speech*/ false);
    assert_eq!(
        policy.observe_frame(/*has_speech*/ false),
        ChunkBoundary::Split
    );
    assert!(!policy.finish());
}

#[test]
fn speech_resets_trailing_silence_before_threshold() {
    let mut policy = ChunkPolicy::default();
    observe_continue(&mut policy, /*frames*/ 749, /*has_speech*/ true);
    observe_continue(&mut policy, /*frames*/ 49, /*has_speech*/ false);
    assert_eq!(
        policy.observe_frame(/*has_speech*/ true),
        ChunkBoundary::Continue
    );
    observe_continue(&mut policy, /*frames*/ 49, /*has_speech*/ false);
    assert!(policy.finish());
}

#[test]
fn maximum_duration_splits_at_exact_three_thousand_frames() {
    let mut policy = ChunkPolicy::default();
    observe_continue(&mut policy, /*frames*/ 2_999, /*has_speech*/ true);
    assert_eq!(
        policy.observe_frame(/*has_speech*/ true),
        ChunkBoundary::Split
    );
    assert!(!policy.finish());
}

#[test]
fn split_resets_policy_and_short_finish_is_reported() {
    let mut policy = ChunkPolicy::default();
    observe_continue(&mut policy, /*frames*/ 749, /*has_speech*/ true);
    observe_continue(&mut policy, /*frames*/ 49, /*has_speech*/ false);
    assert_eq!(
        policy.observe_frame(/*has_speech*/ false),
        ChunkBoundary::Split
    );
    assert_eq!(
        policy.observe_frame(/*has_speech*/ true),
        ChunkBoundary::Continue
    );
    assert!(policy.finish());
    assert!(!policy.finish());
}

#[test]
fn finish_reports_full_frames_once_and_split_resets() {
    let mut policy = ChunkPolicy::default();
    assert!(!policy.finish());
    assert_eq!(
        policy.observe_frame(/*has_speech*/ true),
        ChunkBoundary::Continue
    );
    assert!(policy.finish());
    assert!(!policy.finish());
}

#[test]
fn silence_alone_still_waits_for_minimum_duration() {
    let mut policy = ChunkPolicy::default();
    observe_continue(&mut policy, /*frames*/ 749, /*has_speech*/ false);
    assert_eq!(
        policy.observe_frame(/*has_speech*/ false),
        ChunkBoundary::Split
    );
    assert!(!policy.finish());
}
