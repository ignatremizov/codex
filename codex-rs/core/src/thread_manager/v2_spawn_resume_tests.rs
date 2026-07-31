use super::*;
use codex_agent_graph_store::AgentGraphStoreFuture;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;

struct EmptyAgentGraphStore;

impl AgentGraphStore for EmptyAgentGraphStore {
    fn upsert_thread_spawn_edge(
        &self,
        _parent_thread_id: ThreadId,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn set_thread_spawn_edge_status(
        &self,
        _child_thread_id: ThreadId,
        _status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn list_thread_spawn_children(
        &self,
        _parent_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn list_thread_spawn_descendants(
        &self,
        _root_thread_id: ThreadId,
        _status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn resumed_v2_history(
    child_thread_id: ThreadId,
    session_id: SessionId,
    source: SessionSource,
) -> InitialHistory {
    InitialHistory::Resumed(ResumedHistory {
        conversation_id: child_thread_id,
        history: Arc::new(vec![RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id,
                id: child_thread_id,
                source,
                multi_agent_version: Some(MultiAgentVersion::V2),
                ..SessionMeta::default()
            },
            git: None,
        })]),
        rollout_path: None,
    })
}

fn assert_invalid_request(
    result: CodexResult<Option<PersistedV2SpawnResume>>,
    expected_message: &str,
) {
    match result {
        Err(err) => match err.details() {
            CodexErrorDetails::InvalidRequest(message) => assert!(
                message.contains(expected_message),
                "unexpected invalid-request message: {message}"
            ),
            _ => panic!("expected invalid request, got {err}"),
        },
        Ok(_) => panic!("expected spawned V2 history to reject detached fallback"),
    }
}

#[tokio::test]
async fn spawned_v2_history_without_graph_rejects_detached_fallback() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let history = resumed_v2_history(
        child_thread_id,
        parent_thread_id.into(),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&history, None).await,
        "persisted agent graph is unavailable",
    );
}

#[tokio::test]
async fn spawned_v2_history_uses_latest_exact_session_metadata() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let session_id = SessionId::from(parent_thread_id);
    let history = resumed_v2_history(
        child_thread_id,
        session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let InitialHistory::Resumed(mut resumed) = history else {
        panic!("test history should be resumed");
    };
    let mut items = resumed.history.as_ref().clone();
    items.insert(
        0,
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id,
                id: child_thread_id,
                source: SessionSource::default(),
                multi_agent_version: Some(MultiAgentVersion::V2),
                ..SessionMeta::default()
            },
            git: None,
        }),
    );
    resumed.history = Arc::new(items);
    let latest_sources = InitialHistory::Resumed(resumed.clone()).get_resumed_session_sources();

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&InitialHistory::Resumed(resumed), None).await,
        "persisted agent graph is unavailable",
    );
    assert_eq!(
        latest_sources,
        Some((
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            None,
        ))
    );
}

#[tokio::test]
async fn latest_non_subagent_metadata_keeps_generic_resume_identity() {
    let stale_parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let session_id = SessionId::from(stale_parent_thread_id);
    let history = resumed_v2_history(
        child_thread_id,
        session_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: stale_parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let InitialHistory::Resumed(mut resumed) = history else {
        panic!("test history should be resumed");
    };
    let mut items = resumed.history.as_ref().clone();
    items.push(RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id,
            id: child_thread_id,
            source: SessionSource::Exec,
            multi_agent_version: Some(MultiAgentVersion::V2),
            ..SessionMeta::default()
        },
        git: None,
    }));
    resumed.history = Arc::new(items);
    let history = InitialHistory::Resumed(resumed);

    let result = resolve_persisted_v2_spawn_resume(&history, None)
        .await
        .expect("latest arbitrary metadata should remain eligible for generic resume");

    assert!(result.is_none());
    assert_eq!(
        history.get_resumed_session_sources(),
        Some((SessionSource::Exec, None))
    );
}

#[tokio::test]
async fn spawned_v2_history_without_edge_rejects_detached_fallback() {
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let history = resumed_v2_history(
        child_thread_id,
        parent_thread_id.into(),
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
    );
    let agent_graph_store: Arc<dyn AgentGraphStore> = Arc::new(EmptyAgentGraphStore);

    assert_invalid_request(
        resolve_persisted_v2_spawn_resume(&history, Some(agent_graph_store)).await,
        "persisted thread-spawn edge",
    );
}

#[tokio::test]
async fn arbitrary_v2_history_without_graph_keeps_generic_resume_semantics() {
    let thread_id = ThreadId::new();
    let history = resumed_v2_history(thread_id, thread_id.into(), SessionSource::default());

    let result = resolve_persisted_v2_spawn_resume(&history, None)
        .await
        .expect("arbitrary V2 history should remain eligible for generic resume");

    assert!(result.is_none());
}
