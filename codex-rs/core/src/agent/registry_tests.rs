use super::*;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErrorDetails;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

fn agent_path(path: &str) -> AgentPath {
    AgentPath::try_from(path).expect("valid agent path")
}

fn agent_metadata(thread_id: ThreadId) -> AgentMetadata {
    AgentMetadata {
        agent_id: Some(thread_id),
        ..Default::default()
    }
}

#[tokio::test]
async fn reregistered_recipient_reuses_live_gate_but_rejects_stale_assignment_updates() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    registry
        .reserve_spawn_slot(Some(1))
        .expect("reserve")
        .commit(agent_metadata(thread_id));
    let old = registry.mailbox_submission(thread_id);
    let permit = Arc::clone(&old.semaphore)
        .acquire_owned()
        .await
        .expect("gate");
    registry.release_spawned_thread(thread_id);
    registry
        .reserve_spawn_slot(Some(1))
        .expect("reserve again")
        .commit(agent_metadata(thread_id));
    let current = registry.mailbox_submission(thread_id);
    assert!(Arc::ptr_eq(&old.semaphore, &current.semaphore));
    assert!(!Arc::ptr_eq(&old, &current));
    assert!(!registry.submission_is_current(thread_id, &old));
    assert!(registry.submission_is_current(thread_id, &current));
    assert!(current.semaphore.try_acquire().is_err());
    registry.update_last_task_message(thread_id, &old, Some("stale".to_string()));
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .and_then(|metadata| metadata.last_task_message),
        None,
    );
    drop(permit);
    let _permit = tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 1),
        Arc::clone(&current.semaphore).acquire_owned(),
    )
    .await
    .expect("shared gate released")
    .expect("gate");
    registry.update_last_task_message(thread_id, &current, Some("current".to_string()));
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .and_then(|metadata| metadata.last_task_message),
        Some("current".to_string()),
    );
}

#[test]
fn repeated_root_registration_preserves_submission_identity_and_assignment() {
    let registry = AgentRegistry::default();
    let root_id = ThreadId::new();
    registry.register_root_thread(root_id);
    let submission = registry.mailbox_submission(root_id);
    registry.update_last_task_message(root_id, &submission, Some("accepted".to_string()));
    registry.register_root_thread(root_id);
    assert!(Arc::ptr_eq(
        &submission,
        &registry.mailbox_submission(root_id)
    ));
    assert_eq!(
        registry
            .agent_metadata_for_thread(root_id)
            .and_then(|metadata| metadata.last_task_message),
        Some("accepted".to_string()),
    );
}

#[test]
fn format_agent_nickname_adds_ordinals_after_reset() {
    assert_eq!(
        format_agent_nickname("Plato", /*nickname_reset_count*/ 0),
        "Plato"
    );
    assert_eq!(
        format_agent_nickname("Plato", /*nickname_reset_count*/ 1),
        "Plato the 2nd"
    );
    assert_eq!(
        format_agent_nickname("Plato", /*nickname_reset_count*/ 2),
        "Plato the 3rd"
    );
    assert_eq!(
        format_agent_nickname("Plato", /*nickname_reset_count*/ 10),
        "Plato the 11th"
    );
    assert_eq!(
        format_agent_nickname("Plato", /*nickname_reset_count*/ 20),
        "Plato the 21st"
    );
}

#[test]
fn session_depth_defaults_to_zero_for_root_sources() {
    assert_eq!(session_depth(&SessionSource::Cli), 0);
}

#[test]
fn thread_spawn_depth_increments_and_enforces_limit() {
    let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let child_depth = next_thread_spawn_depth(&session_source);
    assert_eq!(child_depth, 2);
    assert!(exceeds_thread_spawn_depth_limit(
        child_depth,
        /*max_depth*/ 1
    ));
}

#[test]
fn non_thread_spawn_subagents_default_to_depth_zero() {
    let session_source = SessionSource::SubAgent(SubAgentSource::Review);
    assert_eq!(session_depth(&session_source), 0);
    assert_eq!(next_thread_spawn_depth(&session_source), 1);
    assert!(!exceeds_thread_spawn_depth_limit(
        /*depth*/ 1, /*max_depth*/ 1
    ));
}

