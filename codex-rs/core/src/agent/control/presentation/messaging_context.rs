//! Model-only projection of current messaging policy. Stored notices remain audit evidence.

use crate::context::AGENT_MESSAGING_POLICY_PREFIX as NOTICE_PREFIX;
use crate::context::AgentContextIdentity;
pub(super) use crate::context::AgentMessagingPolicyNotice as PermissionNotice;
use crate::context::ContextualUserFragment;
use crate::context::LegacyAgentReplyRoute;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::BTreeMap;
use std::collections::HashSet;

pub(super) fn identity_label(identity: &AgentContextIdentity) -> String {
    match identity {
        AgentContextIdentity::V1 {
            agent_id,
            agent_ref,
            nickname,
            task_path,
        } => match (nickname, agent_ref, task_path) {
            (Some(nickname), Some(agent_ref), _) => format!("{nickname} ({agent_ref})"),
            (_, _, Some(task_path)) => task_path.clone(),
            (_, Some(agent_ref), _) => format!("ref {agent_ref}"),
            (Some(nickname), _, _) => nickname.clone(),
            (None, None, None) => agent_id.to_string(),
        },
        AgentContextIdentity::V2 { agent_path, .. } => agent_path.to_string(),
        AgentContextIdentity::Canonical { agent_id } => agent_id.to_string(),
    }
}

pub(super) fn notice_parts(item: &ResponseItem) -> Option<(&str, &str)> {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = item
    else {
        return None;
    };
    if role != "developer" {
        return None;
    }
    let [kind] = metadata.content_item_kinds.as_deref()? else {
        return None;
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        return None;
    };
    Some((kind.0.strip_prefix(NOTICE_PREFIX)?, text))
}

#[derive(Default)]
pub(crate) struct MessagingContextSnapshot {
    pub(super) notices: Vec<ResponseItem>,
    pub(in crate::agent::control) allowed_targets: HashSet<ThreadId>,
}

impl MessagingContextSnapshot {
    pub(crate) fn reconcile(&self, items: &mut Vec<ResponseItem>) {
        let mut annotated = std::mem::take(items)
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect();
        self.reconcile_annotated(&mut annotated);
        *items = annotated
            .into_iter()
            .map(ResponseItemEnvelope::into_item)
            .collect();
    }

    pub(crate) fn reconcile_annotated(&self, items: &mut Vec<ResponseItemEnvelope>) {
        let baseline = (self.notices.is_empty()
            && items.iter().any(|item| {
                notice_parts(&item.item).is_some()
                    || LegacyAgentReplyRoute::persistent_agent_id(&item.item).is_some()
            }))
        .then(|| {
            ContextualUserFragment::into(PermissionNotice {
                key: "baseline".to_string(),
                text: "No live user messaging overrides are active.".to_string(),
            })
        });
        let desired: BTreeMap<_, _> = self
            .notices
            .iter()
            .chain(baseline.iter())
            .filter_map(|item| notice_parts(item).map(|(key, text)| (key, (text, item))))
            .collect();
        // Retain the latest identical notice in its original location and with its exact ID
        // and harness metadata. Do not create fresh message IDs on each sampling request.
        let mut retained = BTreeMap::new();
        for (index, envelope) in items.iter().enumerate() {
            if let Some((key, text)) = notice_parts(&envelope.item)
                && desired
                    .get(key)
                    .is_some_and(|(expected, _)| text == *expected)
            {
                retained.insert(key.to_string(), index);
            }
        }
        let mut index = 0;
        items.retain_mut(|envelope| {
            let keep = notice_parts(&envelope.item)
                .is_none_or(|(key, _)| retained.get(key) == Some(&index));
            index += 1;
            keep
        });
        for (key, (_, item)) in desired {
            if !retained.contains_key(key) {
                items.push(ResponseItemEnvelope::new(item.clone()));
            }
        }
    }
}

#[cfg(test)]
#[path = "messaging_context_tests.rs"]
mod tests;
