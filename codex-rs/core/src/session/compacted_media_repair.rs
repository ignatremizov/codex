use codex_protocol::protocol::RolloutItem;
use tracing::info;
use tracing::warn;

use super::Session;
use super::rollout_reconstruction::AppliedRolloutReconstructionRepair;
use super::rollout_reconstruction::RolloutReconstructionRepairPersistence;
use crate::context::CompactedMediaSanitization;

impl Session {
    pub(super) async fn persist_initial_reconstruction_repair(
        &self,
        repair: &AppliedRolloutReconstructionRepair,
    ) -> anyhow::Result<()> {
        match self
            .persist_reconstruction_repair(repair.items.as_slice(), repair.sanitization)
            .await
        {
            Ok(()) => Ok(()),
            Err(err)
                if matches!(
                    repair.persistence,
                    RolloutReconstructionRepairPersistence::BestEffort
                ) =>
            {
                warn!(
                    %err,
                    "failed to persist optional compacted-media policy certification"
                );
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn persist_reconstruction_repair(
        &self,
        repair_items: &[RolloutItem],
        sanitization: CompactedMediaSanitization,
    ) -> anyhow::Result<()> {
        let Some(live_thread) = self.live_thread() else {
            return Ok(());
        };
        live_thread.append_items(repair_items).await?;
        live_thread.flush().await?;
        info!(
            omitted_image_count = sanitization.omitted_image_count,
            omitted_inline_media_bytes = sanitization.omitted_inline_media_bytes,
            "persisted compacted-media rollout repair"
        );
        Ok(())
    }
}
