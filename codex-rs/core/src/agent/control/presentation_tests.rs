use super::*;
use pretty_assertions::assert_eq;

fn identity() -> SessionPresentationId {
    SessionPresentationId::new(ThreadId::new(), Uuid::now_v7())
}

fn terminal(
    state: &WaitAgentPresentations,
    parent: SessionPresentationId,
    child: SessionPresentationId,
) -> AgentTerminalPresentation {
    let mut state = state.state.lock().unwrap();
    let waits = state
        .waits
        .iter()
        .filter_map(|(id, wait)| {
            (wait.parent == parent
                && wait
                    .children
                    .as_ref()
                    .is_none_or(|children| children.contains(&child.thread_id)))
            .then_some(*id)
        })
        .collect::<HashSet<_>>();
    let inner = Arc::new(Terminal {
        parent,
        child,
        parent_thread: Mutex::new(None),
        context_id: new_sub_agent_completion_context_response_item_id(),
        status: AgentStatus::Completed(Some("captured result".to_owned())),
        presentation: CompletionPresentation {
            item: TurnItem::AgentMessage(
                codex_protocol::protocol::sub_agent_completion_item(
                    "/root/child",
                    &AgentStatus::Completed(Some("captured result".to_owned())),
                )
                .expect("terminal presentation"),
            ),
            history_only_turn_id: Uuid::now_v7().to_string(),
        },
        accepted: Mutex::new(None),
        ownership: Mutex::new(Ownership {
            waits: waits.clone(),
            presenter: None,
            background_claimed: false,
            committed: false,
        }),
        changed: Notify::new(),
    });
    for id in waits {
        state
            .waits
            .get_mut(&id)
            .unwrap()
            .terminals
            .push(Arc::downgrade(&inner));
    }
    state
        .contexts
        .insert(inner.context_id.clone(), Arc::clone(&inner));
    AgentTerminalPresentation { inner }
}

#[tokio::test]
async fn committed_wait_suppresses_background_presentation() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let wait = state.register(parent, Some(HashSet::from([child.thread_id])));
    let terminal = terminal(&state, parent, child);
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            child.thread_id,
            AgentStatus::Completed(Some("captured result".to_owned()))
        ),])
    );
    commit.commit();
    assert!(terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn dropped_frozen_wait_releases_background_delivery() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let wait = state.register(parent, None);
    let terminal = terminal(&state, parent, child);
    drop(wait.freeze_for_children([child.thread_id]));
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn concurrent_waits_cannot_both_claim_the_same_terminal() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let child = identity();
    let first = state.register(parent, None);
    let second = state.register(parent, None);
    let terminal = terminal(&state, parent, child);
    let first = first.freeze_for_children([child.thread_id]);
    let second = second.freeze_for_children([child.thread_id]);
    assert_eq!(second.completion_presentation_agent_ids(), None);
    second.commit();
    drop(first);
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn late_mailbox_wait_cannot_claim_background_already_selected() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let terminal = terminal(&state, parent, identity());
    assert!(!terminal.wait_owns_presentation().await);
    let wait = state.register(parent, None);
    let commit = wait
        .freeze_for_mailbox_response_item_ids(&[terminal.completion_context_response_item_id()]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            terminal.inner.child.thread_id,
            AgentStatus::Completed(Some("captured result".to_owned())),
        )])
    );
    assert_eq!(commit.completion_presentation_agent_ids(), None);
    commit.commit();
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn replacement_runtime_cannot_claim_prior_mailbox_terminal() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let terminal = terminal(&state, parent, identity());
    let replacement = SessionPresentationId::new(parent.thread_id, Uuid::now_v7());
    let wait = state.register(replacement, None);
    let commit = wait
        .freeze_for_mailbox_response_item_ids(&[terminal.completion_context_response_item_id()]);
    assert_eq!(commit.agent_states(), HashMap::new());
    commit.commit();
    assert!(!terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn freeze_releases_unselected_children() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let selected = identity();
    let wait = state.register(parent, None);
    let selected_terminal = terminal(&state, parent, selected);
    let unselected_terminal = terminal(&state, parent, identity());
    let commit = wait.freeze_for_children([selected.thread_id]);
    assert!(!unselected_terminal.wait_owns_presentation().await);
    commit.commit();
    assert!(selected_terminal.wait_owns_presentation().await);
}

#[tokio::test]
async fn mailbox_wait_displays_all_matches_but_owns_only_unclaimed_terminals() {
    let state = Arc::new(WaitAgentPresentations::default());
    let parent = identity();
    let old = terminal(&state, parent, identity());
    assert!(!old.wait_owns_presentation().await);
    let wait = state.register(parent, None);
    let current = terminal(&state, parent, identity());
    let commit = wait.freeze_for_mailbox_response_item_ids(&[
        old.completion_context_response_item_id(),
        current.completion_context_response_item_id(),
    ]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([
            (old.inner.child.thread_id, old.inner.status.clone()),
            (current.inner.child.thread_id, current.inner.status.clone()),
        ])
    );
    assert_eq!(
        commit.completion_presentation_agent_ids(),
        Some(vec![current.inner.child.thread_id])
    );
    commit.commit();
    assert!(!old.wait_owns_presentation().await);
    assert!(current.wait_owns_presentation().await);
}
