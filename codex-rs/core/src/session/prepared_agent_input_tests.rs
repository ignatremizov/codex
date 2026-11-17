use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::RolloutItem;
use codex_protocol::AgentInputAttribution;
use codex_protocol::AgentInputIdentity;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::protocol::AgentQueueTurnMetadata;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

fn input_for(session: &Session, audit: &str) -> PreparedAgentInput {
    let identity = |thread_id| AgentInputIdentity {
        thread_id,
        nickname: None,
        agent_ref: None,
        task_path: None,
        role: None,
        model: None,
        reasoning_effort: None,
    };
    let mut communication = InterAgentCommunication::new_encrypted(
        AgentPath::root(),
        AgentPath::try_from("/root/receiver").expect("receiver path"),
        Vec::new(),
        "opaque-prepared-body".to_string(),
        /*trigger_turn*/ true,
    );
    communication.content = audit.to_owned();
    PreparedAgentInput {
        communication,
        attribution: Box::new(AgentInputAttribution {
            sender: identity(ThreadId::new()),
            recipient: identity(session.thread_id()),
            sender_turn_id: "original-sender-turn".to_string(),
            batch_id: Some("original-batch".to_string()),
        }),
        sender_instance_id: uuid::Uuid::now_v7(),
        recipient_instance_id: session.presentation_id().instance_id,
        call_id: "original-invocation".to_string(),
    }
}

#[test_case(""; "encrypted without manufactured audit")]
#[test_case("Readable é audit that must remain outside the model payload.\n"; "encrypted with full audit")]
#[tokio::test]
async fn prepared_agent_input_publication_retains_canonical_pair_and_settles_receipt(audit: &str) {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let input = input_for(&session, &audit.repeat(180));
    let queued = AgentQueueTurnMetadata {
        queue_id: "prepared-queue".to_string(),
        source_thread_id: input.attribution.sender.thread_id,
        response_handling: None,
    };
    let (permit, mut persisted) = session.register_queued_input_start("prepared", queued);
    let resolution =
        session.capture_input_turn_admission_resolution(turn.sub_id.clone(), turn.root_turn_id());
    session.resolve_input_turn_admission("prepared", resolution);
    permit.publish();
    assert!(persisted.try_recv().is_err());
    session
        .record_prepared_agent_input(
            &turn,
            turn.model_info(),
            Box::new(input.clone()),
            UserInputMetadata {
                acceptance_order: Some(17),
                ..Default::default()
            },
            PersistContext::Standard,
        )
        .await
        .expect("paired canonical publication");
    persisted
        .try_recv()
        .expect("publication receipt")
        .expect("canonical commit succeeded");
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        })
        .await
        .expect("canonical history");
    let recorded = history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(envelope)
                if envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.prepared_agent_input.is_some()) =>
            {
                Some(envelope)
            }
            _ => None,
        })
        .expect("prepared source is canonically retained");
    let metadata = recorded.metadata.as_ref().expect("host metadata");
    assert_eq!(metadata.prepared_agent_input.as_deref(), Some(&input));
    assert_eq!(metadata.user_input_order, Some(17));
    assert!(!crate::context::is_user_authorization_message(
        &recorded.item
    ));
    let ResponseItem::AgentMessage { content, .. } = &recorded.item else {
        panic!("prepared model input must remain AgentMessage, never user Message");
    };
    assert_eq!(
        content
            .iter()
            .filter_map(|part| match part {
                AgentMessageInputContent::EncryptedContent { encrypted_content } =>
                    Some(encrypted_content.as_str()),
                AgentMessageInputContent::InputText { .. } => None,
            })
            .collect::<Vec<_>>(),
        vec!["opaque-prepared-body"]
    );
    let model_json = serde_json::to_string(&recorded.item).expect("model item");
    assert!(!model_json.contains("original-invocation"));
    if !audit.is_empty() {
        assert!(
            !model_json.contains("Readable"),
            "readable audit is not substituted for ciphertext"
        );
    }
    let presentation = history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)) => match &completed.item {
                TurnItem::AgentMessage(message)
                    if message.attribution.as_ref() == Some(input.attribution.as_ref()) =>
                {
                    Some(message)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("paired canonical presentation");
    assert!(
        presentation.input.is_none(),
        "audit must not be converted to human UserInput"
    );
    let expected = input
        .readable_content()
        .map(|text| AgentMessageContent::Text {
            text: text.to_owned(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(&presentation.content).expect("serialized presentation content"),
        serde_json::to_value(&expected).expect("serialized expected audit content")
    );
}

#[tokio::test]
async fn prepared_agent_input_stale_receiver_is_rejected_before_publication() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let before = session.clone_history().await.annotated_items().to_vec();
    let mut input = input_for(&session, "unchanged audit");
    input.recipient_instance_id = uuid::Uuid::now_v7();
    let error = session
        .record_prepared_agent_input(
            &turn,
            turn.model_info(),
            Box::new(input),
            UserInputMetadata::default(),
            PersistContext::Standard,
        )
        .await
        .expect_err("stale receiver must fail before a write");
    assert!(error.to_string().contains("previous receiver incarnation"));
    assert_eq!(
        session.clone_history().await.annotated_items(),
        before.as_slice()
    );
}

#[test_case(codex_protocol::turn_input::TurnInputMode::StartOrSteer; "idle start or steer has exact receipt")]
#[test_case(codex_protocol::turn_input::TurnInputMode::StartIfIdle; "idle only has exact receipt")]
#[tokio::test]
async fn prepared_agent_input_idle_start_binds_real_turn(
    mode: codex_protocol::turn_input::TurnInputMode,
) {
    use codex_protocol::turn_input::TurnInput;
    use codex_protocol::turn_input::TurnInputRequest;
    use codex_protocol::turn_input::TurnInputSubmission;
    let (session, _context, _events) = make_session_and_context_with_rx().await;
    let input = input_for(&session, "complete audit for an idle start");
    let submission_id = "prepared-idle-submission".to_string();
    let mut admission = session.register_input_turn_admission(submission_id.clone());
    admission.mark_submitted();
    let routing = crate::session::turn_input::handle(
        &session,
        TurnInputRequest::new(TurnInput::PreparedAgentInput(Box::new(input))),
        mode,
        submission_id,
    )
    .await
    .expect("typed idle admission");
    let TurnInputSubmission::Started {
        turn_id,
        root_turn_id,
    } = routing
    else {
        panic!("prepared idle input must establish an actual turn, not return Accepted");
    };
    let receipt =
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 5), admission.recv())
            .await
            .expect("exact admission receipt")
            .expect("receipt channel")
            .expect("accepted turn identity");
    assert_eq!(
        (receipt.target_turn_id, receipt.root_turn_id),
        (turn_id, root_turn_id)
    );
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}
