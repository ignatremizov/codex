use super::*;
use crate::AgentTaskPathMapping;

fn allocation(root: ThreadId, child: ThreadId, task_path: &str) -> AllocateAgentAliasRequest {
    AllocateAgentAliasRequest {
        session_id: root.into(),
        parent_thread_id: root,
        child_thread_id: child,
        nickname: None,
        task_path: Some(task_path.to_string()),
    }
}

#[tokio::test]
async fn task_paths_are_unique_atomically_and_survive_close_resume_and_reopen() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(Arc::clone(&fixture.state_db));
    let root = thread_id(/*suffix*/ 2000);
    let first = thread_id(/*suffix*/ 2001);
    let second = thread_id(/*suffix*/ 2002);
    let request = allocation(root, first, "/root/backend/auth");
    let other_request = allocation(root, second, "/root/backend/auth");
    let (first_result, second_result) = tokio::join!(
        store.allocate_agent_alias(request.clone()),
        store.allocate_agent_alias(other_request.clone()),
    );
    let (winner, loser, error) = match (first_result, second_result) {
        (Ok(alias), Err(error)) => (alias, other_request, error),
        (Err(error), Ok(alias)) => (alias, request, error),
        outcome => panic!("exactly one concurrent allocation must succeed: {outcome:?}"),
    };
    for expected in [
        "ref 2",
        &winner.thread_id.to_string(),
        "status open",
        "use the existing agent",
    ] {
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(
        store
            .list_thread_spawn_children(root, /*status_filter*/ None)
            .await
            .expect("spawn edges"),
        vec![winner.thread_id],
    );
    let repeated = store
        .allocate_agent_alias(allocation(root, winner.thread_id, "/root/ignored"))
        .await
        .expect("same UUID allocation is idempotent");
    assert_eq!(repeated, winner);
    store
        .set_agent_lifecycle_state(root.into(), winner.thread_id, ThreadSpawnEdgeStatus::Closed)
        .await
        .expect("close");
    let error = store
        .allocate_agent_alias(loser.clone())
        .await
        .expect_err("closed members retain their task path");
    assert!(error.to_string().contains("status closed"), "{error}");
    assert!(
        error.to_string().contains("resume the existing agent"),
        "{error}"
    );
    let restored = store
        .activate_agent_alias(AllocateAgentAliasRequest {
            task_path: None,
            ..allocation(root, winner.thread_id, "/root/ignored")
        })
        .await
        .expect("resume without a new assignment");
    assert_eq!(restored, winner);
    let resumed_again = store
        .activate_agent_alias(allocation(root, winner.thread_id, "/root/ignored"))
        .await
        .expect("repeat resume preserves assignment");
    assert_eq!(resumed_again, winner);
    let retry = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            task_path: Some("/root/another".to_string()),
            ..loser
        })
        .await
        .expect("failed allocations do not consume refs");
    assert_eq!(retry.agent_ref, 3);

    let reopened = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(fixture._codex_home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("reopen durable database");
    let reopened = LocalAgentGraphStore::new(reopened);
    assert_eq!(
        reopened
            .find_current_agent_alias_by_thread(winner.thread_id)
            .await
            .expect("read restored assignment"),
        Some(winner),
    );
}

#[tokio::test]
async fn task_paths_allow_separate_roots_and_do_not_reserve_history_fork_labels() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2100);
    let fork = thread_id(/*suffix*/ 2110);
    let source_alias = store
        .allocate_agent_alias(allocation(
            source,
            thread_id(/*suffix*/ 2101),
            "/root/Équipe/auth",
        ))
        .await
        .expect("source assignment");
    store
        .reserve_agent_aliases_for_fork(ReserveForkAgentAliasesRequest {
            source_session_id: source.into(),
            fork_session_id: fork.into(),
        })
        .await
        .expect("reserve historical short refs");
    let fork_alias = store
        .allocate_agent_alias(allocation(
            fork,
            thread_id(/*suffix*/ 2111),
            "/root/Équipe/auth",
        ))
        .await
        .expect("task labels are not historical reservations");
    assert_eq!(fork_alias.task_path, source_alias.task_path);
    assert_eq!(fork_alias.agent_ref, 3);
    assert_eq!(
        store
            .find_agent_alias_by_ref(fork.into(), /*agent_ref*/ 2)
            .await
            .expect("historical ref lookup"),
        None,
    );
}

