use super::super::DictationFinishAction;
use super::super::DictationSession;
use super::*;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;

#[test]
fn ordered_transcript_appends_out_of_order_completions_in_sequence() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 1, "second".to_string());
    assert_eq!(transcript.text(), "");

    transcript.complete(/*sequence*/ 0, "first".to_string());
    assert_eq!(transcript.text(), "first second");
}

#[test]
fn failed_chunk_unblocks_later_transcript_text() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 2, "third".to_string());
    transcript.fail(/*sequence*/ 1);
    assert_eq!(transcript.text(), "");

    transcript.complete(/*sequence*/ 0, "first".to_string());
    assert_eq!(transcript.text(), "first third");
}

#[test]
fn final_flush_waits_for_missing_chunk_result() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 1, "second".to_string());
    transcript.set_final_sequence(Some(1));
    assert!(!transcript.is_finished());

    transcript.fail(/*sequence*/ 0);
    assert!(transcript.is_finished());
    assert_eq!(transcript.text(), "second");
}

#[test]
fn final_flush_waits_for_last_sequence_result() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 0, "first".to_string());
    assert!(!transcript.is_finished());

    transcript.set_final_sequence(Some(1));

    assert!(!transcript.is_finished());

    transcript.complete(/*sequence*/ 1, "second".to_string());
    assert!(transcript.is_finished());
    assert_eq!(transcript.text(), "first second");
}

#[test]
fn duplicate_pending_sequence_results_do_not_replace_first_result() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 1, "second".to_string());
    transcript.fail(/*sequence*/ 1);
    transcript.complete(/*sequence*/ 1, "replacement".to_string());
    transcript.complete(/*sequence*/ 0, "first".to_string());

    assert_eq!(transcript.text(), "first second");
}

#[test]
fn stale_sequence_results_do_not_change_transcript_text() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 0, "first".to_string());
    transcript.complete(/*sequence*/ 0, "stale replacement".to_string());
    transcript.fail(/*sequence*/ 0);
    transcript.complete(/*sequence*/ 1, "second".to_string());

    assert_eq!(transcript.text(), "first second");
}

#[test]
fn empty_final_flush_finishes_without_text() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.set_final_sequence(None);

    assert!(transcript.is_finished());
    assert_eq!(transcript.text(), "");
}

#[test]
fn transcript_preserves_long_user_dictation_text() {
    let mut transcript = OrderedDictationTranscript::default();
    let long_text = "a".repeat(120_000);

    transcript.complete(/*sequence*/ 0, long_text.clone());

    assert_eq!(transcript.text(), long_text);
}

#[test]
fn transcript_inserts_separator_between_chunks_without_trailing_space() {
    let mut transcript = OrderedDictationTranscript::default();

    transcript.complete(/*sequence*/ 0, "first".to_string());
    transcript.complete(/*sequence*/ 1, "second".to_string());

    assert_eq!(transcript.text(), "first second");
}

#[test]
fn dictation_session_preserves_long_multi_chunk_text_through_final_action() {
    let mut session = DictationSession::new("placeholder".to_string(), "⠋".to_string());
    let first = "a".repeat(80_000);
    let second = "b".repeat(80_000);
    let expected = format!("{first} {second}");

    session.complete_chunk(/*sequence*/ 1, second);
    session.complete_chunk(/*sequence*/ 0, first);
    session.flush_chunks(Some(1));

    assert_eq!(session.transcript.text(), expected);
    assert_eq!(
        session.finish_action(),
        Some(DictationFinishAction::Replace {
            placeholder_id: "placeholder".to_string(),
            text: expected,
        })
    );
}

#[test]
fn dictation_session_renders_incremental_chunks_and_final_replacement_text() {
    let mut session = DictationSession::new("placeholder".to_string(), "⠋".to_string());

    let second_update = session.complete_chunk(/*sequence*/ 1, "second".to_string());
    assert_eq!(second_update.rendered_placeholder, "⠋");
    assert!(!second_update.empty_recording);

    let first_update = session.complete_chunk(/*sequence*/ 0, "first".to_string());
    assert_eq!(first_update.rendered_placeholder, "first second ⠋");
    assert_snapshot!(session.render_placeholder(), @"first second ⠋");
    assert!(!first_update.empty_recording);

    let flush_update = session.flush_chunks(Some(1));
    assert_eq!(flush_update.rendered_placeholder, "first second ⠋");
    assert!(!flush_update.empty_recording);
    assert!(session.transcript.is_finished());
    assert_eq!(session.transcript.text(), "first second");
    assert_eq!(
        session.finish_action(),
        Some(DictationFinishAction::Replace {
            placeholder_id: "placeholder".to_string(),
            text: "first second".to_string(),
        })
    );
}

#[test]
fn dictation_session_failed_segment_warns_but_unblocks_final_text() {
    let mut session = DictationSession::new("placeholder".to_string(), "⠋".to_string());

    session.complete_chunk(/*sequence*/ 2, "third".to_string());
    let failed_update = session.fail_chunk(/*sequence*/ 1);
    assert_eq!(failed_update.rendered_placeholder, "⠋");

    let first_update = session.complete_chunk(/*sequence*/ 0, "first".to_string());
    assert_eq!(first_update.rendered_placeholder, "first third ⠋");

    let flush_update = session.flush_chunks(Some(2));
    assert_eq!(flush_update.rendered_placeholder, "first third ⠋");
    assert!(session.transcript.is_finished());
    assert_eq!(session.transcript.text(), "first third");
}

#[test]
fn dictation_session_empty_flush_signals_placeholder_removal_warning() {
    let mut session = DictationSession::new("placeholder".to_string(), "⠋".to_string());

    let flush_update = session.flush_chunks(/*final_sequence*/ None);

    assert_eq!(flush_update.rendered_placeholder, "⠋");
    assert!(flush_update.empty_recording);
    assert!(session.transcript.is_finished());
    assert_eq!(session.transcript.text(), "");
    assert_eq!(
        session.finish_action(),
        Some(DictationFinishAction::Remove {
            placeholder_id: "placeholder".to_string(),
        })
    );
}
