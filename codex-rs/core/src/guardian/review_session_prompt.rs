//! Captures synchronous evidence once for the common preparation pass.
//! Image admission waits for final input selection after pre-turn compaction.

use codex_guardian_reviewer::GuardianPromptItems;

use super::super::prompt::GuardianNodeReplCursor;
use super::super::prompt::GuardianPromptMode;
use super::super::prompt::GuardianTranscriptHistory;
use super::super::prompt::collect_guardian_prompt_items_with_parent_turn;
use super::GuardianReviewSessionParams;

pub(super) async fn build_guardian_prompt_items(
    params: &GuardianReviewSessionParams,
    history: GuardianTranscriptHistory<'_>,
    mode: GuardianPromptMode,
    node_repl_cursor: GuardianNodeReplCursor,
) -> anyhow::Result<GuardianPromptItems> {
    let collected = collect_guardian_prompt_items_with_parent_turn(
        params.parent_session.as_ref(),
        history,
        Some(&params.parent_context),
        params.reasons.clone(),
        params.request.clone(),
        mode,
        node_repl_cursor,
    )
    .await?;
    // Keep bounded transcript and text preparation, but retain screenshot candidates until
    // input_budget::finalize can compare them with the history that actually reaches the model.
    codex_guardian_reviewer::compose_guardian_prompt(
        collected.sections,
        &collected.profile,
        collected.mode,
        collected.history_version,
        &collected.session_id,
        collected.node_repl_evidence_sequence,
    )
}