#[test]
fn reservation_drop_releases_slot() {
    let registry = Arc::new(AgentRegistry::default());
    let reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    drop(reservation);

    let reservation = registry.reserve_spawn_slot(Some(1)).expect("slot released");
    drop(reservation);
}

#[test]
fn commit_holds_slot_until_release() {
    let registry = Arc::new(AgentRegistry::default());
    let reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    let thread_id = ThreadId::new();
    reservation.commit(agent_metadata(thread_id));

    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .and_then(|metadata| metadata.agent_id),
        Some(thread_id)
    );

    let err = match registry.reserve_spawn_slot(Some(1)) {
        Ok(_) => panic!("limit should be enforced"),
        Err(err) => err,
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    registry.release_spawned_thread(thread_id);
    assert!(registry.agent_metadata_for_thread(thread_id).is_none());
    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("slot released after thread removal");
    drop(reservation);
}

#[test]
fn releasing_one_spawned_thread_preserves_sibling_identity() {
    let registry = Arc::new(AgentRegistry::default());
    let first_id = ThreadId::new();
    let second_id = ThreadId::new();

    for thread_id in [first_id, second_id] {
        registry
            .reserve_spawn_slot(/*max_threads*/ None)
            .expect("reserve sibling slot")
            .commit(agent_metadata(thread_id));
    }

    registry.release_spawned_thread(first_id);

    assert!(registry.agent_metadata_for_thread(first_id).is_none());
    assert_eq!(
        registry
            .agent_metadata_for_thread(second_id)
            .and_then(|metadata| metadata.agent_id),
        Some(second_id)
    );
}

