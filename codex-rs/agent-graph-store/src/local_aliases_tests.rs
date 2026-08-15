use std::sync::Arc;

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::AgentGraphStore;
use crate::ThreadSpawnEdgeStatus;

struct TestRuntime {
    state_db: Arc<StateRuntime>,
    _codex_home: TempDir,
}

fn thread_id(suffix: u128) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012}"))
        .expect("valid thread id")
}

async fn state_runtime() -> TestRuntime {
    let codex_home = TempDir::new().expect("tempdir should be created");
    let state_db = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize");
    TestRuntime {
        state_db,
        _codex_home: codex_home,
    }
}

#[tokio::test]
async fn local_alias_store_allocates_stable_refs_and_retains_closed_aliases() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 100);
    let session_id = SessionId::from(root_thread_id);
    let first_child_thread_id = thread_id(/*suffix*/ 101);
    let second_child_thread_id = thread_id(/*suffix*/ 102);

    let root_alias = store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");
    let first_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id: first_child_thread_id,
            nickname: Some("Parfit".to_string()),
        })
        .await
        .expect("first child alias should allocate");
    let repeated_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id: first_child_thread_id,
            nickname: Some("Ignored replacement".to_string()),
        })
        .await
        .expect("repeated allocation should be idempotent");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id: thread_id(/*suffix*/ 103),
            nickname: Some("Parfit".to_string()),
        })
        .await
        .expect_err("reserved nickname should reject a different child");
    let second_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: first_child_thread_id,
            child_thread_id: second_child_thread_id,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("second child alias should allocate");

    assert_eq!(
        root_alias,
        AgentAlias {
            session_id,
            thread_id: root_thread_id,
            agent_ref: 1,
            nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
            state: AgentAliasState::Active,
        }
    );
    assert_eq!(first_alias.agent_ref, 2);
    assert_eq!(repeated_alias, first_alias);
    store
        .set_agent_lifecycle_state(session_id, root_thread_id, ThreadSpawnEdgeStatus::Closed)
        .await
        .expect_err("Main's root alias must remain active");
    // The failed nickname collision rolls back its ref reservation.
    assert_eq!(second_alias.agent_ref, 3);
    assert!(
        store
            .set_agent_lifecycle_state(
                session_id,
                first_child_thread_id,
                ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .expect("agent lifecycle should update")
    );

    let closed_alias = store
        .find_agent_alias_by_nickname(session_id, "Parfit")
        .await
        .expect("nickname lookup should succeed")
        .expect("closed nickname should remain reserved");
    assert_eq!(
        closed_alias,
        AgentAlias {
            state: AgentAliasState::Closed,
            ..first_alias
        }
    );
    assert_eq!(
        store
            .list_agent_aliases(session_id)
            .await
            .expect("aliases should list")
            .into_iter()
            .map(|alias| alias.agent_ref)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(root_thread_id, Some(ThreadSpawnEdgeStatus::Closed),)
            .await
            .expect("closed root children should list"),
        vec![first_child_thread_id]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(first_child_thread_id, /*status_filter*/ None)
            .await
            .expect("nested children should list"),
        vec![second_child_thread_id]
    );
}

#[tokio::test]
async fn main_nickname_is_case_insensitive_and_reserved_from_children() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 110);
    let session_id = SessionId::from(root_thread_id);
    let root_alias = store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");

    for nickname in ["main", "Main", "MAIN"] {
        assert_eq!(
            store
                .find_agent_alias_by_nickname(session_id, nickname)
                .await
                .expect("Main nickname lookup should succeed"),
            Some(root_alias.clone())
        );
    }
    for nickname in ["main", "MAIN"] {
        store
            .allocate_agent_alias(AllocateAgentAliasRequest {
                session_id,
                parent_thread_id: root_thread_id,
                child_thread_id: ThreadId::new(),
                nickname: Some(nickname.to_string()),
            })
            .await
            .expect_err("Main nickname variants should remain reserved for the root");
    }
}

