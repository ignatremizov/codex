use super::*;

fn completed_status() -> AgentStatus {
    AgentStatus::Completed(Some("done".to_string()))
}

fn session_presentation_id(thread_id: ThreadId) -> SessionPresentationId {
    SessionPresentationId::new(
        thread_id,
        uuid::Uuid::parse_str(&thread_id.to_string()).expect("thread UUID"),
    )
}

#[tokio::test]
async fn active_targeted_wait_commits_terminal_presentation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn wait_registration_cannot_cross_terminal_publication() {
    let control = Arc::new(AgentControl::default());
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
    let (registration_started_tx, registration_started_rx) = std::sync::mpsc::sync_channel(1);
    let record_control = Arc::clone(&control);
    let recorder = std::thread::spawn(move || {
        record_control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {
                    published_tx.send(()).expect("publication signal");
                    registration_started_rx
                        .recv()
                        .expect("registration start signal");
                },
            )
            .expect("direct presentation")
    });
    published_rx.recv().expect("terminal should publish");
    let wait_control = Arc::clone(&control);
    let waiter = std::thread::spawn(move || {
        registration_started_tx
            .send(())
            .expect("registration start signal");
        wait_control.register_targeted_wait_agent_presentation(parent, &[child_thread_id])
    });

    let presentation = recorder.join().expect("recorder should finish");
    waiter
        .join()
        .expect("wait registration should finish")
        .freeze_for_children([child_thread_id])
        .commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_targeted_wait_does_not_claim_previous_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_mailbox_wait_reports_previous_terminal_without_claiming_its_presentation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    control.authorize_pending_completion_context(parent, &presentation);
    let response_item_ids = vec![presentation.completion_context_response_item_id()];
    let wait = control.register_any_child_wait_agent_presentation(parent);

    let commit = wait.freeze_for_mailbox_response_item_ids(&response_item_ids);

    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    commit.commit();
    assert!(!presentation.wait_owns_presentation().await);
}

#[test]
fn cancelled_late_mailbox_wait_preserves_pending_terminal_state() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    control.authorize_pending_completion_context(parent, &presentation);
    let response_item_ids = vec![presentation.completion_context_response_item_id()];
    let cancelled_wait = control.register_any_child_wait_agent_presentation(parent);
    let cancelled_commit = cancelled_wait.freeze_for_mailbox_response_item_ids(&response_item_ids);
    drop(cancelled_commit);
    let next_wait = control.register_any_child_wait_agent_presentation(parent);

    assert_eq!(
        next_wait
            .freeze_for_mailbox_response_item_ids(&response_item_ids)
            .agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
}

#[test]
fn mailbox_wait_uses_latest_queued_generation_for_a_child() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let first = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("first presentation");
    let latest_status = AgentStatus::Errored("latest failed".to_string());
    let latest = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-2",
            latest_status.clone(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("latest presentation");
    control.authorize_pending_completion_context(parent, &first);
    control.authorize_pending_completion_context(parent, &latest);
    let response_item_ids = vec![
        first.completion_context_response_item_id(),
        latest.completion_context_response_item_id(),
    ];
    let wait = control.register_any_child_wait_agent_presentation(parent);

    assert_eq!(
        wait.freeze_for_mailbox_response_item_ids(&response_item_ids)
            .agent_states(),
        HashMap::from([(child_thread_id, latest_status)])
    );
}

#[tokio::test]
async fn targeted_wait_after_previous_terminal_can_claim_next_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let previous = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("previous presentation");
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let next = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("next presentation");

    wait.freeze_for_children([child_thread_id]).commit();

    assert!(!previous.wait_owns_presentation().await);
    assert!(next.wait_owns_presentation().await);
}

#[tokio::test]
async fn cancelled_wait_releases_terminal_to_background() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");

    drop(wait);

    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn frozen_wait_does_not_claim_later_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let first_child_thread_id = ThreadId::new();
    let second_child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(
        parent,
        &[first_child_thread_id, second_child_thread_id],
    );
    let first = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(first_child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("first presentation");
    let commit = wait.freeze_for_children([first_child_thread_id]);
    let second = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(second_child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("second presentation");

    commit.commit();

    assert!(first.wait_owns_presentation().await);
    assert!(!second.wait_owns_presentation().await);
}

#[tokio::test]
async fn any_child_wait_claims_only_terminal_messages_already_in_the_mailbox() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let delivered_child_thread_id = ThreadId::new();
    let blocked_child_thread_id = ThreadId::new();
    let wait = control.register_any_child_wait_agent_presentation(parent);
    let delivered = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(delivered_child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let blocked = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(blocked_child_thread_id),
            "turn-2",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let delivered_response_item_ids = vec![delivered.completion_context_response_item_id()];

    let commit = wait.freeze_for_mailbox_response_item_ids(&delivered_response_item_ids);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(delivered_child_thread_id, completed_status())])
    );
    commit.commit();

    assert!(delivered.wait_owns_presentation().await);
    assert!(!blocked.wait_owns_presentation().await);
}

