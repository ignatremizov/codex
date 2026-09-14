//! Wait-owned final receipts are separate from c744's model-response receipt proof.

use codex_history::RolloutItem;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_thread_store::MailboxFinalSubscription;

pub(super) fn wait_delivered_subscription(
    history: &[RolloutItem],
    subscription: &MailboxFinalSubscription,
) -> bool {
    let Some(turn_id) = subscription.bound_turn_id.as_deref() else {
        return false;
    };
    let removed = codex_history::rollout::exact_rollback_removed_items(history);
    history.iter().enumerate().any(|(index, item)| {
        if removed[index] {
            return false;
        }
        let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
            return false;
        };
        let TurnItem::CollabAgentToolCall(wait) = &event.item else {
            return false;
        };
        if event.thread_id != subscription.sender_thread_id
            || wait.sender_thread_id != subscription.sender_thread_id
            || wait.tool != CollabAgentTool::Wait
            || !wait
                .completion_presentation_agent_ids
                .as_ref()
                .is_some_and(|children| children.contains(&subscription.receiver_thread_id))
        {
            return false;
        }
        history[index + 1..]
            .iter()
            .enumerate()
            .take_while(|(_, item)| matches!(item, RolloutItem::AgentResponseObservation(_)))
            .any(|(offset, item)| {
                let RolloutItem::AgentResponseObservation(observation) = item else {
                    return false;
                };
                !removed[index + 1 + offset]
                    && observation.observer_thread_id == subscription.sender_thread_id
                    && observation.target_thread_id == subscription.receiver_thread_id
                    && observation.target_turn_id.as_deref() == Some(turn_id)
                    && observation.mailbox_final_subscription_message_id.as_deref()
                        == Some(subscription.message_id.as_str())
                    && observation
                        .final_delivery_response_item_id
                        .as_ref()
                        .is_some_and(|id| {
                            observation
                                .committed_delivery_response_item_ids
                                .contains(id)
                        })
            })
    })
}

#[cfg(test)]
#[path = "mailbox_final_subscription_delivery_tests.rs"]
mod tests;
