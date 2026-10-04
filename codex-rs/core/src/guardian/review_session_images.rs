//! Compares private evidence against the reviewer's model-prepared image identities.
//! Aggregate prompt limits remain the final input-selection owner's responsibility.

use std::borrow::Cow;
use std::collections::HashSet;

use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::normalize_output_image_detail;
use tracing::warn;

use crate::image_preparation::ImagePreparationMode;
use crate::image_preparation::resize_image;

/// Normalizes detail and compares the transmitted identity, so resized screenshots
/// are not mistaken for missing images and replayed on every subsequent review.
pub(super) fn is_missing_review_image(
    image: &ImageReference,
    detail: &mut Option<ImageDetail>,
    model_info: &ModelInfo,
    mode: ImagePreparationMode,
    reviewer_image_urls: &HashSet<&str>,
    reviewer_file_ids: &HashSet<&str>,
) -> bool {
    *detail = match normalize_output_image_detail(model_info, *detail) {
        _ if mode == ImagePreparationMode::UnifiedBudget => Some(ImageDetail::Original),
        Some(ImageDetail::Low) => Some(ImageDetail::High),
        detail => detail,
    };
    match image {
        ImageReference::Inline { image_url } => {
            let prepared_image_url = match resize_image(image_url, detail, mode) {
                Ok(Some(prepared)) => Cow::Owned(prepared.into_data_url()),
                Ok(None) => Cow::Borrowed(image_url),
                Err(error) => {
                    warn!(%error, "failed to prepare guardian review image");
                    return false;
                }
            };
            !reviewer_image_urls.contains(prepared_image_url.as_str())
        }
        ImageReference::File { file_id } => !reviewer_file_ids.contains(file_id.as_str()),
    }
}
