use codex_rollout::RolloutItem;
use tracing::info;
use tracing::warn;

use super::Session;
use super::rollout_reconstruction::AppliedRolloutReconstructionRepair;
use super::rollout_reconstruction::RolloutReconstructionRepairPersistence;
use crate::context::CompactedMediaSanitization;

impl Session {
    pub(super) async fn persist_reconstruction_repair_with_policy(
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
        if self.live_thread().is_none() {
            return Ok(());
        }
        let permit = super::thread_settings::acquire_persistence_lock(self).await;
        // Certification changes no live history, but uses the same durable publication fence
        // so a cancelled waiter cannot hide an uncertain canonical write.
        let receiver = self.dispatch_history_publication(
            permit,
            repair_items.to_vec(),
            Vec::new(),
            /*acknowledgement*/ None,
            |_state| (),
        )?;
        self.publication_result(receiver).await?;
        info!(
            omitted_image_count = sanitization.omitted_image_count,
            omitted_inline_media_bytes = sanitization.omitted_inline_media_bytes,
            "persisted compacted-media rollout repair"
        );
        Ok(())
    }
}
