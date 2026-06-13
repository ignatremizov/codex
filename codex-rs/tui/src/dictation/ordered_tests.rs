use super::OrderedTranscript;
use super::ResolvedChunk;
use pretty_assertions::assert_eq;

#[test]
fn drains_reverse_completion_in_sequence_order() {
    let mut transcript = OrderedTranscript::default();
    assert!(
        transcript
            .resolve(/*sequence*/ 2, Ok("two".into()))
            .is_empty()
    );
    assert!(transcript.resolve(/*sequence*/ 1, Ok("".into())).is_empty());
    assert!(
        transcript
            .resolve(/*sequence*/ 2, Ok("duplicate".into()))
            .is_empty()
    );
    assert_eq!(
        transcript.resolve(/*sequence*/ 0, Ok("零🙂".into())),
        vec![
            ResolvedChunk {
                sequence: 0,
                result: Ok("零🙂".into()),
            },
            ResolvedChunk {
                sequence: 1,
                result: Ok(String::new()),
            },
            ResolvedChunk {
                sequence: 2,
                result: Ok("two".into()),
            },
        ]
    );
}

#[test]
fn failures_advance_gaps_and_duplicates_are_ignored() {
    let mut transcript = OrderedTranscript::default();
    assert!(
        transcript
            .resolve(/*sequence*/ 3, Ok("late".into()))
            .is_empty()
    );
    assert_eq!(
        transcript.resolve(/*sequence*/ 0, Err("failed".into())),
        vec![ResolvedChunk {
            sequence: 0,
            result: Err("failed".into()),
        }]
    );
    assert!(
        transcript
            .resolve(/*sequence*/ 0, Ok("duplicate".into()))
            .is_empty()
    );
    assert_eq!(
        transcript.resolve(/*sequence*/ 1, Ok("one".into())),
        vec![ResolvedChunk {
            sequence: 1,
            result: Ok("one".into()),
        }]
    );
    assert_eq!(
        transcript.resolve(/*sequence*/ 2, Ok("two".into())),
        vec![
            ResolvedChunk {
                sequence: 2,
                result: Ok("two".into()),
            },
            ResolvedChunk {
                sequence: 3,
                result: Ok("late".into()),
            },
        ]
    );
}

#[test]
fn preserves_long_unicode_and_empty_results_exactly() {
    let mut transcript = OrderedTranscript::default();
    let text = "é".repeat(/*n*/ 20_000);
    let resolved = transcript.resolve(/*sequence*/ 0, Ok(text.clone()));
    assert_eq!(
        resolved,
        vec![ResolvedChunk {
            sequence: 0,
            result: Ok(text),
        }]
    );
    assert_eq!(
        transcript.resolve(/*sequence*/ 1, Ok(String::new())),
        vec![ResolvedChunk {
            sequence: 1,
            result: Ok(String::new()),
        }]
    );
}

#[test]
fn rejects_results_after_maximum_sequence_is_consumed() {
    let mut transcript = OrderedTranscript {
        next_sequence: u64::MAX,
        ..Default::default()
    };
    assert_eq!(
        transcript.resolve(u64::MAX, Ok("last".into())),
        vec![ResolvedChunk {
            sequence: u64::MAX,
            result: Ok("last".into()),
        }]
    );
    assert!(transcript.exhausted);
    assert!(
        transcript
            .resolve(u64::MAX, Ok("duplicate".into()))
            .is_empty()
    );
}
