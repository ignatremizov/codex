//! Native V2 completion delivery follows the real parent mailbox, not the removed Op adapter.

use super::*;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[derive(Clone, Copy)]
enum CompletionRoute {
    Watcher,
    Direct,
}

struct CompletionFixture {
    _harness: AgentControlHarness,
    root: Arc<CodexThread>,
    worker: Arc<CodexThread>,
    tester: Arc<CodexThread>,
    worker_path: AgentPath,
    tester_path: AgentPath,
}

impl CompletionFixture {
    async fn new() -> Self {
        let (home, mut config) = test_config().await;
        config.features.enable(Feature::MultiAgentV2).expect("V2");
        let mut harness = AgentControlHarness::new_with_config(home, config).await;
        let (root_id, root) = harness.start_thread().await;
        harness.control = root
            .session
            .services
            .local_agent_runtime
            .control(root.session.session_id());
        let worker_path = AgentPath::root().join("worker_a").expect("worker path");
        let tester_path = worker_path.join("tester").expect("tester path");
        let worker_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_id,
            depth: 1,
            agent_path: Some(worker_path.clone()),
            agent_nickname: None,
            agent_role: None,
        });
        let (worker_id, worker) = harness
            .start_thread_with_source(harness.config.clone(), worker_source.clone())
            .await;
        let tester_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: worker_id,
            depth: 2,
            agent_path: Some(tester_path.clone()),
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        });
        let (_, tester) = harness
            .start_thread_with_source(harness.config.clone(), tester_source.clone())
            .await;
        for (child, parent, source, path) in [
            (&worker, &root, worker_source, worker_path.clone()),
            (&tester, &worker, tester_source, tester_path.clone()),
        ] {
            harness
                .control
                .maybe_start_completion_watcher(
                    child,
                    Some(source),
                    path.to_string(),
                    Some(path),
                    MultiAgentVersion::V2,
                )
                .await;
            assert_eq!(
                harness.control.completion_parent_for_child(
                    child.session.presentation_id(),
                    parent.session.thread_id(),
                ),
                Some(parent.session.presentation_id()),
            );
        }
        root.ensure_rollout_materialized().await;
        worker.ensure_rollout_materialized().await;
        Self {
            _harness: harness,
            root,
            worker,
            tester,
            worker_path,
            tester_path,
        }
    }

    async fn assert_delivery(
        &self,
        mut event: EventMsg,
        status: AgentStatus,
        route: CompletionRoute,
    ) {
        let turn = self.tester.session.new_default_turn().await;
        self.tester.session.emit_turn_started(turn.as_ref()).await;
        let turn_id = turn.sub_id.clone();
        let guardian_interruption = matches!(&event, EventMsg::TurnAborted(event)
            if event.error.as_ref().is_some_and(|error|
                error.codex_error_info == Some(CodexErrorInfo::TooManyDenials)));
        if let EventMsg::TurnAborted(event) = &mut event {
            event.turn_id = Some(turn_id.clone());
        }
        match route {
            CompletionRoute::Watcher => {
                self.tester
                    .session
                    .send_event_raw(Event {
                        id: turn_id,
                        msg: event,
                    })
                    .await;
            }
            CompletionRoute::Direct => self.tester.session.send_event(&turn, event).await,
        }
        let ids = timeout(Duration::from_secs(/*secs*/ 5), async {
            loop {
                let ids = self
                    .worker
                    .session
                    .input_queue
                    .pending_mailbox_response_item_ids(/*turn_state*/ None)
                    .await;
                if !ids.is_empty() {
                    break ids;
                }
                sleep(Duration::from_millis(/*millis*/ 10)).await;
            }
        })
        .await
        .expect("completion reaches the direct parent's mailbox");
        assert_eq!(ids.len(), 1);
        assert!(is_sub_agent_completion_context_response_item_id(
            ids[0].as_str()
        ));
        let message = if guardian_interruption {
            let AgentStatus::Errored(error) = &status else {
                panic!("Guardian interruption has a parent-facing error");
            };
            crate::session_prefix::format_guardian_interruption_message(
                self.worker_path.clone(),
                self.tester_path.clone(),
                error,
            )
        } else {
            crate::session_prefix::format_inter_agent_completion_message(
                self.worker_path.clone(),
                self.tester_path.clone(),
                &status,
            )
            .expect("terminal status renders a message")
        };
        let mut expected = InterAgentCommunication::new(
            self.tester_path.clone(),
            self.worker_path.clone(),
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        expected.id = Some(ids[0].clone());
        let stored = self
            .worker
            .session
            .services
            .thread_store
            .load_sub_agent_completion_context_item(LoadSubAgentCompletionContextItemParams {
                thread_id: self.worker.session.thread_id(),
                include_archived: false,
                response_item_id: ids[0].clone(),
            })
            .await
            .expect("canonical evidence is present before enqueue");
        assert_eq!(stored, Some(expected.to_model_input_item()));
        assert_eq!(
            self.tester.agent_status().await,
            if guardian_interruption {
                AgentStatus::Interrupted
            } else {
                status
            }
        );
        if guardian_interruption {
            self.tester
                .session
                .send_event_raw(Event {
                    id: uuid::Uuid::now_v7().to_string(),
                    msg: EventMsg::ShutdownComplete,
                })
                .await;
            assert_eq!(
                self.worker
                    .session
                    .input_queue
                    .pending_mailbox_response_item_ids(/*turn_state*/ None)
                    .await,
                ids,
                "a safety stop remains the sole completion until Running rearms the child",
            );
        }
        assert!(
            !self
                .worker
                .session
                .input_queue
                .has_trigger_turn_mailbox_items()
                .await
        );
        assert!(
            !self
                .root
                .session
                .input_queue
                .has_pending_mailbox_items()
                .await
        );
        let root_history = self.root.session.clone_history().await;
        assert!(
            !root_history
                .raw_items()
                .any(|item| item.id() == Some(&ids[0]))
        );
        assert!(!has_subagent_notification(root_history.raw_items()));
    }
}

