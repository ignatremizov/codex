//! Exact rollback coordinates are decoded source-record indexes, independent of ordinals.

use std::collections::HashSet;
use std::io;
use std::io::BufRead;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_rollout::RolloutItem;

pub(super) struct ExactRollbackProjection {
    pub(super) removed_offsets: HashSet<u64>,
    pub(super) contains_exact_marker: bool,
}

impl ExactRollbackProjection {
    pub(super) fn read(
        reader: &mut impl BufRead,
        start_offset: u64,
        is_cancelled: impl Fn() -> bool,
    ) -> io::Result<Self> {
        let mut items = Vec::new();
        let mut offsets = Vec::new();
        let mut offset = start_offset;
        let mut contains_exact_marker = false;
        let mut record = Vec::new();
        loop {
            if is_cancelled() {
                return Err(io::Error::other("rollout read cancelled"));
            }
            record.clear();
            if reader.read_until(b'\n', &mut record)? == 0 || !record.ends_with(b"\n") {
                break;
            }
            let line = serde_json::from_slice(&record)
                .ok()
                .and_then(|value| codex_rollout::decode_canonical_rollout_line(value).ok())
                .flatten();
            if let Some(line) = line {
                contains_exact_marker |= matches!(
                    &line.item,
                    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(event))
                        if event.rollback_start_index.is_some()
                );
                offsets.push(offset);
                // Keep lifecycle, catalog, and typed completion evidence inspected by the exact
                // mask. Ordinary decoded records keep their position without becoming a forged
                // delivery-metadata neighbor for a reserved-looking context item.
                items.push(match line.item {
                    item @ RolloutItem::EventMsg(
                        EventMsg::ThreadRolledBack(_)
                        | EventMsg::TurnStarted(_)
                        | EventMsg::TurnComplete(_)
                        | EventMsg::TurnAborted(_),
                    ) => item,
                    RolloutItem::EventMsg(EventMsg::ItemCompleted(completed))
                        if completed.item.is_sub_agent_completion_presentation() =>
                    {
                        RolloutItem::EventMsg(EventMsg::ItemCompleted(completed))
                    }
                    item @ RolloutItem::InterAgentCommunicationMetadata { .. } => item,
                    RolloutItem::ResponseItem(envelope)
                        if envelope.id().is_some_and(|id| {
                            is_sub_agent_completion_context_response_item_id(id.as_str())
                        }) =>
                    {
                        RolloutItem::ResponseItem(envelope)
                    }
                    // Catalog selection belongs to the thread rather than the removed
                    // instruction range. Its payload is irrelevant to the exact mask; actual
                    // projection still reads the original canonical line below.
                    RolloutItem::DynamicToolsUpdated { thread_id, .. } => {
                        RolloutItem::DynamicToolsUpdated {
                            thread_id,
                            tools: Vec::new(),
                        }
                    }
                    _ => RolloutItem::EventMsg(EventMsg::Warning(WarningEvent {
                        message: String::new(),
                    })),
                });
            }
            offset += record.len() as u64;
        }
        // A suffix's local indexes are not canonical coordinates. Its marker only requests a
        // rebuild; the full source pass computes the mask before any projection filtering.
        let removed_offsets = if start_offset == 0 && contains_exact_marker {
            offsets
                .into_iter()
                .zip(codex_rollout::exact_rollback_removed_items(&items))
                .filter_map(|(offset, removed)| removed.then_some(offset))
                .collect()
        } else {
            HashSet::new()
        };
        Ok(Self {
            removed_offsets,
            contains_exact_marker,
        })
    }
}
