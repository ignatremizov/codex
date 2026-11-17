//! Runtime integration of selected-control admission and canonical prepared input.

use super::*;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;
use crate::agent::api::UnifiedInputPolicy;
use crate::agent::api::UnifiedInputStatus;
use crate::agent::api::UnifiedModelInput;
use crate::agent::api::UnifiedModelInputRequest;
use codex_history::RolloutItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_thread_store::LoadThreadHistoryParams;
use pretty_assertions::assert_eq;
use test_case::test_case;
use tokio::sync::oneshot;

struct PreparedInputTask {
    started: Arc<tokio::sync::Notify>,
    recorded: std::sync::Mutex<Option<oneshot::Sender<Result<(), String>>>>,
}

impl crate::tasks::SessionTask for PreparedInputTask {
    fn kind(&self) -> crate::state::TaskKind {
        crate::state::TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "agent_control_test.prepared_input"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        context: Arc<crate::session::turn_context::TurnContext>,
        _initial: Vec<crate::session::TurnInput>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> crate::tasks::SessionTaskResult {
        session
            .send_event(
                &context,
                EventMsg::TurnStarted(TurnStartedEvent {
                    turn_attribution: None,
                    turn_id: context.sub_id.clone(),
                    root_turn_id: None,
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                    agent_queue: None,
                }),
            )
            .await;
        let recorded = self.recorded.lock().expect("recording channel").take();
        self.started.notify_one();
        if let Some(recorded) = recorded {
            loop {
                let (pending, _) = session
                    .input_queue
                    .get_pending_input(&session.active_turn)
                    .await;
                if !pending.is_empty() {
                    let outcome = crate::session::turn::run_hooks_and_record_inputs(
                        &session,
                        &context,
                        context.model_info(),
                        &pending,
                        codex_thread_store::PersistContext::SteeredUserInput,
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|blocked| {
                        if blocked {
                            Err("prepared input unexpectedly blocked".to_string())
                        } else {
                            Ok(())
                        }
                    });
                    let _ = recorded.send(outcome);
                    break;
                }
                tokio::select! {
                    () = cancellation_token.cancelled() => return Ok(None),
                    () = tokio::time::sleep(Duration::from_millis(/*millis*/ 10)) => {}
                }
            }
        }
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

#[derive(Clone, Copy)]
enum Payload {
    Plaintext,
    Encrypted,
    EncryptedWithAudit,
}

#[test_case(Payload::Plaintext, false; "plaintext keeps agent authority")]
#[test_case(Payload::Encrypted, false; "encrypted has no invented audit")]
#[test_case(Payload::EncryptedWithAudit, false; "encrypted audit pairs with exact receiving turn")]
#[test_case(Payload::EncryptedWithAudit, true; "encrypted queued input does not steer")]
#[tokio::test]
async fn prepared_agent_input_admission_and_publication(payload: Payload, queued: bool) {
    const SOURCE_TURN: &str = "prepared-source-turn";
    const RECEIVER_TURN: &str = "prepared-receiver-turn";
    const CIPHERTEXT: &str = "opaque-prepared-runtime-content";
    const AUDIT: &str = "Readable original agent audit, not a human instruction.";
    let (home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("native V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let mut threads = Vec::new();
    for task in ["prepared-sender", "prepared-receiver"] {
        let child = root
            .spawn_agent(UserAgentSpawnOptions {
                task: Some(task.to_string()),
                response_handling: UserAgentResponseHandling::Presentation,
                ..Default::default()
            })
            .await
            .expect("spawn native V2 child");
        threads.push(
            harness
                .manager
                .get_thread(child.target_thread_id)
                .await
                .expect("child"),
        );
    }
    let sender = &threads[0];
    let receiver = &threads[1];
    let (recorded, mut recorded_result) = oneshot::channel();
    let mut recorded = Some(recorded);
    for (thread, name) in [(sender, SOURCE_TURN), (receiver, RECEIVER_TURN)] {
        let started = Arc::new(tokio::sync::Notify::new());
        let context = thread
            .session
            .new_turn_with_default_settings(name.to_string(), Default::default())
            .await;
        assert_eq!(context.multi_agent_version, MultiAgentVersion::V2);
        thread
            .session
            .spawn_task(
                context,
                Vec::new(),
                PreparedInputTask {
                    started: Arc::clone(&started),
                    recorded: std::sync::Mutex::new(if name == RECEIVER_TURN {
                        recorded.take()
                    } else {
                        None
                    }),
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), started.notified())
            .await
            .expect("live turn");
        assert_eq!(
            thread.session.active_agent_response_turn_id().as_deref(),
            Some(name)
        );
    }
    let message = match payload {
        Payload::Plaintext => AgentMessage::Plaintext(AUDIT.to_string()),
        Payload::Encrypted => AgentMessage::Encrypted(CIPHERTEXT.to_string()),
        Payload::EncryptedWithAudit => AgentMessage::EncryptedWithAudit {
            encrypted_content: CIPHERTEXT.to_string(),
            audit_content: AUDIT.to_string(),
        },
    };
    let policy = if queued {
        UnifiedInputPolicy::Queue {
            commentary: false,
            final_delivery: AgentResponseFinalDelivery::None,
        }
    } else {
        UnifiedInputPolicy::Response {
            commentary: false,
            final_delivery: AgentResponseFinalDelivery::None,
        }
    };
    let control = sender.session.services.agent_control.as_ref();
    assert_eq!(control.identity(), sender.session.session_id());
    let receipt = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 10),
        control.submit_unified_input(UnifiedModelInputRequest {
            caller: sender.session.thread_id(),
            source_instance_id: sender.session.presentation_id().instance_id,
            sender_turn_id: SOURCE_TURN.to_string(),
            call_id: "prepared-runtime-call".to_string(),
            batch_id: Some("prepared-runtime-batch".to_string()),
            target: AgentTarget::Id(receiver.session.thread_id()),
            resume_config: harness.config.clone(),
            input: UnifiedModelInput::Prepared(message),
            policy,
            interrupt_descendant: false,
            start_options: TurnStartOptions {
                parent_turn_id: Some(SOURCE_TURN.to_string()),
                ..Default::default()
            },
        }),
    )
    .await
    .expect("native prepared routing resolved")
    .expect("native prepared admission");
    assert_eq!(
        receipt.status,
        if queued {
            UnifiedInputStatus::Queued
        } else {
            UnifiedInputStatus::Submitted
        }
    );
    assert_eq!(
        receipt.target_turn_id.as_deref(),
        if queued { None } else { Some(RECEIVER_TURN) }
    );
    assert!(receipt.warning.is_none());
    if queued {
        assert!(matches!(
            recorded_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            harness
                .control
                .runtime
                .upgrade()
                .expect("manager")
                .agent_turn_queue
                .has_pending(receiver.session.thread_id())
        );
    } else {
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), recorded_result)
            .await
            .expect("publication completed")
            .expect("recording channel")
            .expect("canonical recording");
        receiver.flush_rollout().await.expect("flush receiver");
        // Native V2 uses paginated history. Inspect the persisted canonical
        // segments rather than the legacy full-history API, which rejects it.
        let history = receiver
            .session
            .services
            .thread_store
            .load_canonical_artifact_segments(LoadThreadHistoryParams {
                thread_id: receiver.session.thread_id(),
                include_archived: false,
            })
            .await
            .expect("receiver canonical history");
        let envelope = history
            .segments
            .iter()
            .flatten()
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
            .expect("prepared input persisted by real record-input pipeline");
        let source = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.prepared_agent_input.as_deref())
            .expect("complete source capture");
        assert_eq!(source.call_id, "prepared-runtime-call");
        assert_eq!(
            source.attribution.sender.thread_id,
            sender.session.thread_id()
        );
        assert_eq!(
            source.attribution.recipient.thread_id,
            receiver.session.thread_id()
        );
        assert_eq!(source.attribution.sender_turn_id, SOURCE_TURN);
        assert!(!crate::context::is_user_authorization_message(
            &envelope.item
        ));
        let ResponseItem::AgentMessage { content, .. } = &envelope.item else {
            panic!("never human UserInput");
        };
        let encrypted = content.iter().find_map(|part| match part {
            AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                Some(encrypted_content.as_str())
            }
            AgentMessageInputContent::InputText { .. } => None,
        });
        assert_eq!(
            encrypted,
            match payload {
                Payload::Plaintext => None,
                Payload::Encrypted | Payload::EncryptedWithAudit => Some(CIPHERTEXT),
            }
        );
        match payload {
            Payload::Plaintext => assert!(source.communication.content.contains(AUDIT)),
            Payload::Encrypted => assert_eq!(source.readable_content(), None),
            Payload::EncryptedWithAudit => assert_eq!(source.readable_content(), Some(AUDIT)),
        }
    }
    // Closing the source cancels its process-local queue before the receiver stops.
    sender.shutdown_and_wait().await.expect("shutdown sender");
    receiver
        .shutdown_and_wait()
        .await
        .expect("shutdown receiver");
    root.shutdown_and_wait().await.expect("shutdown root");
}