#[tokio::test]
async fn multi_agent_v2_shutdown_watcher_queues_message_for_direct_parent() {
    CompletionFixture::new()
        .await
        .assert_delivery(
            EventMsg::ShutdownComplete,
            AgentStatus::Shutdown,
            CompletionRoute::Watcher,
        )
        .await;
}

#[tokio::test]
async fn multi_agent_v2_raw_error_watcher_queues_message_for_direct_parent() {
    let message = "invalid thread settings";
    CompletionFixture::new()
        .await
        .assert_delivery(
            EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
            AgentStatus::Errored(message.to_string()),
            CompletionRoute::Watcher,
        )
        .await;
}

#[test_case(CompletionRoute::Watcher; "fallback watcher")]
#[test_case(CompletionRoute::Direct; "direct terminal publication")]
#[tokio::test]
async fn guardian_stop_preserves_interrupted_status_and_parent_guidance(route: CompletionRoute) {
    let message = "Action is not authorized.";
    CompletionFixture::new()
        .await
        .assert_delivery(
            EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                root_turn_id: None,
                turn_id: None,
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                error: Some(ErrorEvent {
                    misalignment: None,
                    message: message.to_string(),
                    codex_error_info: Some(CodexErrorInfo::TooManyDenials),
                }),
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
            AgentStatus::Errored(message.to_string()),
            route,
        )
        .await;
}

#[tokio::test]
async fn multi_agent_v2_completion_ignores_dead_direct_parent() {
    let fixture = CompletionFixture::new().await;
    let parent_id = fixture.worker.session.thread_id();
    let removed = fixture
        ._harness
        .manager
        .remove_thread(&parent_id)
        .await
        .expect("remove the exact direct parent");
    assert!(Arc::ptr_eq(&removed, &fixture.worker));
    assert!(
        removed
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .is_none()
    );
    let turn = fixture.tester.session.new_default_turn().await;
    fixture
        .tester
        .session
        .emit_turn_started(turn.as_ref())
        .await;
    let message = "tester result after direct parent removal";
    fixture
        .tester
        .session
        .send_event_raw(Event {
            id: turn.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    // Terminal capture synchronously rejects the sealed parent before waking the watcher.
    // Removing worker may deliver its own NotFound to root; that is not tester fallback.
    assert_eq!(
        fixture.tester.agent_status().await,
        AgentStatus::Errored(message.to_string())
    );
    assert!(
        fixture
            ._harness
            .control
            .take_watcher_terminal_presentation(fixture.tester.session.presentation_id(),)
            .is_none()
    );
    assert!(
        !fixture
            .worker
            .session
            .input_queue
            .has_pending_mailbox_items()
            .await
    );
    for parent in [&fixture.worker, &fixture.root] {
        assert!(!history_contains_text(
            parent.session.clone_history().await.raw_items(),
            message
        ));
    }
}
