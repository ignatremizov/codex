use super::*;
use pretty_assertions::assert_eq;

#[test]
fn replacement_preserves_live_submission_identity_assignment_and_environments() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    registry
        .reserve_spawn_slot(/*max_threads*/ Some(1))
        .expect("reserve")
        .commit(AgentMetadata {
            agent_id: Some(thread_id),
            agent_path: Some(AgentPath::try_from("/root/old").expect("path")),
            last_task_message: Some("old assignment".to_string()),
            ..Default::default()
        });
    let submission = registry.mailbox_submission(thread_id);
    let permit = submission.semaphore.try_acquire().expect("hold gate");
    let replacement = registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::try_from("/root/new").expect("path")),
                last_task_message: Some("stale snapshot".to_string()),
                ..Default::default()
            },
        )
        .expect("reserve replacement");
    // Both updates happen after the restoration caller captured its metadata snapshot.
    registry.update_last_task_message(thread_id, &submission, /*last_task_message*/ None);
    registry.save_evicted_environments(thread_id, Vec::new());
    replacement.commit().expect("commit");

    let current = registry.mailbox_submission(thread_id);
    assert!(Arc::ptr_eq(&submission, &current));
    assert!(current.semaphore.try_acquire().is_err());
    assert!(registry.submission_is_current(thread_id, &submission));
    assert_eq!(registry.evicted_environments(thread_id), Some(Vec::new()));
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (
                metadata.agent_id,
                metadata.agent_path,
                metadata.agent_nickname,
                metadata.agent_role,
                metadata.last_task_message,
            )),
        Some((
            Some(thread_id),
            Some(AgentPath::try_from("/root/new").expect("path")),
            None,
            None,
            None,
        )),
    );
    drop(permit);
    assert!(current.semaphore.try_acquire().is_ok());
    registry.release_spawned_thread(thread_id);
    assert!(registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok());
}

#[test]
fn stale_replacement_cannot_overwrite_same_id_registration_or_assignment() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    registry
        .reserve_spawn_slot(/*max_threads*/ Some(1))
        .expect("reserve")
        .commit(AgentMetadata {
            agent_id: Some(thread_id),
            ..Default::default()
        });
    let previous = registry.mailbox_submission(thread_id);
    let replacement = registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::try_from("/root/stale").expect("path")),
                ..Default::default()
            },
        )
        .expect("reserve replacement");
    registry.release_spawned_thread(thread_id);
    registry
        .reserve_spawn_slot(/*max_threads*/ Some(1))
        .expect("reserve new generation")
        .commit(AgentMetadata {
            agent_id: Some(thread_id),
            last_task_message: Some("new generation".to_string()),
            ..Default::default()
        });
    let current = registry.mailbox_submission(thread_id);
    assert!(!Arc::ptr_eq(&previous, &current));
    assert!(Arc::ptr_eq(&previous.semaphore, &current.semaphore));
    assert!(replacement.commit().is_err());
    assert_eq!(
        registry
            .agent_metadata_for_thread(thread_id)
            .map(|metadata| (
                metadata.agent_id,
                metadata.agent_path,
                metadata.last_task_message,
            )),
        Some((Some(thread_id), None, Some("new generation".to_string()))),
    );
    assert!(registry.submission_is_current(thread_id, &current));
    assert!(!registry.submission_is_current(thread_id, &previous));
    registry.release_spawned_thread(thread_id);
    assert!(registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok());
}

#[test]
fn new_registration_shares_a_preexisting_unregistered_submission_gate() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let previous = registry.mailbox_submission(thread_id);
    let permit = previous.semaphore.try_acquire().expect("hold gate");
    registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                ..Default::default()
            },
        )
        .expect("reserve metadata")
        .commit()
        .expect("commit");
    let current = registry.mailbox_submission(thread_id);
    assert!(!Arc::ptr_eq(&previous, &current));
    assert!(Arc::ptr_eq(&previous.semaphore, &current.semaphore));
    assert!(!registry.submission_is_current(thread_id, &previous));
    assert!(registry.submission_is_current(thread_id, &current));
    assert!(current.semaphore.try_acquire().is_err());
    drop(permit);
    assert!(current.semaphore.try_acquire().is_ok());
    registry.release_spawned_thread(thread_id);
    assert!(registry.reserve_spawn_slot(/*max_threads*/ Some(1)).is_ok());
}

#[test]
fn failed_stale_commit_preserves_a_later_path_reservation() {
    let registry = Arc::new(AgentRegistry::default());
    let thread_id = ThreadId::new();
    let path = AgentPath::try_from("/root/shared").expect("path");
    let replacement = registry
        .reserve_agent_metadata_replacement(
            thread_id,
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(path.clone()),
                ..Default::default()
            },
        )
        .expect("reserve metadata");
    // Replace the reservation token, modeling a newer transaction owning the same path.
    let newer_reservation = Arc::new(());
    registry
        .active_agents
        .lock()
        .expect("registry")
        .metadata_reservations
        .insert(path.to_string(), Arc::clone(&newer_reservation));
    assert!(replacement.commit().is_err());
    let active_agents = registry.active_agents.lock().expect("registry");
    assert!(
        active_agents
            .metadata_reservations
            .get(path.as_str())
            .is_some_and(|token| Arc::ptr_eq(token, &newer_reservation))
    );
    assert!(active_agents.thread_paths.is_empty());
    assert!(active_agents.agent_tree.is_empty());
    assert_eq!(registry.total_count.load(Ordering::Acquire), 0);
}
