use std::time::Instant;

use codex_protocol::protocol::RolloutItem;
use tracing::info;
use tracing::warn;

use super::Session;
use crate::context::CompactedImageOmission;
use crate::context::CompactedMediaSanitization;
use crate::context::ContextualUserFragment;

impl Session {
    pub(super) async fn persist_reconstruction_repair(
        &self,
        repair_items: &[RolloutItem],
        sanitization: CompactedMediaSanitization,
    ) {
        let Some(live_thread) = self.live_thread() else {
            return;
        };
        if let Err(err) = live_thread.append_items(repair_items).await {
            warn!(
                %err,
                "failed to persist compacted-media rollout repair; a later resume will retry"
            );
            return;
        }
        if let Err(err) = live_thread.flush().await {
            warn!(
                %err,
                "failed to flush compacted-media rollout repair; a later resume will retry"
            );
            return;
        }
        info!(
            omitted_image_count = sanitization.omitted_image_count,
            omitted_inline_media_bytes = sanitization.omitted_inline_media_bytes,
            "persisted compacted-media rollout repair"
        );
        self.schedule_compacted_media_vacuum();
    }

    pub(super) fn schedule_compacted_media_vacuum(&self) {
        let Some(live_thread) = self.live_thread().cloned() else {
            return;
        };
        let policy = codex_rollout::CompactedMediaVacuumPolicy {
            reopenable_image_omission: CompactedImageOmission::reopenable_local_image().render(),
            unavailable_image_omission: CompactedImageOmission::unavailable().render(),
        };
        tokio::spawn(async move {
            let started = Instant::now();
            match live_thread.vacuum_compacted_media(policy).await {
                Ok(Some(report)) => info!(
                    bytes_before = report.bytes_before,
                    bytes_after = report.bytes_after,
                    records_rewritten = report.records_rewritten,
                    omitted_image_count = report.omitted_image_count,
                    omitted_inline_media_bytes = report.omitted_inline_media_bytes,
                    duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "vacuumed superseded compacted-media rollout snapshots"
                ),
                Ok(None) => {}
                Err(err) => warn!(
                    %err,
                    duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "failed to vacuum superseded compacted-media rollout snapshots"
                ),
            }
        });
    }
}
