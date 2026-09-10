//! Model-only mailbox receipts derived from existing fixed claims, never inventory.

use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_thread_store::MailboxClaim;
use codex_thread_store::MailboxInvocation;
use codex_thread_store::MailboxMessageState;
use codex_thread_store::MailboxSelection;
use codex_thread_store::MailboxSender;
use codex_thread_store::ThreadStore;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AcceptanceStatus {
    DeliveryRequested,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    status: AcceptanceStatus,
    #[serde(deserialize_with = "Option::deserialize")]
    from: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryStatus {
    Empty,
    Delivered,
    Rejected,
}

#[derive(Serialize)]
struct DeliveryCounts {
    status: DeliveryStatus,
    delivered_count: usize,
    rejected_count: usize,
}

#[derive(Serialize)]
struct Receipt<'a> {
    from: Option<String>,
    #[serde(flatten)]
    counts: &'a DeliveryCounts,
}

struct ClaimSummary {
    from: Option<String>,
    counts: Option<DeliveryCounts>,
}

impl ClaimSummary {
    fn from_claim(claim: &MailboxClaim) -> Option<Self> {
        let from = match &claim.selection {
            MailboxSelection::All => None,
            MailboxSelection::Senders(senders) => match senders.as_slice() {
                [MailboxSender::User] => Some("user".to_string()),
                [MailboxSender::Agent(sender)] => Some(sender.to_string()),
                _ => return None,
            },
        };
        let mut delivered_count = 0;
        let mut rejected_count = 0;
        let mut terminal = true;
        for member in &claim.messages {
            match member.message.state {
                MailboxMessageState::Consumed => delivered_count += 1,
                MailboxMessageState::Rejected => rejected_count += 1,
                MailboxMessageState::Pending | MailboxMessageState::Claimed => terminal = false,
            }
        }
        let counts = terminal.then_some(DeliveryCounts {
            status: if delivered_count > 0 {
                DeliveryStatus::Delivered
            } else if rejected_count > 0 {
                DeliveryStatus::Rejected
            } else {
                DeliveryStatus::Empty
            },
            delivered_count,
            rejected_count,
        });
        Some(Self { from, counts })
    }
}

/// Rewrites only matched check_mail results in a disposable request. Claim lookups
/// are read-only and deduplicated for this request, including missing claims.
/// Canonical result/payload order, UUIDs, and delivery acknowledgement stay untouched.
pub(crate) async fn project_check_mail_results(
    items: &mut [ResponseItem],
    receiver: ThreadId,
    store: &dyn ThreadStore,
    refs: &HashMap<ThreadId, u64>,
) -> CodexResult<()> {
    let mut calls = HashMap::new();
    let mut summaries = HashMap::new();
    for item in items {
        let Some(turn_id) = item.turn_id().map(str::to_owned) else {
            continue;
        };
        if let ResponseItem::FunctionCall {
            name,
            namespace,
            call_id,
            ..
        } = item
        {
            let eligible = name == "check_mail" && namespace.as_deref() == Some("multi_agent_v1");
            calls
                .entry((turn_id, call_id.clone()))
                .and_modify(|valid| *valid &= eligible)
                .or_insert(eligible);
            continue;
        }
        let ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            output,
            ..
        } = item
        else {
            continue;
        };
        let key = (turn_id, call_id.clone());
        if calls.get(&key) != Some(&true) || output.success == Some(false) {
            continue;
        }
        let FunctionCallOutputBody::Text(text) = &mut output.body else {
            continue;
        };
        let Ok(acceptance) = serde_json::from_str::<Acceptance>(text) else {
            continue;
        };
        let summary = match summaries.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let claim = store
                    .lookup_mailbox_claim(MailboxInvocation {
                        receiver_thread_id: receiver,
                        turn_id: entry.key().0.clone(),
                        tool_call_id: entry.key().1.clone(),
                    })
                    .await
                    .map_err(|error| {
                        CodexErr::Fatal(format!(
                            "failed to read fixed check_mail claim for model projection: {error}"
                        ))
                    })?;
                entry.insert(claim.as_ref().and_then(ClaimSummary::from_claim))
            }
        };
        let Some(summary) = summary else {
            continue;
        };
        if acceptance.from != summary.from {
            continue;
        }
        let from = summary.from.as_ref().map(|sender| {
            ThreadId::from_string(sender)
                .ok()
                .and_then(|id| refs.get(&id))
                .map(u64::to_string)
                .unwrap_or_else(|| sender.clone())
        });
        let projected = if let Some(counts) = &summary.counts {
            serde_json::to_value(Receipt { from, counts })
        } else {
            serde_json::to_value(Acceptance { from, ..acceptance })
        };
        *text = projected
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to serialize check_mail model receipt: {error}"
                ))
            })?
            .to_string();
    }
    Ok(())
}

#[cfg(test)]
#[path = "check_mail_result_tests.rs"]
mod tests;