#[tokio::test]
async fn concurrent_alias_allocations_are_unique_and_monotonic() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 200);
    let session_id = SessionId::from(root_thread_id);
    store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");

    let first = store.allocate_agent_alias(AllocateAgentAliasRequest {
        session_id,
        parent_thread_id: root_thread_id,
        child_thread_id: thread_id(/*suffix*/ 201),
        nickname: Some("Curie".to_string()),
    });
    let second = store.allocate_agent_alias(AllocateAgentAliasRequest {
        session_id,
        parent_thread_id: root_thread_id,
        child_thread_id: thread_id(/*suffix*/ 202),
        nickname: Some("Franklin".to_string()),
    });
    let (first, second) = tokio::join!(first, second);
    let mut refs = vec![
        first.expect("first alias should allocate").agent_ref,
        second.expect("second alias should allocate").agent_ref,
    ];
    refs.sort_unstable();

    assert_eq!(refs, vec![2, 3]);
}

#[tokio::test]
async fn concurrent_nickname_collision_is_retryable_without_consuming_a_ref() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 210);
    let session_id = SessionId::from(root_thread_id);
    let first_child_thread_id = thread_id(/*suffix*/ 211);
    let second_child_thread_id = thread_id(/*suffix*/ 212);
    store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");

    let first = store.allocate_agent_alias(AllocateAgentAliasRequest {
        session_id,
        parent_thread_id: root_thread_id,
        child_thread_id: first_child_thread_id,
        nickname: Some("Curie".to_string()),
    });
    let second = store.allocate_agent_alias(AllocateAgentAliasRequest {
        session_id,
        parent_thread_id: root_thread_id,
        child_thread_id: second_child_thread_id,
        nickname: Some("Curie".to_string()),
    });
    let (first, second) = tokio::join!(first, second);
    let (winner, loser_thread_id, error) = match (first, second) {
        (Ok(winner), Err(error)) => (winner, second_child_thread_id, error),
        (Err(error), Ok(winner)) => (winner, first_child_thread_id, error),
        (first, second) => panic!(
            "exactly one colliding nickname allocation should succeed: first={first:?}, second={second:?}"
        ),
    };
    assert_eq!(winner.agent_ref, 2);
    assert!(
        error
            .to_string()
            .contains("retry the spawn to choose another nickname")
    );

    let retried = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id: loser_thread_id,
            nickname: Some("Franklin".to_string()),
        })
        .await
        .expect("losing allocation should retry with another nickname");
    assert_eq!(retried.agent_ref, 3);
}

#[tokio::test]
async fn activating_closed_alias_preserves_identity_and_reopens_edge() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 220);
    let session_id = SessionId::from(root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 221);
    store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");
    let original = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: Some("Curie".to_string()),
        })
        .await
        .expect("child alias should allocate");
    assert!(
        store
            .set_agent_lifecycle_state(session_id, child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .expect("child alias should close")
    );

    let activated = store
        .activate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: Some("Ignored replacement".to_string()),
        })
        .await
        .expect("child alias should reactivate");
    assert_eq!(
        activated,
        AgentAlias {
            state: AgentAliasState::Active,
            ..original
        }
    );
    assert_eq!(
        store
            .list_thread_spawn_children(root_thread_id, Some(ThreadSpawnEdgeStatus::Open),)
            .await
            .expect("reopened edge should list"),
        vec![child_thread_id]
    );
}

#[tokio::test]
async fn activating_alias_fills_previously_missing_nickname() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 230);
    let session_id = SessionId::from(root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 231);
    store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("root alias should initialize");
    let original = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: None,
        })
        .await
        .expect("unnamed child alias should allocate");
    assert!(
        store
            .set_agent_lifecycle_state(session_id, child_thread_id, ThreadSpawnEdgeStatus::Closed)
            .await
            .expect("child alias should close")
    );

    let activated = store
        .activate_agent_alias(AllocateAgentAliasRequest {
            session_id,
            parent_thread_id: root_thread_id,
            child_thread_id,
            nickname: Some("Curie".to_string()),
        })
        .await
        .expect("child alias should reactivate with its restored nickname");

    assert_eq!(
        activated,
        AgentAlias {
            nickname: Some("Curie".to_string()),
            state: AgentAliasState::Active,
            ..original
        }
    );
}

