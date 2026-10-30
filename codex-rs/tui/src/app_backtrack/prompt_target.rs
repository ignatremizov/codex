//! Resolve an edit against canonical item identity before computing a guarded suffix.

use super::*;
use crate::history_cell::UserMessageIdentity;
use color_eyre::eyre::eyre;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LegacyRollbackTarget {
    pub(crate) num_turns: u32,
    pub(crate) expected_start_turn_id: String,
    pub(crate) expected_start_item_id: Option<String>,
    pub(crate) expected_turn_count: u32,
}

/// Canonical turn and optional mid-turn user-message boundary selected in the transcript.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PromptEditTarget {
    pub(crate) turn_index: usize,
    pub(crate) before_item_id: Option<String>,
}

/// Durable prompt-edit boundary; a missing item selects the whole turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptEditBoundary {
    pub(crate) turn_id: String,
    pub(crate) before_item_id: Option<String>,
}

impl LegacyRollbackTarget {
    pub(crate) fn new(turns: &[Turn], selected: &PromptEditTarget) -> Result<Self> {
        let turn = turns
            .get(selected.turn_index)
            .ok_or_else(|| eyre!("selected turn disappeared"))?;
        Ok(Self {
            num_turns: u32::try_from(turns.len() - selected.turn_index)?,
            expected_start_turn_id: turn.id.clone(),
            expected_start_item_id: selected.before_item_id.clone(),
            expected_turn_count: u32::try_from(turns.len())?,
        })
    }
}

pub(crate) fn selected_prompt_target(
    turns: &[Turn],
    identity: Option<&UserMessageIdentity>,
    start_item: Option<&(String, String)>,
    nth_user_message: usize,
    prompt: &mut UserMessage,
) -> Result<PromptEditTarget> {
    let (turn_index, item_index, content) = if let Some(identity) = identity {
        let mut matches = turns.iter().enumerate().flat_map(|(turn_index, turn)| {
            turn.items
                .iter()
                .enumerate()
                .filter_map(move |(item_index, item)| {
                    let ThreadItem::UserMessage { id, content, .. } = item else {
                        return None;
                    };
                    (turn.id == identity.turn_id && id == &identity.item_id)
                        .then_some((turn_index, item_index, content))
                })
        });
        let selected = matches.next().ok_or_else(|| {
            eyre!("the selected prompt identity was not found in the persisted thread")
        })?;
        if matches.next().is_some() {
            bail!("the selected prompt has ambiguous persisted identity");
        }
        selected
    } else {
        // Without receipt identity, repeated equal prompts cannot prove which optimistic
        // occurrence persisted. Never let an ordinal silently select another occurrence.
        let matches_prompt =
            |prompt: &UserMessage, content: &[codex_app_server_protocol::UserInput]| {
                let display = ChatWidget::user_message_display_from_inputs(content);
                prompt.text == display.message
                    && prompt.text_elements == display.text_elements
                    && prompt.remote_image_urls == display.remote_image_urls
                    && prompt
                        .local_images
                        .iter()
                        .map(|image| &image.path)
                        .eq(display.local_images.iter())
            };
        let matching_prompts = turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter(|item| {
                let ThreadItem::UserMessage { content, .. } = item else {
                    return false;
                };
                matches_prompt(prompt, content)
            })
            .count();
        if matching_prompts > 1 {
            bail!(
                "the selected prompt has ambiguous persisted content; reload the session before editing"
            );
        }
        let id = backtrack_revert_before_turn_id(turns, start_item, nth_user_message, prompt)?;
        let mut matches = turns.iter().enumerate().filter(|(_, turn)| turn.id == id);
        let index = matches
            .next()
            .map(|(index, _)| index)
            .ok_or_else(|| eyre!("selected turn disappeared"))?;
        if matches.next().is_some() {
            bail!("the selected prompt has ambiguous persisted turn identity");
        }
        let (item_index, content) = turns[index]
            .items
            .iter()
            .enumerate()
            .find_map(|(item_index, item)| {
                let ThreadItem::UserMessage { content, .. } = item else {
                    return None;
                };
                matches_prompt(prompt, content).then_some((item_index, content))
            })
            .ok_or_else(|| eyre!("the selected prompt disappeared"))?;
        (index, item_index, content)
    };
    let turn = &turns[turn_index];
    if turn.status == TurnStatus::InProgress {
        bail!("the selected prompt belongs to a turn that is still in progress");
    }
    let display = ChatWidget::user_message_display_from_inputs(content);
    prompt.mention_bindings = mention_bindings_from_user_inputs(content, &display.message);
    let before_item_id = turn.items[..item_index]
        .iter()
        .any(|item| matches!(item, ThreadItem::UserMessage { .. }))
        .then(|| turn.items[item_index].id().to_string());
    Ok(PromptEditTarget {
        turn_index,
        before_item_id,
    })
}

#[cfg(test)]
#[path = "prompt_target_tests.rs"]
mod tests;
