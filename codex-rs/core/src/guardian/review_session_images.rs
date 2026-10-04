//! Prepares private image identities for the shared review-history comparison.
//! Aggregate prompt limits remain the final input-selection owner's responsibility.

use std::borrow::Cow;

use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::normalize_output_image_detail;
use tracing::warn;

use crate::image_preparation::ImagePreparationMode;
use crate::image_preparation::resize_image;

/// Normalizes detail and returns the transmitted identity for shared deduplication.
pub(super) fn prepare_review_image<'a>(
    image: &'a ImageReference,
    detail: &mut Option<ImageDetail>,
    model_info: &ModelInfo,
    mode: ImagePreparationMode,
) -> Option<Cow<'a, ImageReference>> {
    *detail = match normalize_output_image_detail(model_info, *detail) {
        _ if mode == ImagePreparationMode::UnifiedBudget => Some(ImageDetail::Original),
        Some(ImageDetail::Low) => Some(ImageDetail::High),
        detail => detail,
    };
    match image {
        ImageReference::Inline { image_url } => match resize_image(image_url, detail, mode) {
            Ok(Some(prepared)) => Some(Cow::Owned(ImageReference::Inline {
                image_url: prepared.into_data_url(),
            })),
            Ok(None) => Some(Cow::Borrowed(image)),
            Err(error) => {
                warn!(%error, "failed to prepare guardian review image");
                None
            }
        },
        ImageReference::File { .. } => Some(Cow::Borrowed(image)),
    }
}