#[tokio::test]
async fn namespace_initialization_backfills_graph_in_stable_order() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 250);
    let session_id = SessionId::from(root_thread_id);
    let later_child_thread_id = thread_id(/*suffix*/ 252);
    let earlier_child_thread_id = thread_id(/*suffix*/ 251);
    let grandchild_thread_id = thread_id(/*suffix*/ 253);
    store
        .upsert_thread_spawn_edge(
            root_thread_id,
            later_child_thread_id,
            ThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("later child edge should persist");
    store
        .upsert_thread_spawn_edge(
            root_thread_id,
            earlier_child_thread_id,
            ThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("earlier child edge should persist");
    store
        .upsert_thread_spawn_edge(
            later_child_thread_id,
            grandchild_thread_id,
            ThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("grandchild edge should persist");

    store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect("namespace should initialize and backfill");
    assert_eq!(
        store
            .list_agent_aliases(session_id)
            .await
            .expect("backfilled aliases should list"),
        vec![
            AgentAlias {
                session_id,
                thread_id: root_thread_id,
                agent_ref: 1,
                nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                state: AgentAliasState::Active,
            },
            AgentAlias {
                session_id,
                thread_id: earlier_child_thread_id,
                agent_ref: 2,
                nickname: None,
                state: AgentAliasState::Closed,
            },
            AgentAlias {
                session_id,
                thread_id: later_child_thread_id,
                agent_ref: 3,
                nickname: None,
                state: AgentAliasState::Active,
            },
            AgentAlias {
                session_id,
                thread_id: grandchild_thread_id,
                agent_ref: 4,
                nickname: None,
                state: AgentAliasState::Active,
            },
        ]
    );
}

#[tokio::test]
async fn namespace_initialization_rejects_a_cyclic_spawn_graph() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 270);
    let session_id = SessionId::from(root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 271);
    store
        .upsert_thread_spawn_edge(root_thread_id, child_thread_id, ThreadSpawnEdgeStatus::Open)
        .await
        .expect("child edge should persist");
    store
        .upsert_thread_spawn_edge(child_thread_id, root_thread_id, ThreadSpawnEdgeStatus::Open)
        .await
        .expect("test cycle should persist");

    let error = store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect_err("cyclic graph should reject alias backfill");
    assert!(
        error
            .to_string()
            .contains("persisted spawn graph is cyclic")
    );
}

#[tokio::test]
async fn namespace_initialization_rejects_a_self_parented_main_edge() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root_thread_id = thread_id(/*suffix*/ 272);
    let session_id = SessionId::from(root_thread_id);
    store
        .upsert_thread_spawn_edge(root_thread_id, root_thread_id, ThreadSpawnEdgeStatus::Open)
        .await
        .expect("corrupt self-parent edge should persist for migration coverage");

    let error = store
        .ensure_agent_alias_namespace(session_id)
        .await
        .expect_err("self-parented Main should reject alias backfill");
    assert!(
        error
            .to_string()
            .contains("persisted spawn graph is cyclic")
    );
}

#[tokio::test]
async fn one_thread_cannot_be_current_in_two_roots() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 300);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 301);
    let second_session_id = SessionId::from(second_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 302);
    store
        .ensure_agent_alias_namespace(first_session_id)
        .await
        .expect("first root alias should initialize");
    store
        .ensure_agent_alias_namespace(second_session_id)
        .await
        .expect("second root alias should initialize");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("first root should allocate child");

    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: second_session_id,
            parent_thread_id: second_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect_err("second current owner should be rejected");
    assert_eq!(
        store
            .find_agent_alias_by_thread(first_session_id, child_thread_id)
            .await
            .expect("old alias lookup should succeed")
            .expect("old alias should remain"),
        AgentAlias {
            session_id: first_session_id,
            thread_id: child_thread_id,
            agent_ref: 2,
            nickname: Some("Hopper".to_string()),
            state: AgentAliasState::Active,
        }
    );
}