#[tokio::test]
async fn adoption_remaps_task_hierarchy_not_lifecycle_and_preserves_historical_labels() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2200);
    let destination = thread_id(/*suffix*/ 2210);
    let target = thread_id(/*suffix*/ 2201);
    let task_parent = thread_id(/*suffix*/ 2202);
    let unrelated = thread_id(/*suffix*/ 2203);
    let target_alias = store
        .allocate_agent_alias(allocation(source, target, "/root/backend/review"))
        .await
        .expect("selected lifecycle parent");
    let parent_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            parent_thread_id: target,
            ..allocation(source, task_parent, "/root/backend")
        })
        .await
        .expect("task parent is a lifecycle child");
    let unrelated_alias = store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            parent_thread_id: target,
            ..allocation(source, unrelated, "/root/independent")
        })
        .await
        .expect("unrelated task prefix");
    store
        .set_agent_lifecycle_state(source.into(), task_parent, ThreadSpawnEdgeStatus::Closed)
        .await
        .expect("close imported task parent");
    let mut destination_aliases = Vec::new();
    for (suffix, path) in [
        (2211, "/root/backend"),
        (2212, "/root/backend-2"),
        (2213, "/root/backend-3/review"),
    ] {
        destination_aliases.push(
            store
                .allocate_agent_alias(allocation(destination, thread_id(suffix), path))
                .await
                .expect("destination assignment"),
        );
    }
    let result = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            task_path: None,
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: vec![task_parent, unrelated],
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: target,
            nickname: None,
            authored_selector: target.to_string(),
        })
        .await
        .expect("atomic subtree adoption");
    let AgentAliasTransfer::Transferred {
        alias,
        task_path_mapping,
        ..
    } = result
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        task_path_mapping,
        vec![
            AgentTaskPathMapping {
                thread_id: task_parent,
                previous_task_path: Some("/root/backend".to_string()),
                task_path: Some("/root/backend-3".to_string()),
            },
            AgentTaskPathMapping {
                thread_id: target,
                previous_task_path: Some("/root/backend/review".to_string()),
                task_path: Some("/root/backend-3/review-2".to_string()),
            },
        ],
    );
    assert_eq!(
        alias,
        AgentAlias {
            session_id: destination.into(),
            agent_ref: 5,
            task_path: Some("/root/backend-3/review-2".to_string()),
            ..target_alias.clone()
        },
    );
    for original in [target_alias, parent_alias, unrelated_alias] {
        assert_eq!(
            store
                .find_agent_alias_by_thread(source.into(), original.thread_id)
                .await
                .expect("historical alias"),
            Some(AgentAlias {
                state: AgentAliasState::Transferred,
                ..original
            }),
        );
    }
    for original in destination_aliases {
        assert_eq!(
            store
                .find_current_agent_alias_by_thread(original.thread_id)
                .await
                .expect("destination identity is untouched"),
            Some(original),
        );
    }
    let imported_parent = store
        .find_current_agent_alias_by_thread(task_parent)
        .await
        .expect("imported child")
        .expect("current owner");
    assert_eq!(
        (imported_parent.task_path, imported_parent.state),
        (Some("/root/backend-3".to_string()), AgentAliasState::Closed),
    );
    let replacement = store
        .allocate_agent_alias(allocation(
            source,
            thread_id(/*suffix*/ 2204),
            "/root/backend",
        ))
        .await
        .expect("transferred historical label does not block former root");
    assert_eq!(replacement.agent_ref, 5);
}

