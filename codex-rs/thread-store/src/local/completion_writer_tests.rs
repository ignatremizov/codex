use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::sub_agent_completion_item;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::LocalThreadStore;
use super::live_writer;
use super::test_support::test_config;
use super::tests::create_thread_params;
use crate::AppendThreadItemsParams;
use crate::ThreadStore;

#[tokio::test]
async fn completion_write_materializes_lazy_metadata_and_acknowledges_without_reappending() {
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::new();
        let mut params = create_thread_params(thread_id);
        params.history_mode = history_mode;
        store
            .create_thread(params)
            .await
            .expect("create lazy thread");
        let item = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "background".to_string(),
            item: TurnItem::AgentMessage(
                sub_agent_completion_item("/root/worker", &AgentStatus::Shutdown)
                    .expect("terminal"),
            ),
            started_at_ms: None,
            completed_at_ms: 1,
        }));
        store
            .append_completion_items_and_flush(AppendThreadItemsParams {
                thread_id,
                items: vec![item.clone()],
            })
            .await
            .expect("completion barrier");
        store
            .append_completion_items_and_flush(AppendThreadItemsParams {
                thread_id,
                items: Vec::new(),
            })
            .await
            .expect("reconciliation barrier");
        let path = live_writer::rollout_path(&store, thread_id)
            .await
            .expect("rollout path");
        store
            .shutdown_thread(thread_id)
            .await
            .expect("close writer");
        let (actual, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
            .await
            .expect("canonical read");
        assert!(matches!(actual.first(), Some(RolloutItem::SessionMeta(_))));
        assert_eq!(
            serde_json::to_value(&actual[1..]).expect("actual"),
            serde_json::to_value([item]).expect("expected"),
        );
    }
}