#[tokio::test]
async fn concurrent_alias_transfers_commit_one_exclusive_owner() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 400);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 401);
    let second_session_id = SessionId::from(second_root_thread_id);
    let third_root_thread_id = thread_id(/*suffix*/ 402);
    let third_session_id = SessionId::from(third_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 403);
    for session_id in [first_session_id, second_session_id, third_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("initial owner should allocate child");

    let second_transfer = store.transfer_agent_alias(TransferAgentAliasRequest {
        expected_previous_session_id: Some(first_session_id),
        expected_descendant_thread_ids: Vec::new(),
        new_session_id: second_session_id,
        new_parent_thread_id: second_root_thread_id,
        thread_id: child_thread_id,
        nickname: Some("Hopper".to_string()),
        authored_selector: child_thread_id.to_string(),
    });
    let third_transfer = store.transfer_agent_alias(TransferAgentAliasRequest {
        expected_previous_session_id: Some(first_session_id),
        expected_descendant_thread_ids: Vec::new(),
        new_session_id: third_session_id,
        new_parent_thread_id: third_root_thread_id,
        thread_id: child_thread_id,
        nickname: Some("Hopper".to_string()),
        authored_selector: child_thread_id.to_string(),
    });
    let (second_result, third_result) = tokio::join!(second_transfer, third_transfer);
    let (winner_session_id, winner_parent_thread_id) = match (second_result, third_result) {
        (Ok(AgentAliasTransfer::Transferred { alias, .. }), Err(_)) => {
            (alias.session_id, second_root_thread_id)
        }
        (Err(_), Ok(AgentAliasTransfer::Transferred { alias, .. })) => {
            (alias.session_id, third_root_thread_id)
        }
        outcomes => panic!("expected one transfer winner and one conflict, got {outcomes:?}"),
    };

    assert_eq!(
        store
            .find_agent_alias_by_thread(first_session_id, child_thread_id)
            .await
            .expect("old alias lookup should succeed")
            .expect("old alias should remain reserved")
            .state,
        AgentAliasState::Transferred
    );
    assert_eq!(
        store
            .find_agent_alias_by_thread(winner_session_id, child_thread_id)
            .await
            .expect("winner alias lookup should succeed")
            .expect("winner alias should exist")
            .state,
        AgentAliasState::Active
    );
    assert_eq!(
        store
            .list_thread_spawn_children(winner_parent_thread_id, /*status_filter*/ None)
            .await
            .expect("winner edge should list"),
        vec![child_thread_id]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(first_root_thread_id, /*status_filter*/ None)
            .await
            .expect("old owner edges should list"),
        Vec::<ThreadId>::new()
    );
}

#[tokio::test]
async fn repeated_transfer_requires_the_current_owner_as_its_expected_owner() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 450);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 451);
    let second_session_id = SessionId::from(second_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 452);
    for session_id in [first_session_id, second_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("initial owner should allocate child");
    let request = TransferAgentAliasRequest {
        expected_previous_session_id: Some(first_session_id),
        expected_descendant_thread_ids: Vec::new(),
        new_session_id: second_session_id,
        new_parent_thread_id: second_root_thread_id,
        thread_id: child_thread_id,
        nickname: Some("Hopper".to_string()),
        authored_selector: child_thread_id.to_string(),
    };
    assert!(matches!(
        store
            .transfer_agent_alias(request.clone())
            .await
            .expect("first transfer should succeed"),
        AgentAliasTransfer::Transferred { .. }
    ));

    let stale_error = store
        .transfer_agent_alias(request)
        .await
        .expect_err("a stale expected owner must not become an idempotent success");
    assert!(
        stale_error.to_string().contains(&format!(
            "expected {first_session_id}, found {second_session_id}"
        )),
        "unexpected stale-owner error: {stale_error:#}"
    );
    assert!(matches!(
        store
            .transfer_agent_alias(TransferAgentAliasRequest {
                expected_previous_session_id: Some(second_session_id),
                expected_descendant_thread_ids: Vec::new(),
                new_session_id: second_session_id,
                new_parent_thread_id: second_root_thread_id,
                thread_id: child_thread_id,
                nickname: Some("Hopper".to_string()),
                authored_selector: child_thread_id.to_string(),
            })
            .await
            .expect("current-owner retry should be idempotent"),
        AgentAliasTransfer::AlreadyOwned { .. }
    ));
}