#[test]
fn release_ignores_unknown_thread_id() {
    let registry = Arc::new(AgentRegistry::default());
    let reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    let thread_id = ThreadId::new();
    reservation.commit(agent_metadata(thread_id));

    registry.release_spawned_thread(ThreadId::new());

    let err = match registry.reserve_spawn_slot(Some(1)) {
        Ok(_) => panic!("limit should still be enforced"),
        Err(err) => err,
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    registry.release_spawned_thread(thread_id);
    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("slot released after real thread removal");
    drop(reservation);
}

#[test]
fn release_is_idempotent_for_registered_threads() {
    let registry = Arc::new(AgentRegistry::default());
    let reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    let first_id = ThreadId::new();
    reservation.commit(agent_metadata(first_id));

    registry.release_spawned_thread(first_id);

    let reservation = registry.reserve_spawn_slot(Some(1)).expect("slot reused");
    let second_id = ThreadId::new();
    reservation.commit(agent_metadata(second_id));

    registry.release_spawned_thread(first_id);

    let err = match registry.reserve_spawn_slot(Some(1)) {
        Ok(_) => panic!("limit should still be enforced"),
        Err(err) => err,
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    registry.release_spawned_thread(second_id);
    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("slot released after second thread removal");
    drop(reservation);
}

#[test]
fn failed_spawn_keeps_nickname_marked_used() {
    let registry = Arc::new(AgentRegistry::default());
    let mut reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let agent_nickname = reservation
        .reserve_agent_nickname_with_preference(&["alpha"], /*preferred*/ None)
        .expect("reserve agent name");
    assert_eq!(agent_nickname, "alpha");
    drop(reservation);

    let mut reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let agent_nickname = reservation
        .reserve_agent_nickname_with_preference(&["alpha", "beta"], /*preferred*/ None)
        .expect("unused name should still be preferred");
    assert_eq!(agent_nickname, "beta");
}

#[test]
fn agent_nickname_resets_used_pool_when_exhausted() {
    let registry = Arc::new(AgentRegistry::default());
    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    let first_name = first
        .reserve_agent_nickname_with_preference(&["alpha"], /*preferred*/ None)
        .expect("reserve first agent name");
    let first_id = ThreadId::new();
    first.commit(agent_metadata(first_id));
    assert_eq!(first_name, "alpha");

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    let second_name = second
        .reserve_agent_nickname_with_preference(&["alpha"], /*preferred*/ None)
        .expect("name should be reused after pool reset");
    assert_eq!(second_name, "alpha the 2nd");
    let active_agents = registry
        .active_agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(active_agents.nickname_reset_count, 1);
}

#[test]
fn reserved_main_candidate_never_acquires_an_ordinal_suffix() {
    let registry = Arc::new(AgentRegistry::default());
    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    assert_eq!(
        first
            .reserve_agent_nickname_with_preference(&["Main", "Hopper"], /*preferred*/ None,)
            .expect("ordinary candidate should remain available"),
        "Hopper"
    );
    first.commit(agent_metadata(ThreadId::new()));

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    assert_eq!(
        second
            .reserve_agent_nickname_with_preference(&["main", "Hopper"], /*preferred*/ None,)
            .expect("pool reset should suffix only the ordinary candidate"),
        "Hopper the 2nd"
    );
}

#[test]
fn released_nickname_stays_used_until_pool_reset() {
    let registry = Arc::new(AgentRegistry::default());

    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    let first_name = first
        .reserve_agent_nickname_with_preference(&["alpha"], /*preferred*/ None)
        .expect("reserve first agent name");
    let first_id = ThreadId::new();
    first.commit(agent_metadata(first_id));
    assert_eq!(first_name, "alpha");

    registry.release_spawned_thread(first_id);

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    let second_name = second
        .reserve_agent_nickname_with_preference(&["alpha", "beta"], /*preferred*/ None)
        .expect("released name should still be marked used");
    assert_eq!(second_name, "beta");
    let second_id = ThreadId::new();
    second.commit(agent_metadata(second_id));
    registry.release_spawned_thread(second_id);

    let mut third = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve third slot");
    let third_name = third
        .reserve_agent_nickname_with_preference(&["alpha", "beta"], /*preferred*/ None)
        .expect("pool reset should permit a duplicate");
    let expected_names = HashSet::from(["alpha the 2nd".to_string(), "beta the 2nd".to_string()]);
    assert!(expected_names.contains(&third_name));
    let active_agents = registry
        .active_agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(active_agents.nickname_reset_count, 1);
}

#[test]
fn durable_nickname_reservations_survive_pool_resets() {
    let registry = Arc::new(AgentRegistry::default());
    registry.reserve_durable_agent_nicknames(["alpha".to_string()]);

    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    let first_name = first
        .reserve_agent_nickname_with_preference(&["alpha", "beta"], /*preferred*/ None)
        .expect("reserve first agent name");
    assert_eq!(first_name, "beta");
    first.commit(agent_metadata(ThreadId::new()));

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    let second_name = second
        .reserve_agent_nickname_with_preference(&["alpha", "beta"], /*preferred*/ None)
        .expect("reserve suffixed agent name");
    assert!(
        matches!(second_name.as_str(), "alpha the 2nd" | "beta the 2nd"),
        "pool reset should advance past durable base names, got {second_name}"
    );
}

#[test]
fn repeated_resets_advance_the_ordinal_suffix() {
    let registry = Arc::new(AgentRegistry::default());

    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    let first_name = first
        .reserve_agent_nickname_with_preference(&["Plato"], /*preferred*/ None)
        .expect("reserve first agent name");
    let first_id = ThreadId::new();
    first.commit(agent_metadata(first_id));
    assert_eq!(first_name, "Plato");
    registry.release_spawned_thread(first_id);

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    let second_name = second
        .reserve_agent_nickname_with_preference(&["Plato"], /*preferred*/ None)
        .expect("reserve second agent name");
    let second_id = ThreadId::new();
    second.commit(agent_metadata(second_id));
    assert_eq!(second_name, "Plato the 2nd");
    registry.release_spawned_thread(second_id);

    let mut third = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve third slot");
    let third_name = third
        .reserve_agent_nickname_with_preference(&["Plato"], /*preferred*/ None)
        .expect("reserve third agent name");
    assert_eq!(third_name, "Plato the 3rd");
    let active_agents = registry
        .active_agents
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(active_agents.nickname_reset_count, 2);
}

#[test]
fn register_root_thread_indexes_root_path() {
    let registry = Arc::new(AgentRegistry::default());
    let root_thread_id = ThreadId::new();

    registry.register_root_thread(root_thread_id);

    assert_eq!(
        registry.agent_id_for_path(&AgentPath::root()),
        Some(root_thread_id)
    );
    assert_eq!(
        registry.agent_metadata_for_thread(root_thread_id),
        Some(AgentMetadata {
            agent_id: Some(root_thread_id),
            agent_path: Some(AgentPath::root()),
            agent_nickname: Some(MAIN_AGENT_NICKNAME.to_string()),
            ..Default::default()
        })
    );

    let other_thread_id = ThreadId::new();
    registry.register_root_thread(other_thread_id);

    assert_eq!(
        registry.agent_id_for_path(&AgentPath::root()),
        Some(root_thread_id)
    );
    assert_eq!(
        registry.agent_metadata_for_thread(root_thread_id),
        Some(AgentMetadata {
            agent_id: Some(root_thread_id),
            agent_path: Some(AgentPath::root()),
            agent_nickname: Some(MAIN_AGENT_NICKNAME.to_string()),
            ..Default::default()
        })
    );
    assert!(
        registry
            .agent_metadata_for_thread(other_thread_id)
            .is_none()
    );

    registry.release_spawned_thread(root_thread_id);
    assert_eq!(registry.agent_id_for_path(&AgentPath::root()), None);
    assert!(registry.agent_metadata_for_thread(root_thread_id).is_none());

    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("releasing the uncounted root should not consume a spawn slot");
    drop(reservation);
}

#[test]
fn reserved_agent_path_is_released_when_spawn_fails() {
    let registry = Arc::new(AgentRegistry::default());
    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first slot");
    first
        .reserve_agent_path(&agent_path("/root/researcher"))
        .expect("reserve first path");
    drop(first);

    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve second slot");
    second
        .reserve_agent_path(&agent_path("/root/researcher"))
        .expect("dropped reservation should free the path");
}

#[test]
fn committed_agent_path_is_indexed_until_release() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let mut reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    reservation
        .reserve_agent_path(&agent_path("/root/researcher"))
        .expect("reserve path");
    reservation.commit(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(agent_path("/root/researcher")),
        ..Default::default()
    });

    assert_eq!(
        registry.agent_id_for_path(&agent_path("/root/researcher")),
        Some(thread_id)
    );
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .and_then(|metadata| metadata.agent_path),
        Some(agent_path("/root/researcher"))
    );

    registry.release_spawned_thread(thread_id);
    assert_eq!(
        registry.agent_id_for_path(&agent_path("/root/researcher")),
        None
    );
    assert!(registry.agent_metadata_for_thread(thread_id).is_none());
}

