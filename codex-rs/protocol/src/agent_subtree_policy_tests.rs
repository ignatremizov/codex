use super::*;
use pretty_assertions::assert_eq;

#[test]
fn nearest_shared_policy_controls_peers_and_tracks_parent_changes() {
    let root = ThreadId::new();
    let supervisor = ThreadId::new();
    let left = ThreadId::new();
    let right = ThreadId::new();
    let outside = ThreadId::new();
    let mut parents = HashMap::from([
        (supervisor, root),
        (left, supervisor),
        (right, supervisor),
        (outside, root),
    ]);
    let mut policies = HashMap::from([(root, false), (supervisor, true)]);
    assert_eq!(
        [
            inherited_subtree_policy(left, right, &parents, |id| policies.get(&id).copied()),
            inherited_subtree_policy(right, left, &parents, |id| policies.get(&id).copied()),
            inherited_subtree_policy(left, outside, &parents, |id| policies.get(&id).copied()),
            inherited_subtree_policy(left, supervisor, &parents, |id| policies.get(&id).copied()),
        ],
        [Some(true), Some(true), Some(false), Some(true)],
    );
    policies.remove(&supervisor);
    assert_eq!(
        inherited_subtree_policy(left, right, &parents, |id| policies.get(&id).copied()),
        Some(false),
    );
    parents.remove(&right);
    assert_eq!(
        inherited_subtree_policy(left, right, &parents, |id| policies.get(&id).copied()),
        None,
    );
}

#[test]
fn downward_dispatch_does_not_consult_policy_even_for_nested_descendants() {
    let root = ThreadId::new();
    let supervisor = ThreadId::new();
    let worker = ThreadId::new();
    let parents = HashMap::from([(supervisor, root), (worker, supervisor)]);
    for (sender, recipient) in [(root, supervisor), (root, worker), (supervisor, worker)] {
        assert_eq!(
            inherited_subtree_policy::<bool>(sender, recipient, &parents, |_| {
                panic!("downward dispatch must not consult inherited policy")
            }),
            None,
        );
    }
    assert_eq!(
        [
            is_agent_descendant(worker, root, &parents),
            is_agent_descendant(root, worker, &parents),
            is_agent_descendant(root, root, &parents),
        ],
        [true, false, false],
    );
}

#[test]
fn missing_policy_and_disconnected_cycles_terminate_without_a_match() {
    let sender = ThreadId::new();
    let recipient = ThreadId::new();
    let first = ThreadId::new();
    let second = ThreadId::new();
    let parents = HashMap::from([(sender, first), (first, second), (second, first)]);
    assert_eq!(
        inherited_subtree_policy(sender, recipient, &parents, |_| Some(true)),
        None,
    );
    assert_eq!(
        inherited_subtree_policy(sender, second, &parents, |_| None::<bool>),
        None,
    );
    assert!(!is_agent_descendant(sender, recipient, &parents));
}

#[test]
fn cycles_preserve_first_shared_policy_and_edge_reachable_self_semantics() {
    let first = ThreadId::new();
    let second = ThreadId::new();
    let left = ThreadId::new();
    let right = ThreadId::new();
    let parents = HashMap::from([
        (first, second),
        (second, first),
        (left, first),
        (right, second),
    ]);
    let policies = HashMap::from([(first, "first"), (second, "second")]);
    assert_eq!(
        [
            inherited_subtree_policy(left, right, &parents, |id| policies.get(&id).copied()),
            inherited_subtree_policy(right, left, &parents, |id| policies.get(&id).copied()),
            inherited_subtree_policy(first, first, &parents, |id| policies.get(&id).copied()),
        ],
        [Some("first"), Some("second"), None],
    );
    assert!(is_agent_descendant(first, first, &parents));
    assert_eq!(
        inherited_subtree_policy(left, left, &parents, |id| policies.get(&id).copied()),
        Some("first"),
    );
}