#[tokio::test]
async fn alias_transfer_moves_the_complete_persisted_subtree() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 450);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 451);
    let second_session_id = SessionId::from(second_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 452);
    let grandchild_thread_id = thread_id(/*suffix*/ 453);
    for session_id in [first_session_id, second_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("child alias should allocate");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: child_thread_id,
            child_thread_id: grandchild_thread_id,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("grandchild alias should allocate");

    let stale_snapshot = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(first_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: second_session_id,
            new_parent_thread_id: second_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect_err("a stale reserved subtree snapshot should reject transfer");
    assert!(
        stale_snapshot
            .to_string()
            .contains("subtree changed while rollout writers were being reserved")
    );

    let transferred = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(first_session_id),
            expected_descendant_thread_ids: vec![grandchild_thread_id],
            new_session_id: second_session_id,
            new_parent_thread_id: second_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("subtree transfer should succeed");
    let AgentAliasTransfer::Transferred { alias, .. } = transferred else {
        panic!("subtree transfer should change ownership");
    };
    assert_eq!(
        alias,
        AgentAlias {
            session_id: second_session_id,
            thread_id: child_thread_id,
            agent_ref: 2,
            nickname: Some("Hopper".to_string()),
            state: AgentAliasState::Active,
        }
    );
    assert_eq!(
        store
            .list_agent_aliases(first_session_id)
            .await
            .expect("source aliases should list")
            .into_iter()
            .map(|alias| (alias.thread_id, alias.state))
            .collect::<Vec<_>>(),
        vec![
            (first_root_thread_id, AgentAliasState::Active),
            (child_thread_id, AgentAliasState::Transferred),
            (grandchild_thread_id, AgentAliasState::Transferred),
        ]
    );
    assert_eq!(
        store
            .list_agent_aliases(second_session_id)
            .await
            .expect("destination aliases should list"),
        vec![
            AgentAlias {
                session_id: second_session_id,
                thread_id: second_root_thread_id,
                agent_ref: 1,
                nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
                state: AgentAliasState::Active,
            },
            alias,
            AgentAlias {
                session_id: second_session_id,
                thread_id: grandchild_thread_id,
                agent_ref: 3,
                nickname: Some("Noether".to_string()),
                state: AgentAliasState::Active,
            },
        ]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(second_root_thread_id, /*status_filter*/ None)
            .await
            .expect("destination child edge should list"),
        vec![child_thread_id]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(child_thread_id, /*status_filter*/ None)
            .await
            .expect("descendant edge should remain attached"),
        vec![grandchild_thread_id]
    );
    assert_eq!(
        store
            .list_thread_spawn_children(first_root_thread_id, /*status_filter*/ None)
            .await
            .expect("source edge should be detached"),
        Vec::<ThreadId>::new()
    );

    store
        .ensure_agent_alias_namespace(second_session_id)
        .await
        .expect("destination backfill should remain conflict-free");
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(grandchild_thread_id)
            .await
            .expect("grandchild owner lookup should succeed")
            .map(|alias| alias.session_id),
        Some(second_session_id)
    );
}