#[test]
fn replacing_agent_metadata_updates_thread_identity_index() {
    let registry = AgentRegistry::default();
    let previous_thread_id = ThreadId::new();
    let current_thread_id = ThreadId::new();
    let path = agent_path("/root/researcher");

    registry.register_spawned_thread(AgentMetadata {
        agent_id: Some(previous_thread_id),
        agent_path: Some(path.clone()),
        ..Default::default()
    });
    registry.register_spawned_thread(AgentMetadata {
        agent_id: Some(current_thread_id),
        agent_path: Some(path.clone()),
        ..Default::default()
    });
    registry.register_spawned_thread(AgentMetadata {
        agent_id: Some(current_thread_id),
        agent_path: Some(path.clone()),
        agent_role: Some("researcher".to_string()),
        ..Default::default()
    });

    assert!(
        registry
            .agent_metadata_for_thread(previous_thread_id)
            .is_none()
    );
    assert_eq!(registry.agent_id_for_path(&path), Some(current_thread_id));
    assert_eq!(
        registry
            .agent_metadata_for_thread(current_thread_id)
            .map(|metadata| (metadata.agent_path, metadata.agent_role)),
        Some((Some(path), Some("researcher".to_string())))
    );

    registry.release_spawned_thread(previous_thread_id);
    assert_eq!(
        registry
            .agent_metadata_for_thread(current_thread_id)
            .and_then(|metadata| metadata.agent_id),
        Some(current_thread_id)
    );
}