#[test]
fn finishing_old_watcher_terminal_preserves_new_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-old",
        AgentStatus::Shutdown,
        TerminalPresentationDelivery::Watcher,
        || {},
    );
    let old = control
        .take_watcher_terminal_presentation(session_presentation_id(child_thread_id))
        .expect("old watcher terminal");
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-new",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    control.finish_watcher_terminal_presentation(
        session_presentation_id(child_thread_id),
        &old.turn_id,
    );

    let new = control
        .take_watcher_terminal_presentation(session_presentation_id(child_thread_id))
        .expect("new watcher terminal");
    assert_eq!(new.turn_id, "turn-new");
    assert_eq!(new.status, completed_status());
}

#[test]
fn completion_context_response_item_ids_require_authorization() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(ThreadId::new()),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();

    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    control.authorize_pending_completion_context(parent, &presentation);
    let unrelated_parent_thread_id = ThreadId::new();
    let unrelated_parent = session_presentation_id(unrelated_parent_thread_id);
    let unrelated_wait = control.register_any_child_wait_agent_presentation(unrelated_parent);
    assert!(
        unrelated_wait
            .freeze_for_mailbox_response_item_ids(std::slice::from_ref(&id))
            .agent_states()
            .is_empty()
    );
    assert!(!control.claim_completion_context_response_item_id(unrelated_parent, &id));
    assert!(control.claim_completion_context_response_item_id(parent, &id));
    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    let wait = control
        .register_any_child_wait_agent_presentation(session_presentation_id(ThreadId::new()));
    assert!(
        wait.freeze_for_mailbox_response_item_ids(&[id])
            .agent_states()
            .is_empty()
    );
}

#[test]
fn claimed_completion_context_remains_available_to_a_later_wait() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let response_item_id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(parent, &presentation);

    assert!(control.claim_completion_context_response_item_id(parent, &response_item_id));
    let commit = control
        .register_any_child_wait_agent_presentation(parent)
        .freeze_for_mailbox_response_item_ids(&[response_item_id]);

    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    commit.commit();
}

#[test]
fn removing_parent_revokes_unclaimed_completion_context() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = session_presentation_id(parent_thread_id);
    let child_thread_id = ThreadId::new();
    let presentation = control
        .record_agent_terminal_presentation(
            session_presentation_id(parent_thread_id),
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(parent, &presentation);

    control.clear_completion_contexts_for_session(parent);

    assert!(!control.claim_completion_context_response_item_id(parent, &id));
    let wait = control.register_any_child_wait_agent_presentation(parent);
    assert!(
        wait.freeze_for_mailbox_response_item_ids(&[id])
            .agent_states()
            .is_empty()
    );
}

#[test]
fn old_session_cleanup_preserves_new_session_authorization() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let presentation = control
        .record_agent_terminal_presentation(
            new_parent,
            session_presentation_id(ThreadId::new()),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let id = presentation.completion_context_response_item_id();
    control.authorize_pending_completion_context(new_parent, &presentation);

    control.clear_completion_contexts_for_session(old_parent);

    assert_eq!(
        control
            .register_any_child_wait_agent_presentation(new_parent)
            .freeze_for_mailbox_response_item_ids(std::slice::from_ref(&id))
            .agent_states(),
        HashMap::from([(presentation.inner.child.thread_id, completed_status())])
    );
    assert!(control.claim_completion_context_response_item_id(new_parent, &id));
}