#[tokio::test]
async fn alias_transfer_omits_unavailable_descendant_nickname() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 460);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 461);
    let second_session_id = SessionId::from(second_root_thread_id);
    let third_root_thread_id = thread_id(/*suffix*/ 465);
    let third_session_id = SessionId::from(third_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 462);
    let grandchild_thread_id = thread_id(/*suffix*/ 463);
    let destination_child_thread_id = thread_id(/*suffix*/ 464);
    for session_id in [first_session_id, second_session_id, third_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("source child alias should allocate");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: child_thread_id,
            child_thread_id: grandchild_thread_id,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("source grandchild alias should allocate");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: second_session_id,
            parent_thread_id: second_root_thread_id,
            child_thread_id: destination_child_thread_id,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("destination nickname owner should allocate");

    store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(first_session_id),
            expected_descendant_thread_ids: vec![grandchild_thread_id],
            new_session_id: second_session_id,
            new_parent_thread_id: second_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("a descendant nickname collision should not block subtree transfer");

    assert_eq!(
        store
            .find_agent_alias_by_thread(second_session_id, grandchild_thread_id)
            .await
            .expect("transferred grandchild alias lookup should succeed")
            .expect("transferred grandchild alias should exist")
            .nickname,
        None
    );
    assert_eq!(
        store
            .find_agent_alias_by_nickname(second_session_id, "Noether")
            .await
            .expect("destination nickname lookup should succeed")
            .map(|alias| alias.thread_id),
        Some(destination_child_thread_id)
    );

    store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(second_session_id),
            expected_descendant_thread_ids: vec![grandchild_thread_id],
            new_session_id: third_session_id,
            new_parent_thread_id: third_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("subtree should transfer to a third root");
    store
        .activate_agent_alias(AllocateAgentAliasRequest {
            session_id: third_session_id,
            parent_thread_id: child_thread_id,
            child_thread_id: grandchild_thread_id,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("the third root should restore the descendant nickname");
    store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(third_session_id),
            expected_descendant_thread_ids: vec![grandchild_thread_id],
            new_session_id: second_session_id,
            new_parent_thread_id: second_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("returning to a root must preserve its earlier omitted nickname");

    assert_eq!(
        (
            store
                .find_agent_alias_by_thread(second_session_id, grandchild_thread_id)
                .await
                .expect("returned grandchild alias lookup should succeed")
                .expect("returned grandchild alias should exist")
                .nickname,
            store
                .find_current_agent_alias_by_thread(grandchild_thread_id)
                .await
                .expect("returned grandchild owner lookup should succeed")
                .map(|alias| alias.session_id),
        ),
        (None, Some(second_session_id))
    );
}

#[tokio::test]
async fn alias_transfer_rejects_an_unavailable_target_nickname_before_ownership_changes() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source_root_thread_id = thread_id(/*suffix*/ 470);
    let source_session_id = SessionId::from(source_root_thread_id);
    let destination_root_thread_id = thread_id(/*suffix*/ 471);
    let destination_session_id = SessionId::from(destination_root_thread_id);
    let source_child_thread_id = thread_id(/*suffix*/ 472);
    let destination_child_thread_id = thread_id(/*suffix*/ 473);
    for session_id in [source_session_id, destination_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root_thread_id,
            child_thread_id: source_child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("source child alias should allocate");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: destination_session_id,
            parent_thread_id: destination_root_thread_id,
            child_thread_id: destination_child_thread_id,
            nickname: Some("Curie".to_string()),
        })
        .await
        .expect("destination child alias should allocate");

    let error = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination_session_id,
            new_parent_thread_id: destination_root_thread_id,
            thread_id: source_child_thread_id,
            nickname: Some("Curie".to_string()),
            authored_selector: source_child_thread_id.to_string(),
        })
        .await
        .expect_err("target nickname collision should reject the transfer");
    assert!(
        error
            .to_string()
            .contains("retry adoption to choose another nickname")
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(source_child_thread_id)
            .await
            .expect("source owner should remain queryable")
            .map(|alias| alias.session_id),
        Some(source_session_id)
    );
}

