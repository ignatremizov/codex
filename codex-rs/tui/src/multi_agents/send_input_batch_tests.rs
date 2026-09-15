use super::*;
use codex_protocol::CollabAgentInputResult;
use pretty_assertions::assert_eq;

#[test]
fn batch_mailbox_rows_distinguish_acceptance_errors_and_show_message_once() {
    let receiver = ThreadId::new();
    let batch = CollabAgentInputBatch {
        flags: "zf".into(),
        results: vec![
            CollabAgentInputResult {
                target: "43".into(),
                receiver_thread_id: Some(receiver.to_string()),
                status: CollabAgentInputStatus::MailboxAccepted,
                error: None,
                hint: Some("Resume the receiver first.".into()),
            },
            CollabAgentInputResult {
                target: "missing".into(),
                receiver_thread_id: None,
                status: CollabAgentInputStatus::Error,
                error: Some("Unknown selector.".into()),
                hint: None,
            },
        ],
    };
    let cell = history_cell(
        &batch,
        &CollabAgentToolCallStatus::Failed,
        "Read these notes when ready.",
        V1ResponseObservation {
            observe_commentary: Some(false),
            wake_on_completion: Some(false),
            target_messages: Some(false),
            queue_input: Some(false),
        },
        /*preview_lines*/ 0,
        &mut |_| AgentMetadata {
            agent_ref: Some("43".into()),
            agent_nickname: Some("Darwin".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let text = cell
        .transcript_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    • Input batch completed with errors
      └ Saved to mailbox for Darwin (43) (mailbox · conditional final wake)
        Resume the receiver first.
        Send error for missing (mailbox · conditional final wake)
        Unknown selector.
        Read these notes when ready.
    ");
    assert_eq!(text.matches("Read these notes when ready.").count(), 1);
}

#[test]
fn live_batch_rows_have_independent_admission_labels_and_shared_flags() {
    let batch = CollabAgentInputBatch {
        flags: "cmq".into(),
        results: vec![
            CollabAgentInputResult {
                target: "43".into(),
                receiver_thread_id: None,
                status: CollabAgentInputStatus::Submitted,
                error: None,
                hint: None,
            },
            CollabAgentInputResult {
                target: "44".into(),
                receiver_thread_id: None,
                status: CollabAgentInputStatus::Queued,
                error: None,
                hint: None,
            },
        ],
    };
    let cell = history_cell(
        &batch,
        &CollabAgentToolCallStatus::Completed,
        "Shared message.",
        V1ResponseObservation {
            observe_commentary: Some(true),
            wake_on_completion: Some(false),
            target_messages: Some(true),
            queue_input: Some(true),
        },
        /*preview_lines*/ 0,
        &mut |_| AgentMetadata::default(),
    )
    .unwrap();
    let text = cell
        .transcript_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    • Input batch completed
      └ Sent input to 43 (receive commentary · no wake on completion · allow replies · queue turn + reply)
        Queued input for 44 (receive commentary · no wake on completion · allow replies · queue turn + reply)
        Shared message.
    ");
}

#[test]
fn one_receiver_array_keeps_batch_presentation_at_narrow_width() {
    let batch = CollabAgentInputBatch {
        flags: "z".into(),
        results: vec![CollabAgentInputResult {
            target: "43".into(),
            receiver_thread_id: None,
            status: CollabAgentInputStatus::MailboxAccepted,
            error: None,
            hint: None,
        }],
    };
    let cell = history_cell(
        &batch,
        &CollabAgentToolCallStatus::Completed,
        "One message.",
        V1ResponseObservation {
            observe_commentary: Some(false),
            wake_on_completion: None,
            target_messages: Some(false),
            queue_input: Some(false),
        },
        /*preview_lines*/ 0,
        &mut |_| AgentMetadata::default(),
    )
    .unwrap();
    let text = cell
        .display_lines(/*width*/ 40)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    • Input batch completed
      └ Saved to mailbox for 43 (mailbox)
        One message.
    ");
}
