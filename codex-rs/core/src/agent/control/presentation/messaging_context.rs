//! Model-only projection of current messaging policy. Stored notices remain audit evidence.

use crate::context::AgentContextIdentity;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashSet;

const NOTICE_PREFIX: &str = "multi_agent.messaging_policy.";

pub(super) struct PermissionNotice {
    pub(super) key: String,
    pub(super) text: String,
}

impl ContextualUserFragment for PermissionNotice {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(format!("{NOTICE_PREFIX}{}", self.key))
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("", "")
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

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

pub(super) fn has_route_hint(item: &ResponseItem) -> bool {
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = item
    else {
        return false;
    };
    metadata.content_item_kinds.as_ref().is_some_and(|kinds| {
        kinds.iter().any(|kind| {
            kind.0 == "multi_agent.agent_reply_route"
                || kind.0 == codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND
        })
    })
}

#[derive(Default)]
pub(crate) struct MessagingContextSnapshot {
    pub(super) notices: Vec<ResponseItem>,
    pub(super) allowed_targets: HashSet<ThreadId>,
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
                    || crate::context::AgentReplyRoute::persistent_agent_id(&item.item).is_some()
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
            if keep {
                self.reconcile_route_hint(&mut envelope.item);
            }
            keep
        });
        for (key, (_, item)) in desired {
            if !retained.contains_key(key) {
                items.push(ResponseItemEnvelope::new(item.clone()));
            }
        }
    }

    fn reconcile_route_hint(&self, item: &mut ResponseItem) {
        if !has_route_hint(item) {
            return;
        }
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return;
        };
        let Some(kinds) = &metadata.content_item_kinds else {
            return;
        };
        for (content, kind) in content.iter_mut().zip(kinds) {
            if kind.0 != "multi_agent.agent_reply_route"
                && kind.0 != codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND
            {
                continue;
            }
            let ContentItem::InputText { text } = content else {
                continue;
            };
            let Some(body) = text
                .trim()
                .strip_prefix("<agent_reply_route>")
                .and_then(|body| body.strip_suffix("</agent_reply_route>"))
            else {
                continue;
            };
            let Ok(mut fields) = serde_json::from_str::<Value>(body) else {
                continue;
            };
            let target = fields["agent_id"]
                .as_str()
                .and_then(|id| ThreadId::from_string(id).ok());
            if target.is_some_and(|target| self.allowed_targets.contains(&target)) {
                continue;
            }
            fields["send_input"] = Value::String("not_authorized".to_string());
            *text = format!("<agent_reply_route>\n{fields}\n</agent_reply_route>");
        }
    }
}

#[cfg(test)]
#[path = "messaging_context_tests.rs"]
mod tests;