#[tokio::test]
async fn adopted_main_explicit_assignment_remaps_imported_prefixes() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2300);
    let destination = thread_id(/*suffix*/ 2310);
    let child = thread_id(/*suffix*/ 2301);
    let pending_child = thread_id(/*suffix*/ 2302);
    store
        .allocate_agent_alias(allocation(source, child, "/root/review"))
        .await
        .expect("source assignment");
    store
        .allocate_agent_alias(allocation(source, pending_child, "/root/backend-3"))
        .await
        .expect("old descendant label will move beneath the explicit assignment");
    for (suffix, path) in [(2311, "/root/backend"), (2312, "/root/backend-2")] {
        store
            .allocate_agent_alias(allocation(destination, thread_id(suffix), path))
            .await
            .expect("destination assignment");
    }
    let result = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            task_path: Some("/root/backend".to_string()),
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: vec![child, pending_child],
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: source,
            nickname: None,
            authored_selector: source.to_string(),
        })
        .await
        .expect("foreign Main adoption");
    let AgentAliasTransfer::Transferred {
        task_path_mapping, ..
    } = result
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        task_path_mapping,
        vec![
            AgentTaskPathMapping {
                thread_id: source,
                previous_task_path: Some("/root".to_string()),
                task_path: Some("/root/backend-3".to_string()),
            },
            AgentTaskPathMapping {
                thread_id: pending_child,
                previous_task_path: Some("/root/backend-3".to_string()),
                task_path: Some("/root/backend-3/backend-3".to_string()),
            },
            AgentTaskPathMapping {
                thread_id: child,
                previous_task_path: Some("/root/review".to_string()),
                task_path: Some("/root/backend-3/review".to_string()),
            },
        ],
    );
    assert_eq!(
        store
            .ensure_agent_alias_namespace(destination.into())
            .await
            .expect("destination Main")
            .task_path,
        Some("/root".to_string()),
    );
}

#[tokio::test]
async fn adopted_main_without_assignment_is_unlabeled_and_keeps_descendant_assignments() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2500);
    let destination = thread_id(/*suffix*/ 2510);
    let child = thread_id(/*suffix*/ 2501);
    let original = store
        .allocate_agent_alias(allocation(source, child, "/root/review"))
        .await
        .expect("source assignment");
    let result = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: vec![child],
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: source,
            nickname: None,
            task_path: None,
            authored_selector: source.to_string(),
        })
        .await
        .expect("foreign Main adoption without a new assignment");
    let AgentAliasTransfer::Transferred {
        alias,
        task_path_mapping,
        ..
    } = result
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        task_path_mapping,
        vec![AgentTaskPathMapping {
            thread_id: source,
            previous_task_path: Some("/root".to_string()),
            task_path: None,
        }],
    );
    assert_eq!(
        alias,
        AgentAlias {
            session_id: destination.into(),
            thread_id: source,
            agent_ref: 2,
            nickname: None,
            task_path: None,
            state: AgentAliasState::Active,
        },
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(child)
            .await
            .expect("destination child"),
        Some(AgentAlias {
            session_id: destination.into(),
            agent_ref: 3,
            ..original
        }),
    );
}

#[tokio::test]
async fn adoption_can_explicitly_assign_an_unlabeled_thread() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2600);
    let destination = thread_id(/*suffix*/ 2610);
    let child = thread_id(/*suffix*/ 2601);
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            task_path: None,
            ..allocation(source, child, "/root/unused")
        })
        .await
        .expect("unlabeled source");
    let result = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: child,
            nickname: None,
            task_path: Some("/root/backend".to_string()),
            authored_selector: child.to_string(),
        })
        .await
        .expect("explicit adoption assignment");
    let AgentAliasTransfer::Transferred {
        alias,
        task_path_mapping,
        ..
    } = result
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(alias.task_path, Some("/root/backend".to_string()));
    assert_eq!(
        task_path_mapping,
        vec![AgentTaskPathMapping {
            thread_id: child,
            previous_task_path: None,
            task_path: Some("/root/backend".to_string()),
        }],
    );
}

