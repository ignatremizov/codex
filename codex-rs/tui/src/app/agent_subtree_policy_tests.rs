use super::AgentNavigationState;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[test]
fn cached_adapter_preserves_shared_precedence_and_cycle_rules() {
    let root = ThreadId::new();
    let supervisor = ThreadId::new();
    let left = ThreadId::new();
    let right = ThreadId::new();
    let outside = ThreadId::new();
    let mut navigation = AgentNavigationState::default();
    for (child, parent) in [(supervisor, root), (left, supervisor), (right, supervisor)] {
        navigation.set_parent_thread_id(child, Some(parent));
    }
    navigation.set_subtree_messaging(root, /*enabled*/ false);
    navigation.set_subtree_messaging(supervisor, /*enabled*/ true);
    assert_eq!(
        [
            navigation.inherited_messaging(left, right),
            navigation.inherited_messaging(left, root),
            navigation.inherited_messaging(root, right),
            navigation.inherited_messaging(left, outside),
        ],
        [Some(true), Some(false), None, None],
    );
    navigation.set_subtree_messaging(supervisor, /*enabled*/ false);
    assert_eq!(navigation.inherited_messaging(left, right), Some(false));
    navigation.set_parent_thread_id(root, Some(supervisor));
    assert_eq!(
        [
            navigation.inherited_messaging(left, outside),
            navigation.inherited_messaging(supervisor, supervisor),
            navigation.inherited_messaging(root, right),
        ],
        [None, None, None],
    );
}
