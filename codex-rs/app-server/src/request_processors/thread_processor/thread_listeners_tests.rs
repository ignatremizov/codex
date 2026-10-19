use super::inherited_connection_ids;
use crate::outgoing_message::ConnectionId;
use crate::thread_state::ConnectionCapabilities;
use crate::thread_state::ThreadStateManager;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

#[tokio::test]
async fn created_threads_inherit_only_current_parent_viewers() {
    let manager = ThreadStateManager::new();
    let parent = ThreadId::new();
    let unrelated = ThreadId::new();
    let owner = ConnectionId(1);
    let shared_viewer = ConnectionId(2);
    let other_owner = ConnectionId(3);
    for connection in [owner, shared_viewer, other_owner] {
        manager
            .connection_initialized(connection, ConnectionCapabilities::default())
            .await;
    }
    for (thread, connection) in [
        (parent, owner),
        (parent, shared_viewer),
        (unrelated, other_owner),
    ] {
        assert!(
            manager
                .try_add_connection_to_thread(thread, connection)
                .await
        );
    }
    assert_eq!(
        inherited_connection_ids(&manager, /*parent_thread_id*/ None).await,
        Vec::<ConnectionId>::new()
    );
    assert_eq!(
        inherited_connection_ids(&manager, Some(parent))
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([owner, shared_viewer])
    );

    manager.remove_connection(shared_viewer).await;
    assert_eq!(
        inherited_connection_ids(&manager, Some(parent)).await,
        vec![owner]
    );
    assert_eq!(
        inherited_connection_ids(&manager, Some(unrelated)).await,
        vec![other_owner]
    );
}