#[tokio::test]
async fn adoption_suffix_does_not_take_another_imported_assignment() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2700);
    let destination = thread_id(/*suffix*/ 2710);
    let target = thread_id(/*suffix*/ 2701);
    let child = thread_id(/*suffix*/ 2702);
    store
        .allocate_agent_alias(allocation(source, target, "/root/backend"))
        .await
        .expect("source target");
    store
        .allocate_agent_alias(AllocateAgentAliasRequest {
            parent_thread_id: target,
            ..allocation(source, child, "/root/backend-2")
        })
        .await
        .expect("independent imported task");
    store
        .allocate_agent_alias(allocation(
            destination,
            thread_id(/*suffix*/ 2711),
            "/root/backend",
        ))
        .await
        .expect("destination collision");
    let result = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: vec![child],
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: target,
            nickname: None,
            task_path: None,
            authored_selector: target.to_string(),
        })
        .await
        .expect("adopt together");
    let AgentAliasTransfer::Transferred {
        task_path_mapping, ..
    } = result
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        task_path_mapping,
        vec![AgentTaskPathMapping {
            thread_id: target,
            previous_task_path: Some("/root/backend".to_string()),
            task_path: Some("/root/backend-3".to_string()),
        }],
    );
    assert_eq!(
        store
            .find_current_agent_alias_by_thread(child)
            .await
            .expect("imported task")
            .expect("current alias")
            .task_path,
        Some("/root/backend-2".to_string()),
    );
}

#[tokio::test]
async fn returning_adoption_uses_current_assignment_and_keeps_historical_ref() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let source = thread_id(/*suffix*/ 2900);
    let destination = thread_id(/*suffix*/ 2910);
    let target = thread_id(/*suffix*/ 2901);
    let original = store
        .allocate_agent_alias(allocation(source, target, "/root/backend"))
        .await
        .expect("original assignment");
    store
        .allocate_agent_alias(allocation(
            destination,
            thread_id(/*suffix*/ 2911),
            "/root/backend",
        ))
        .await
        .expect("destination collision");
    store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(source.into()),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: destination.into(),
            new_parent_thread_id: destination,
            thread_id: target,
            nickname: None,
            task_path: None,
            authored_selector: target.to_string(),
        })
        .await
        .expect("first adoption");
    store
        .allocate_agent_alias(allocation(
            source,
            thread_id(/*suffix*/ 2902),
            "/root/backend-2",
        ))
        .await
        .expect("current source member");
    let returned = store
        .transfer_agent_alias(TransferAgentAliasRequest {
            expected_previous_session_id: Some(destination.into()),
            expected_descendant_thread_ids: Vec::new(),
            new_session_id: source.into(),
            new_parent_thread_id: source,
            thread_id: target,
            nickname: None,
            task_path: None,
            authored_selector: target.to_string(),
        })
        .await
        .expect("return to historical namespace");
    let AgentAliasTransfer::Transferred {
        alias,
        task_path_mapping,
        ..
    } = returned
    else {
        panic!("ownership must transfer");
    };
    assert_eq!(
        alias,
        AgentAlias {
            task_path: Some("/root/backend-2-2".to_string()),
            ..original
        },
    );
    assert_eq!(
        task_path_mapping,
        vec![AgentTaskPathMapping {
            thread_id: target,
            previous_task_path: Some("/root/backend-2".to_string()),
            task_path: Some("/root/backend-2-2".to_string()),
        }],
    );
}

#[tokio::test]
async fn allocation_rejects_noncanonical_labels_without_consuming_refs() {
    let fixture = state_runtime().await;
    let store = LocalAgentGraphStore::new(fixture.state_db);
    let root = thread_id(/*suffix*/ 2400);
    let child = thread_id(/*suffix*/ 2401);
    for path in [
        "/root",
        "relative",
        "/root/",
        "/root//auth",
        "/root/.",
        "/root/../auth",
        "/root/a b",
        "/root/a\nb",
        "/root/a\\b",
    ] {
        store
            .allocate_agent_alias(allocation(root, child, path))
            .await
            .expect_err("invalid canonical task label");
    }
    let alias = store
        .allocate_agent_alias(allocation(root, child, "/root/Équipe"))
        .await
        .expect("valid Unicode label");
    assert_eq!(alias.agent_ref, 2);
}
