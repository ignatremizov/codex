//! Pure ancestry rules shared by authoritative messaging policy and cached UI projections.
//!
//! The caller supplies its current parent edges and policy lookup. This module stores no graph,
//! resolves no runtime identities, and does not apply directed exceptions or authorize input.

use crate::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;

/// Finds the nearest shared ancestor with a subtree policy, walking from the sender upward.
///
/// Supervisor-to-descendant task dispatch is outside inherited messaging policy and returns
/// `None`, regardless of the policy value. Missing parents terminate a walk; repeated nodes
/// terminate cycles. Policy values are opaque so callers retain their own policy representation.
/// Directed overrides and admission checks remain the caller's responsibility.
pub fn inherited_subtree_policy<T>(
    sender: ThreadId,
    recipient: ThreadId,
    parents: &HashMap<ThreadId, ThreadId>,
    mut policy_for: impl FnMut(ThreadId) -> Option<T>,
) -> Option<T> {
    if is_agent_descendant(recipient, sender, parents) {
        return None;
    }
    let mut recipient_ancestors = HashSet::new();
    let mut next = Some(recipient);
    while let Some(id) = next {
        if !recipient_ancestors.insert(id) {
            break;
        }
        next = parents.get(&id).copied();
    }
    let mut visited = HashSet::new();
    let mut next = Some(sender);
    while let Some(id) = next {
        if !visited.insert(id) {
            break;
        }
        if recipient_ancestors.contains(&id)
            && let Some(policy) = policy_for(id)
        {
            return Some(policy);
        }
        next = parents.get(&id).copied();
    }
    None
}

/// Whether following one or more parent edges reaches `ancestor`.
///
/// This follows lifecycle edges, never semantic task paths. Repeated nodes stop the search.
/// In a malformed cyclic graph a node can reach itself through an edge; that counts as ancestry
/// and keeps such a route outside inherited messaging policy.
pub fn is_agent_descendant(
    descendant: ThreadId,
    ancestor: ThreadId,
    parents: &HashMap<ThreadId, ThreadId>,
) -> bool {
    let mut next = parents.get(&descendant).copied();
    let mut seen = HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            return false;
        }
        if id == ancestor {
            return true;
        }
        next = parents.get(&id).copied();
    }
    false
}

#[cfg(test)]
#[path = "agent_subtree_policy_tests.rs"]
mod tests;
