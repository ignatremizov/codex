use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;

/// One host-classified canonical inventory, selected after ordinary history reconstruction.
/// A present inventory with an empty promotion list explicitly supersedes earlier inventories.
#[derive(Clone, Debug)]
pub struct RestoredSkillsInventory(ResponseItemEnvelope);

impl RestoredSkillsInventory {
    pub fn from_envelope(envelope: &ResponseItemEnvelope) -> Option<Self> {
        if envelope
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.client_authored)
        {
            return None;
        }
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        else {
            return None;
        };
        if role != "developer" {
            return None;
        }
        let kinds = internal_chat_message_metadata_passthrough
            .as_ref()?
            .content_item_kinds
            .as_ref()?;
        if kinds.len() != content.len() {
            return None;
        }
        // The durable contribution emits its inventory separately. Compound catalogs
        // are not an authority boundary: retain them as ordinary history, not promotion state.
        if content.len() != 1 {
            return None;
        }
        let valid = content.iter().zip(kinds).all(|(content, kind)| {
            let ContentItem::InputText { text } = content else {
                return false;
            };
            if kind.0 != "skills.catalog" {
                return false;
            }
            let Some(body) = text
                .trim()
                .strip_prefix(SKILLS_INSTRUCTIONS_OPEN_TAG)
                .and_then(|body| body.strip_suffix(SKILLS_INSTRUCTIONS_CLOSE_TAG))
            else {
                return false;
            };
            let Some(body) = body.trim_start().strip_prefix("<promoted_skills>") else {
                return false;
            };
            let Some((metadata, _)) = body.split_once("</promoted_skills>") else {
                return false;
            };
            if metadata.len() > 8 * 1024 {
                return false;
            }
            matches!(serde_json::from_str::<serde_json::Value>(metadata),
                Ok(serde_json::Value::Array(identities)) if identities.len() <= 16
                    && identities.iter().all(valid_identity))
        });
        valid.then(|| Self(envelope.clone()))
    }

    pub fn envelope(&self) -> &ResponseItemEnvelope {
        &self.0
    }

    pub fn item(&self) -> &ResponseItem {
        &self.0.item
    }
}

fn valid_identity(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let required = [
        ("authorityKindHex", 128),
        ("authorityIdHex", 256),
        ("packageHex", 256),
    ];
    required.iter().all(|(name, bound)| {
        object
            .get(*name)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| valid_hex(value, *bound))
    }) && object.get("resourceHex").is_none_or(|value| {
        value.is_null()
            || value
                .as_str()
                .is_some_and(|value| valid_hex(value, /*decoded_limit*/ 512))
    })
}

fn valid_hex(value: &str, decoded_limit: usize) -> bool {
    value.len() <= decoded_limit * 2
        && value.len().is_multiple_of(2)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