#[tokio::test]
async fn removed_parent_session_wait_cannot_claim_new_parent_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let old_wait =
        control.register_targeted_wait_agent_presentation(old_parent, &[child_thread_id]);

    control.clear_wait_agent_presentations_for_session(old_parent);
    let stale_wait =
        control.register_targeted_wait_agent_presentation(old_parent, &[child_thread_id]);
    let new_wait =
        control.register_targeted_wait_agent_presentation(new_parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            new_parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let old_commit = old_wait.freeze_for_children([child_thread_id]);
    let stale_commit = stale_wait.freeze_for_children([child_thread_id]);
    let new_commit = new_wait.freeze_for_children([child_thread_id]);

    assert!(old_commit.agent_states().is_empty());
    assert!(stale_commit.agent_states().is_empty());
    assert_eq!(
        new_commit.agent_states(),
        HashMap::from([(child_thread_id, completed_status())])
    );
    old_commit.commit();
    stale_commit.commit();
    new_commit.commit();
    assert!(presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn new_parent_session_wait_cannot_claim_old_parent_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let old_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let new_parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(new_parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            old_parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let commit = wait.freeze_for_children([child_thread_id]);

    assert!(commit.agent_states().is_empty());
    commit.commit();
    assert!(!presentation.wait_owns_presentation().await);
}

#[tokio::test]
async fn removing_parent_session_invalidates_a_frozen_wait_commit() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let parent = SessionPresentationId::new(parent_thread_id, uuid::Uuid::now_v7());
    let child_thread_id = ThreadId::new();
    let wait = control.register_targeted_wait_agent_presentation(parent, &[child_thread_id]);
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            session_presentation_id(child_thread_id),
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("direct presentation");
    let commit = wait.freeze_for_children([child_thread_id]);

    control.clear_wait_agent_presentations_for_session(parent);
    commit.commit();

    assert!(!presentation.wait_owns_presentation().await);
}

#[test]
fn completion_watcher_registration_is_scoped_to_session_instance() {
    let control = AgentControl::default();
    let thread_id = ThreadId::new();
    let parent = session_presentation_id(ThreadId::new());
    let first_session = SessionPresentationId::new(thread_id, uuid::Uuid::now_v7());
    let reloaded_session = SessionPresentationId::new(thread_id, uuid::Uuid::now_v7());
    let first_registration = control
        .register_completion_watcher(first_session, parent)
        .expect("first registration");

    assert!(
        control
            .register_completion_watcher(first_session, parent)
            .is_none()
    );
    assert!(
        control
            .register_completion_watcher(reloaded_session, parent)
            .is_some()
    );
    drop(first_registration);
    assert!(
        control
            .register_completion_watcher(first_session, parent)
            .is_some()
    );
}

#[test]
fn terminal_publication_reserves_registered_parent_delivery() {
    let control = AgentControl::default();
    let parent = session_presentation_id(ThreadId::new());
    let child = session_presentation_id(ThreadId::new());
    let admission = Arc::new(crate::session::SubmissionAdmission::default());
    let _watcher_registration = control
        .register_completion_watcher_with_admission(child, parent, &admission)
        .expect("completion watcher registration");
    let presentation = control
        .record_agent_terminal_presentation(
            parent,
            child,
            "turn-1",
            completed_status(),
            TerminalPresentationDelivery::Direct,
            || {},
        )
        .expect("terminal presentation");

    assert!(presentation.take_accepted_completion_delivery().is_some());
    assert!(presentation.take_accepted_completion_delivery().is_none());
}

#[test]
fn old_child_session_cannot_consume_reloaded_child_terminal() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let old_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let reloaded_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        reloaded_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    assert!(
        control
            .take_watcher_terminal_presentation(old_child)
            .is_none()
    );
    assert!(
        control
            .take_watcher_terminal_presentation(reloaded_child)
            .is_some()
    );
}

#[test]
fn closing_child_clears_terminal_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );

    control.release_spawned_thread(SpawnedThreadRelease::Session(session_presentation_id(
        child_thread_id,
    )));

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_some()
    );
}

#[test]
fn releasing_old_child_session_preserves_new_terminal_generation() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let old_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let new_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        old_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        new_child,
        "turn-1",
        completed_status(),
        TerminalPresentationDelivery::Direct,
        || {},
    );

    control.release_spawned_thread(SpawnedThreadRelease::Session(old_child));

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                old_child,
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_some()
    );
    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                new_child,
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_none()
    );
}

#[test]
fn one_child_turn_queues_only_one_delivery_path() {
    let control = AgentControl::default();
    let parent_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let _ = control.record_agent_terminal_presentation(
        session_presentation_id(parent_thread_id),
        session_presentation_id(child_thread_id),
        "turn-1",
        AgentStatus::Errored("failed".to_string()),
        TerminalPresentationDelivery::Watcher,
        || {},
    );

    assert!(
        control
            .record_agent_terminal_presentation(
                session_presentation_id(parent_thread_id),
                session_presentation_id(child_thread_id),
                "turn-1",
                completed_status(),
                TerminalPresentationDelivery::Direct,
                || {},
            )
            .is_none()
    );
    let terminal = control
        .take_watcher_terminal_presentation(session_presentation_id(child_thread_id))
        .expect("watcher terminal");
    assert_eq!(terminal.status, AgentStatus::Errored("failed".to_string()));
    assert!(
        control
            .take_watcher_terminal_presentation(session_presentation_id(child_thread_id))
            .is_none()
    );
}