#[test]
fn canonical_metadata_replacement_updates_identity_without_changing_count() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let previous_path = agent_path("/root/previous");
    let canonical_path = agent_path("/root/canonical");
    let mut reservation = registry
        .reserve_spawn_slot(/*max_threads*/ Some(1))
        .expect("reserve initial slot");
    reservation
        .reserve_agent_path(&previous_path)
        .expect("reserve initial path");
    reservation.commit(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(previous_path.clone()),
        agent_nickname: Some("Previous".to_string()),
        last_task_message: Some("continue".to_string()),
        ..Default::default()
    });

    registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(canonical_path.clone()),
                agent_nickname: Some("Canonical".to_string()),
                agent_role: Some("worker".to_string()),
                last_task_message: Some("stale snapshot".to_string()),
            },
        )
        .expect("reserve replacement")
        .commit()
        .expect("commit replacement");

    assert_eq!(registry.agent_id_for_path(&previous_path), None);
    assert_eq!(registry.agent_id_for_path(&canonical_path), Some(thread_id));
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (
                metadata.agent_path,
                metadata.agent_nickname,
                metadata.agent_role,
                metadata.last_task_message,
            )),
        Some((
            Some(canonical_path),
            Some("Canonical".to_string()),
            Some("worker".to_string()),
            Some("continue".to_string()),
        ))
    );

    registry.release_spawned_thread(thread_id);
    assert!(
        registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok(),
        "replacement should not double-count the thread"
    );
}

#[test]
fn duplicate_pathless_resume_registration_releases_its_reserved_count() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first resume slot")
        .commit(AgentMetadata {
            agent_id: Some(thread_id),
            ..Default::default()
        });
    let duplicate_committed = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve duplicate resume slot")
        .commit_if_absent(AgentMetadata {
            agent_id: Some(thread_id),
            ..Default::default()
        });

    assert!(!duplicate_committed);
    registry.release_spawned_thread(thread_id);
    assert!(
        registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok(),
        "duplicate registration should not leak capacity"
    );
}

#[test]
fn metadata_replacement_reserves_new_path_without_releasing_old_path() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let old_path = agent_path("/root/old");
    let new_path = agent_path("/root/new");
    let mut initial = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve initial slot");
    initial
        .reserve_agent_path(&old_path)
        .expect("reserve old path");
    initial.commit(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(old_path.clone()),
        ..Default::default()
    });

    let replacement = registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(new_path.clone()),
                ..Default::default()
            },
        )
        .expect("reserve metadata replacement");

    assert_eq!(registry.agent_id_for_path(&old_path), Some(thread_id));
    let mut competing = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve competing slot");
    assert!(competing.reserve_agent_path(&old_path).is_err());
    assert!(competing.reserve_agent_path(&new_path).is_err());

    drop(replacement);
    assert_eq!(registry.agent_id_for_path(&old_path), Some(thread_id));
    assert!(competing.reserve_agent_path(&new_path).is_ok());
}

#[test]
fn metadata_replacement_keeps_old_path_reserved_after_registration_removal() {
    let registry = Arc::new(AgentRegistry::default());
    let restored_thread_id = ThreadId::new();
    let sibling_thread_id = ThreadId::new();
    let old_path = agent_path("/root/old");
    let new_path = agent_path("/root/new");
    let mut initial = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve initial slot");
    initial
        .reserve_agent_path(&old_path)
        .expect("reserve old path");
    initial.commit(AgentMetadata {
        agent_id: Some(restored_thread_id),
        agent_path: Some(old_path.clone()),
        ..Default::default()
    });
    let replacement = registry
        .reserve_agent_metadata_replacement(
            restored_thread_id,
            AgentMetadata {
                agent_id: Some(restored_thread_id),
                agent_path: Some(new_path.clone()),
                ..Default::default()
            },
        )
        .expect("reserve metadata replacement");

    registry.release_spawned_thread(restored_thread_id);
    let mut sibling = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve sibling slot");
    assert!(sibling.reserve_agent_path(&old_path).is_err());
    assert!(sibling.reserve_agent_path(&new_path).is_err());
    assert!(replacement.commit().is_err());
    sibling
        .reserve_agent_path(&old_path)
        .expect("failed replacement releases the old path");
    sibling.commit(AgentMetadata {
        agent_id: Some(sibling_thread_id),
        agent_path: Some(old_path.clone()),
        ..Default::default()
    });

    assert_eq!(
        registry.agent_id_for_path(&old_path),
        Some(sibling_thread_id)
    );
    assert_eq!(registry.agent_id_for_path(&new_path), None);
    registry.release_spawned_thread(sibling_thread_id);
    assert!(registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok());
}