#[tokio::test]
async fn transferring_back_to_a_prior_root_reactivates_its_reserved_alias() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let first_root_thread_id = thread_id(/*suffix*/ 500);
    let first_session_id = SessionId::from(first_root_thread_id);
    let second_root_thread_id = thread_id(/*suffix*/ 501);
    let second_session_id = SessionId::from(second_root_thread_id);
    let child_thread_id = thread_id(/*suffix*/ 502);
    for session_id in [first_session_id, second_session_id] {
        store
            .ensure_agent_alias_namespace(session_id)
            .await
            .expect("root alias should initialize");
    }
    let original = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: first_session_id,
            parent_thread_id: first_root_thread_id,
            child_thread_id,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("initial alias should allocate");
    let transferred = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(first_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: second_session_id,
            new_parent_thread_id: second_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Hopper".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("first transfer should succeed");
    let AgentAliasTransfer::Transferred {
        alias: second_alias,
        ..
    } = transferred
    else {
        panic!("first transfer should change ownership");
    };

    let mismatched_return = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(second_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: first_session_id,
            new_parent_thread_id: first_root_thread_id,
            thread_id: child_thread_id,
            nickname: Some("Different nickname".to_string()),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect_err("return transfer must use its reserved destination identity");
    assert!(
        mismatched_return
            .to_string()
            .contains("retry adoption using that durable identity")
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(child_thread_id)
            .await
            .expect("owner lookup after rejected return should succeed")
            .map(|alias| alias.session_id),
        Some(second_session_id)
    );

    let transferred_back = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(second_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: first_session_id,
            new_parent_thread_id: first_root_thread_id,
            thread_id: child_thread_id,
            nickname: original.nickname.clone(),
            authored_selector: child_thread_id.to_string(),
        })
        .await
        .expect("return transfer should succeed");
    let AgentAliasTransfer::Transferred {
        alias: restored, ..
    } = transferred_back
    else {
        panic!("return transfer should change ownership");
    };

    assert_eq!(
        restored,
        AgentAlias {
            state: AgentAliasState::Active,
            ..original
        }
    );
    assert_eq!(
        store
            .find_agent_alias_by_thread(second_session_id, child_thread_id)
            .await
            .expect("second alias lookup should succeed")
            .expect("second alias should remain reserved"),
        AgentAlias {
            state: AgentAliasState::Transferred,
            ..second_alias
        }
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(child_thread_id)
            .await
            .expect("current owner lookup should succeed"),
        Some(restored)
    );
}

#[tokio::test]
async fn history_fork_reserves_refs_and_nicknames_without_copying_targets() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source_root_thread_id = thread_id(/*suffix*/ 600);
    let source_session_id = SessionId::from(source_root_thread_id);
    let fork_root_thread_id = thread_id(/*suffix*/ 610);
    let fork_session_id = SessionId::from(fork_root_thread_id);
    let first_source_child = thread_id(/*suffix*/ 601);
    let second_source_child = thread_id(/*suffix*/ 602);
    store
        .ensure_agent_alias_namespace(source_session_id)
        .await
        .expect("source namespace should initialize");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root_thread_id,
            child_thread_id: first_source_child,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("first source alias should allocate");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root_thread_id,
            child_thread_id: second_source_child,
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("second source alias should allocate");

    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id,
            fork_session_id,
        })
        .await
        .expect("fork reservations should import");

    assert_eq!(
        store
            .list_agent_aliases(fork_session_id)
            .await
            .expect("fork aliases should list"),
        vec![AgentAlias {
            session_id: fork_session_id,
            thread_id: fork_root_thread_id,
            agent_ref: 1,
            nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
            state: AgentAliasState::Active,
        }]
    );
    assert_eq!(
        store
            .find_agent_alias_by_ref(fork_session_id, /*agent_ref*/ 2)
            .await
            .expect("reserved ref lookup should succeed"),
        None
    );
    assert_eq!(
        store
            .find_agent_alias_by_nickname(fork_session_id, "Hopper")
            .await
            .expect("reserved nickname lookup should succeed"),
        None
    );
    assert_eq!(
        store
            .list_agent_nickname_reservations(fork_session_id)
            .await
            .expect("nickname reservations should list"),
        vec!["Hopper".to_string(), "Noether".to_string()]
    );
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: fork_session_id,
            parent_thread_id: fork_root_thread_id,
            child_thread_id: thread_id(/*suffix*/ 611),
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect_err("inherited nickname must not bind to a different child");

    let adopted = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source_session_id),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: fork_session_id,
            new_parent_thread_id: fork_root_thread_id,
            thread_id: first_source_child,
            nickname: Some("Hopper".to_string()),
            authored_selector: first_source_child.to_string(),
        })
        .await
        .expect("reserved source child should be adoptable");
    let AgentAliasTransfer::Transferred { alias: adopted, .. } = adopted else {
        panic!("adoption should transfer ownership");
    };
    assert_eq!(adopted.agent_ref, 4);
    assert_eq!(adopted.nickname.as_deref(), Some("Hopper"));
    assert_eq!(
        store
            .list_agent_nickname_reservations(fork_session_id)
            .await
            .expect("remaining nickname reservations should list"),
        vec!["Noether".to_string()]
    );

    let new_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: fork_session_id,
            parent_thread_id: fork_root_thread_id,
            child_thread_id: thread_id(/*suffix*/ 612),
            nickname: Some("Curie".to_string()),
        })
        .await
        .expect("new fork-local alias should allocate");
    assert_eq!(new_alias.agent_ref, 5);
}

