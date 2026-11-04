//! Freeze display provenance before the first canonical completion publication.

use super::AgentTerminalPresentation;
use super::CompletionPresentation;
use super::LocalAgentControl;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::MultiAgentVersion;

impl AgentTerminalPresentation {
    /// Keep retries byte-identical even if aliases or model settings subsequently change.
    /// Missing labels never prevent an already accepted completion from being delivered.
    pub(crate) async fn capture_attribution(
        &self,
        control: &LocalAgentControl,
    ) -> &Option<codex_protocol::AgentInputAttribution> {
        self.inner
            .attribution
            .get_or_init(|| async {
                let snapshot: codex_protocol::error::Result<
                    Option<codex_protocol::AgentInputAttribution>,
                > = async {
                    let state = control.runtime.upgrade()?;
                    let recipient = state.get_thread(self.inner.parent.thread_id).await?;
                    if recipient.multi_agent_version() != Some(MultiAgentVersion::V1) {
                        return Ok(None);
                    }
                    let (_, attribution) = control
                        .presentation_attribution(
                            self.inner.child.thread_id,
                            self.inner.parent.thread_id,
                            &self.inner.turn_id,
                            /*batch_id*/ None,
                        )
                        .await?;
                    Ok(Some(attribution))
                }
                .await;
                match snapshot {
                    Ok(attribution) => attribution,
                    Err(error) => {
                        tracing::warn!(%error, "failed to capture completion display labels");
                        None
                    }
                }
            })
            .await
    }

    pub(crate) async fn labeled_presentation(
        &self,
        control: &LocalAgentControl,
        presentation: &CompletionPresentation,
    ) -> CompletionPresentation {
        let attribution = self.capture_attribution(control).await;
        let mut presentation = presentation.clone();
        if let TurnItem::AgentMessage(item) = &mut presentation.item {
            item.attribution.clone_from(attribution);
        }
        presentation
    }
}
