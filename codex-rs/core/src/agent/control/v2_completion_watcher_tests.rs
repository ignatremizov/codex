//! Native V2 completion delivery follows the real parent mailbox, not the removed Op adapter.

use super::*;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::LoadSubAgentCompletionContextItemParams;
use pretty_assertions::assert_eq;

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
        harness.control = root.session.services.agent_control.clone();
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
                .bind_completion_watcher_with_parent(
                    child,
                    parent,
                    source,
                    path.to_string(),
                    Some(path),
                    MultiAgentVersion::V2,
                )
                .expect("bind the exact native parent before any turn starts");
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

    async fn assert_delivery(&self, event: EventMsg, status: AgentStatus) {
        let turn_id = start_completion_test_turn(&self.tester).await;
        self.tester
            .session
            .send_event_raw(Event {
                id: turn_id,
                msg: event,
            })
            .await;
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
        assert!(is_sub_agent_completion_context_response_item_id(&ids[0]));
        let message = crate::session_prefix::format_inter_agent_completion_message(
            self.worker_path.clone(),
            self.tester_path.clone(),
            &status,
        )
        .expect("terminal status renders a message");
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
        assert_eq!(self.tester.agent_status().await, status);
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
        .assert_delivery(EventMsg::ShutdownComplete, AgentStatus::Shutdown)
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
        )
        .await;
}