#[tokio::test]
async fn history_fork_carries_forward_inherited_nickname_reservations() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source_root_thread_id = thread_id(/*suffix*/ 700);
    let source_session_id = SessionId::from(source_root_thread_id);
    let first_fork_session_id = SessionId::from(thread_id(/*suffix*/ 710));
    let second_fork_session_id = SessionId::from(thread_id(/*suffix*/ 720));
    store
        .ensure_agent_alias_namespace(source_session_id)
        .await
        .expect("source namespace should initialize");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root_thread_id,
            child_thread_id: thread_id(/*suffix*/ 701),
            nickname: Some("Franklin".to_string()),
        })
        .await
        .expect("source alias should allocate");
    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id,
            fork_session_id: first_fork_session_id,
        })
        .await
        .expect("first fork reservations should import");
    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id: first_fork_session_id,
            fork_session_id: second_fork_session_id,
        })
        .await
        .expect("second fork reservations should import");

    assert_eq!(
        store
            .list_agent_nickname_reservations(second_fork_session_id)
            .await
            .expect("transitive reservations should list"),
        vec!["Franklin".to_string()]
    );
}

#[tokio::test]
async fn unpublished_fork_reservations_can_only_be_discarded_before_child_ownership() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source_root = thread_id(/*suffix*/ 730);
    let fork_root = thread_id(/*suffix*/ 740);
    let source_session_id = SessionId::from(source_root);
    let fork_session_id = SessionId::from(fork_root);
    store
        .ensure_agent_alias_namespace(source_session_id)
        .await
        .expect("source namespace should initialize");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: source_session_id,
            parent_thread_id: source_root,
            child_thread_id: thread_id(/*suffix*/ 731),
            nickname: Some("Noether".to_string()),
        })
        .await
        .expect("source child should allocate");
    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id,
            fork_session_id,
        })
        .await
        .expect("fork reservations should import");

    assert!(
        store
            .discard_fork_agent_alias_reservations(fork_session_id)
            .await
            .expect("unpublished fork reservations should discard")
    );
    assert_eq!(
        store
            .list_agent_aliases(fork_session_id)
            .await
            .expect("discarded namespace should have no aliases"),
        Vec::<AgentAlias>::new()
    );

    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id,
            fork_session_id,
        })
        .await
        .expect("fork reservations should re-import");
    let fork_child = thread_id(/*suffix*/ 741);
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            session_id: fork_session_id,
            parent_thread_id: fork_root,
            child_thread_id: fork_child,
            nickname: Some("Hopper".to_string()),
        })
        .await
        .expect("published fork child should allocate");

    assert!(
        !store
            .discard_fork_agent_alias_reservations(fork_session_id)
            .await
            .expect("owned fork namespace cleanup should be refused")
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(fork_child)
            .await
            .expect("fork child ownership should remain")
            .map(|alias| alias.session_id),
        Some(fork_session_id)
    );
}