#[test]
fn unchanged_metadata_path_commit_refuses_to_overwrite_new_sibling_owner() {
    let registry = Arc::new(AgentRegistry::default());
    let restored_thread_id = ThreadId::new();
    let sibling_thread_id = ThreadId::new();
    let path = agent_path("/root/worker");
    let mut initial = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve initial slot");
    initial
        .reserve_agent_path(&path)
        .expect("reserve initial path");
    initial.commit(AgentMetadata {
        agent_id: Some(restored_thread_id),
        agent_path: Some(path.clone()),
        ..Default::default()
    });
    let replacement = registry
        .reserve_agent_metadata_replacement(
            restored_thread_id,
            AgentMetadata {
                agent_id: Some(restored_thread_id),
                agent_path: Some(path.clone()),
                agent_role: Some("worker".to_string()),
                ..Default::default()
            },
        )
        .expect("reserve unchanged-path replacement");

    registry.release_spawned_thread(restored_thread_id);
    let sibling = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve sibling slot");
    // Simulate registration replacement outside the guarded restoration path. The pending
    // transaction must still refuse to overwrite that new owner or remove its metadata.
    sibling.commit(AgentMetadata {
        agent_id: Some(sibling_thread_id),
        agent_path: Some(path.clone()),
        ..Default::default()
    });

    assert!(replacement.commit().is_err());
    assert_eq!(registry.agent_id_for_path(&path), Some(sibling_thread_id));
    registry.release_spawned_thread(sibling_thread_id);
    assert!(registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok());
}

#[test]
fn thread_identity_can_move_between_pathless_and_path_backed_metadata() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let path = agent_path("/root/researcher");
    let reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    reservation.commit(agent_metadata(thread_id));

    registry.register_spawned_thread(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(path.clone()),
        ..Default::default()
    });

    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (metadata.agent_id, metadata.agent_path)),
        Some((Some(thread_id), Some(path.clone())))
    );
    assert_eq!(registry.agent_id_for_path(&path), Some(thread_id));

    registry.register_spawned_thread(agent_metadata(thread_id));

    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (metadata.agent_id, metadata.agent_path)),
        Some((Some(thread_id), None))
    );
    assert_eq!(registry.agent_id_for_path(&path), None);

    let mut path_reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve path reuse slot");
    path_reservation
        .reserve_agent_path(&path)
        .expect("moving back to pathless metadata should release the old path");
    drop(path_reservation);

    registry.release_spawned_thread(thread_id);
    assert!(registry.agent_metadata_for_thread(thread_id).is_none());

    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("releasing the migrated agent should free its spawn slot");
    drop(reservation);
}

#[test]
fn thread_identity_can_move_between_agent_paths() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let previous_path = agent_path("/root/researcher");
    let current_path = agent_path("/root/reviewer");
    let mut reservation = registry.reserve_spawn_slot(Some(1)).expect("reserve slot");
    reservation
        .reserve_agent_path(&previous_path)
        .expect("reserve original path");
    reservation.commit(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(previous_path.clone()),
        ..Default::default()
    });

    registry.register_spawned_thread(AgentMetadata {
        agent_id: Some(thread_id),
        agent_path: Some(current_path.clone()),
        agent_role: Some("reviewer".to_string()),
        ..Default::default()
    });

    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (metadata.agent_id, metadata.agent_path, metadata.agent_role)),
        Some((
            Some(thread_id),
            Some(current_path.clone()),
            Some("reviewer".to_string())
        ))
    );
    assert_eq!(registry.agent_id_for_path(&previous_path), None);
    assert_eq!(registry.agent_id_for_path(&current_path), Some(thread_id));

    let mut path_reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve path reuse slot");
    path_reservation
        .reserve_agent_path(&previous_path)
        .expect("moving to a different path should release the old path");
    drop(path_reservation);

    registry.release_spawned_thread(thread_id);
    assert_eq!(registry.agent_id_for_path(&current_path), None);
    assert!(registry.agent_metadata_for_thread(thread_id).is_none());

    let reservation = registry
        .reserve_spawn_slot(Some(1))
        .expect("releasing the migrated agent should free its spawn slot");
    drop(reservation);
}
