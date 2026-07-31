//! Exercises read overlap and ordered identity registration when a V2 root resumes cold.

use super::ROLE_MODEL;
use super::ROLE_NAME;
use super::body_contains;
use super::configure_multi_agent_v2_with_role;
use super::mount_root_collaboration_call;
use super::request_has_input_type;
use super::request_has_model;
use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::LoadForkSourceByRolloutPathParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PersistContext;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::StoredForkSource;
use codex_thread_store::StoredModelContext;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadPage;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreFuture;
use codex_thread_store::UpdateThreadMetadataParams;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

struct GatedChildMetadataStore {
    inner: Arc<dyn ThreadStore>,
    gates: Mutex<HashMap<ThreadId, oneshot::Receiver<()>>>,
    colliding_children: [ThreadId; 2],
    failed_child: ThreadId,
    started: mpsc::UnboundedSender<ThreadId>,
    completed: mpsc::UnboundedSender<ThreadId>,
}

macro_rules! delegate_store_methods {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> ThreadStoreFuture<'_, $result> {
            self.inner.$name($param)
        })*
    };
}

impl ThreadStore for GatedChildMetadataStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn default_history_mode(&self) -> ThreadHistoryMode {
        self.inner.default_history_mode()
    }

    delegate_store_methods! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn append_items(params: AppendThreadItemsParams) -> ();
        fn append_completion_items_and_flush(params: AppendThreadItemsParams) -> ();
        fn load_sub_agent_completion_context_item(params: codex_thread_store::LoadSubAgentCompletionContextItemParams) -> Option<codex_protocol::models::ResponseItem>;
        fn load_sub_agent_completion_presentation(params: codex_thread_store::LoadSubAgentCompletionPresentationParams) -> codex_thread_store::StoredSubAgentCompletionPresentation;
        fn flush_thread(thread_id: ThreadId) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn load_fork_source_by_rollout_path(params: LoadForkSourceByRolloutPathParams) -> StoredForkSource;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(async move {
            let child_id = params.thread_id;
            let mut history = self.inner.load_latest_model_context(params).await?;
            if self.colliding_children.contains(&child_id) {
                let mut replaced = false;
                for item in &mut history.items {
                    let RolloutItem::SessionMeta(meta) = item else {
                        continue;
                    };
                    if meta.meta.id != child_id {
                        continue;
                    }
                    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) =
                        &mut meta.meta.source
                    else {
                        panic!("the child fixture must retain its canonical spawn source");
                    };
                    *agent_path =
                        Some(AgentPath::try_from("/root/restored").expect("fixture path"));
                    meta.meta.agent_path = Some("/root/restored".to_owned());
                    replaced = true;
                }
                assert!(
                    replaced,
                    "the child model window includes its canonical metadata"
                );
            }
            Ok(history)
        })
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            let gate = self
                .gates
                .lock()
                .expect("child read gates")
                .remove(&thread_id);
            let result = if let Some(gate) = gate {
                assert!(!params.include_history, "restoration only reads metadata");
                self.started.send(thread_id).expect("read started receiver");
                gate.await.expect("release child read");

                let result = if thread_id == self.failed_child {
                    Err(ThreadStoreError::Internal {
                        message: "injected child metadata failure".to_owned(),
                    })
                } else {
                    self.inner.read_thread(params).await.map(|mut thread| {
                        // Only one child can register this path, exposing which result is applied first.
                        thread.agent_path = Some("/root/restored".to_owned());
                        thread
                    })
                };
                self.completed
                    .send(thread_id)
                    .expect("read completed receiver");
                result
            } else {
                self.inner.read_thread(params).await
            };
            result.map(|mut thread| {
                // This opaque wrapper is not a LocalThreadStore, so LiveThread does not
                // expose its inner store's host path. Metadata must advertise the same
                // pathless identity when the owning root is revalidated during child load.
                thread.rollout_path = None;
                thread
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_root_resume_overlaps_child_reads_and_applies_identities_in_graph_order() -> Result<()>
{
    let server = start_mock_server().await;
    let initial_url = format!("{}/v1", server.uri());
    let initial = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_config(move |config| {
            configure_multi_agent_v2_with_role(config, &initial_url);
            config
                .features
                .enable(Feature::Sqlite)
                .expect("enable SQLite");
            config.multi_agent_v2.max_concurrent_threads_per_session = 4;
        })
        .build_with_auto_env(&server)
        .await?;
    let root = initial.session_configured.thread_id;

    for (name, prompt, call_id, task) in [
        ("alpha", "start alpha", "spawn-alpha", "alpha child task"),
        ("beta", "start beta", "spawn-beta", "beta child task"),
        ("gamma", "start gamma", "spawn-gamma", "gamma child task"),
    ] {
        mount_root_collaboration_call(
            &server,
            prompt,
            call_id,
            "spawn_agent",
            &json!({ "message": task, "task_name": name, "agent_type": ROLE_NAME, "fork_turns": "none" })
                .to_string(),
        )
        .await;
        mount_sse_once_match(
            &server,
            move |request: &wiremock::Request| {
                request_has_model(request, ROLE_MODEL)
                    && request_has_input_type(request, "agent_message")
                    && body_contains(request, task)
            },
            sse(vec![ev_completed(&format!("{call_id}-child"))]),
        )
        .await;
        initial.submit_turn(prompt).await?;
    }

    let subtree = initial
        .thread_manager
        .list_agent_subtree_thread_ids(root)
        .await?;
    let [listed_root, first, failed, last] = subtree.as_slice() else {
        anyhow::bail!("expected a root and three durable children: {subtree:?}");
    };
    let [first, failed, last] = [*first, *failed, *last];
    assert_eq!(*listed_root, root);
    let mut ordered_children = vec![first, failed, last];
    ordered_children.sort_by_key(ToString::to_string);
    assert_eq!(ordered_children, vec![first, failed, last]);
    for child_id in [first, failed, last] {
        let child = initial.thread_manager.get_thread(child_id).await?;
        wait_for_event(child.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        child.flush_rollout().await?;
        child.shutdown_and_wait().await?;
    }
    initial.codex.flush_rollout().await?;

    let (started_tx, mut started) = mpsc::unbounded_channel();
    let (completed_tx, mut completed) = mpsc::unbounded_channel();
    let (first_release, first_gate) = oneshot::channel();
    let (failed_release, failed_gate) = oneshot::channel();
    let (last_release, last_gate) = oneshot::channel();
    let store = Arc::new(GatedChildMetadataStore {
        inner: Arc::clone(&initial.thread_store),
        colliding_children: [first, last],
        gates: Mutex::new(HashMap::from([
            (first, first_gate),
            (failed, failed_gate),
            (last, last_gate),
        ])),
        failed_child: failed,
        started: started_tx,
        completed: completed_tx,
    });
    let resume_url = format!("{}/v1", server.uri());
    let mut resume_builder = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_thread_store(store)
        .with_config(move |config| {
            configure_multi_agent_v2_with_role(config, &resume_url);
            config
                .features
                .enable(Feature::Sqlite)
                .expect("enable SQLite");
            config.multi_agent_v2.max_concurrent_threads_per_session = 4;
        });

    let (resumed, ()) = tokio::try_join!(resume_builder.restart(&server, &initial), async {
        timeout(Duration::from_secs(10), async {
            // Releasing nothing until all reads start makes a serial implementation fail.
            let mut reads = Vec::new();
            for _ in 0..3 {
                reads.push(started.recv().await.context("child read started")?);
            }
            reads.sort_by_key(ToString::to_string);
            assert_eq!(reads, ordered_children);

            last_release.send(()).expect("release last child");
            assert_eq!(completed.recv().await, Some(last));
            failed_release.send(()).expect("release failed child");
            assert_eq!(completed.recv().await, Some(failed));
            first_release.send(()).expect("release first child");
            assert_eq!(completed.recv().await, Some(first));
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("child metadata reads should overlap")?
    })?;

    assert_eq!(resumed.thread_manager.list_thread_ids().await, vec![root]);
    assert_eq!(resumed.codex.rollout_path(), None);
    // Interrupt observes registered unloaded identities without restoring them. Calling
    // ensure_child_loaded here would perform new restoration rather than inspect the
    // result of the ordered startup reads (and retry the injected read failure).
    for (child_id, prompt, call_id) in [
        (first, "inspect first restored identity", "inspect-first"),
        (last, "inspect colliding restored identity", "inspect-last"),
        (failed, "inspect failed restored identity", "inspect-failed"),
    ] {
        let result = mount_root_collaboration_call(
            &server,
            prompt,
            call_id,
            "interrupt_agent",
            &json!({ "target": child_id.to_string() }).to_string(),
        )
        .await;
        resumed.submit_turn(prompt).await?;
        let output = result
            .single_request()
            .function_call_output_text(call_id)
            .context("the registry probe returns a complete tool result")?;
        if child_id == first {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&output)?,
                json!({"previous_status": AgentStatus::NotFound})
            );
        } else {
            assert_eq!(output, format!("agent with id {child_id} not found"));
        }
        assert_eq!(resumed.thread_manager.list_thread_ids().await, vec![root]);
    }
    resumed
        .thread_manager
        .ensure_multi_agent_v2_child_loaded(first)
        .await?;
    assert!(resumed.thread_manager.get_thread(first).await.is_ok());
    assert!(resumed.thread_manager.get_thread(last).await.is_err());
    assert!(resumed.thread_manager.get_thread(failed).await.is_err());
    Ok(())
}
