use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_with_path_text;

use super::ContextualUserFragment;

const REOPENABLE_IMAGE_OMISSION: &str =
    "Image bytes removed after compaction; use view_image with retained image paths if needed.";
const UNAVAILABLE_IMAGE_OMISSION: &str =
    "Image bytes removed after compaction; no durable source reference is available.";
const MIXED_IMAGE_OMISSION: &str = "Image bytes removed after compaction; retained paths can be reopened with view_image, while images without durable source references are unavailable.";
const COMPACTED_IMAGE_OMISSION_OPEN_TAG: &str = "<compacted_image_omission>";
const COMPACTED_IMAGE_OMISSION_CLOSE_TAG: &str = "</compacted_image_omission>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactedImageOmission {
    kind: CompactedImageOmissionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactedImageOmissionKind {
    ReopenableLocalImage,
    Unavailable,
    Mixed,
}

impl CompactedImageOmission {
    pub(crate) const fn reopenable_local_image() -> Self {
        Self {
            kind: CompactedImageOmissionKind::ReopenableLocalImage,
        }
    }

    pub(crate) const fn unavailable() -> Self {
        Self {
            kind: CompactedImageOmissionKind::Unavailable,
        }
    }

    const fn mixed() -> Self {
        Self {
            kind: CompactedImageOmissionKind::Mixed,
        }
    }

    fn kind_from_text(text: &str) -> Option<CompactedImageOmissionKind> {
        let body = text
            .strip_prefix(COMPACTED_IMAGE_OMISSION_OPEN_TAG)?
            .strip_suffix(COMPACTED_IMAGE_OMISSION_CLOSE_TAG)?;
        match body {
            REOPENABLE_IMAGE_OMISSION => Some(CompactedImageOmissionKind::ReopenableLocalImage),
            UNAVAILABLE_IMAGE_OMISSION => Some(CompactedImageOmissionKind::Unavailable),
            MIXED_IMAGE_OMISSION => Some(CompactedImageOmissionKind::Mixed),
            _ => None,
        }
    }
}

pub(crate) fn is_compacted_image_omission_text(text: &str) -> bool {
    CompactedImageOmission::kind_from_text(text).is_some()
}

impl ContextualUserFragment for CompactedImageOmission {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            COMPACTED_IMAGE_OMISSION_OPEN_TAG,
            COMPACTED_IMAGE_OMISSION_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        match self.kind {
            CompactedImageOmissionKind::ReopenableLocalImage => {
                REOPENABLE_IMAGE_OMISSION.to_string()
            }
            CompactedImageOmissionKind::Unavailable => UNAVAILABLE_IMAGE_OMISSION.to_string(),
            CompactedImageOmissionKind::Mixed => MIXED_IMAGE_OMISSION.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CompactedMediaSanitization {
    pub(crate) omitted_image_count: usize,
    pub(crate) omitted_inline_media_bytes: u64,
    did_rewrite: bool,
}

impl CompactedMediaSanitization {
    pub(crate) fn changed(self) -> bool {
        self.did_rewrite
    }

    pub(crate) fn accumulate(&mut self, other: Self) {
        self.omitted_image_count = self
            .omitted_image_count
            .saturating_add(other.omitted_image_count);
        self.omitted_inline_media_bytes = self
            .omitted_inline_media_bytes
            .saturating_add(other.omitted_inline_media_bytes);
        self.did_rewrite |= other.did_rewrite;
    }
}

pub(crate) fn sanitize_compacted_media(items: &mut [ResponseItem]) -> CompactedMediaSanitization {
    sanitize_compacted_media_prefix(items, items.len())
}

pub(crate) fn sanitize_compacted_media_prefix(
    items: &mut [ResponseItem],
    prefix_len: usize,
) -> CompactedMediaSanitization {
    let prefix_len = prefix_len.min(items.len());
    let mut inventory = CompactedMediaInventory::default();
    for item in items.iter().take(prefix_len) {
        match item {
            ResponseItem::Message { content, .. } => {
                inventory.inspect_message_content(content);
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items() {
                    inventory.inspect_tool_output_content(content);
                }
            }
            _ => {}
        }
    }
    if inventory.sanitization.omitted_image_count == 0 && inventory.omission_count <= 1 {
        return inventory.sanitization;
    }
    inventory.sanitization.did_rewrite = true;

    // Emit one bounded model-visible omission fragment for the entire sanitized checkpoint, not
    // one fragment per image. All canonical source-path wrapper text remains in place.
    let omission = match (inventory.has_local_reference, inventory.has_unavailable) {
        (true, true) => CompactedImageOmission::mixed(),
        (true, false) => CompactedImageOmission::reopenable_local_image(),
        (false, true) => CompactedImageOmission::unavailable(),
        (false, false) => CompactedImageOmission::unavailable(),
    };
    let Some(marker_target_index) = items[..prefix_len]
        .iter()
        .rposition(response_item_contains_compacted_media)
    else {
        return inventory.sanitization;
    };
    let marker_insertion_index = match &items[marker_target_index] {
        ResponseItem::Message { content, .. } => content
            .iter()
            .position(|item| match item {
                ContentItem::InputImage { .. } => true,
                ContentItem::InputText { text } => is_compacted_image_omission_text(text),
                ContentItem::OutputText { .. } => false,
            })
            .unwrap_or(0),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output
            .content_items()
            .and_then(|content| {
                content.iter().position(|item| match item {
                    FunctionCallOutputContentItem::InputImage { .. } => true,
                    FunctionCallOutputContentItem::InputText { text } => {
                        is_compacted_image_omission_text(text)
                    }
                    FunctionCallOutputContentItem::EncryptedContent { .. } => false,
                })
            })
            .unwrap_or(0),
        _ => 0,
    };
    for item in items.iter_mut().take(prefix_len) {
        match item {
            ResponseItem::Message { content, .. } => {
                sanitize_message_content(content);
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    sanitize_tool_output_content(content);
                }
            }
            _ => {}
        }
    }
    let omission_text = omission.render();
    match &mut items[marker_target_index] {
        ResponseItem::Message { content, .. } => {
            content.insert(
                marker_insertion_index.min(content.len()),
                ContentItem::InputText {
                    text: omission_text,
                },
            );
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let Some(content) = output.content_items_mut() {
                content.insert(
                    marker_insertion_index.min(content.len()),
                    FunctionCallOutputContentItem::InputText {
                        text: omission_text,
                    },
                );
            }
        }
        _ => {}
    }
    inventory.sanitization
}

pub(crate) fn sanitize_compacted_media_before_latest_compaction(
    items: &mut [ResponseItem],
) -> CompactedMediaSanitization {
    let Some(compaction_index) = items.iter().rposition(|item| {
        matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        )
    }) else {
        return CompactedMediaSanitization::default();
    };
    sanitize_compacted_media_prefix(items, compaction_index)
}

/// Expires image references inherited from a previously compacted window.
///
/// Callers sanitize the inherited items first so each canonical local-image wrapper is left as an
/// adjacent open/close pair. The current compaction request can still contain those paths, but the
/// newly installed replacement history should retain references only for images introduced after
/// the latest compaction boundary.
pub(crate) fn expire_compacted_media_references(items: &mut [ResponseItem]) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                content.retain(|item| {
                    !matches!(
                        item,
                        ContentItem::InputText { text }
                            if is_compacted_image_omission_text(text)
                    )
                });
                let mut retained = Vec::with_capacity(content.len());
                let mut content_items = std::mem::take(content).into_iter().peekable();
                while let Some(content_item) = content_items.next() {
                    let expires_local_reference = matches!(
                        &content_item,
                        ContentItem::InputText { text }
                            if is_local_image_open_tag_with_path_text(text)
                    ) && matches!(
                        content_items.peek(),
                        Some(ContentItem::InputText { text })
                            if is_local_image_close_tag_text(text)
                    );
                    if expires_local_reference {
                        let _ = content_items.next();
                    } else {
                        retained.push(content_item);
                    }
                }
                *content = retained;
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    content.retain(|item| {
                        !matches!(
                            item,
                            FunctionCallOutputContentItem::InputText { text }
                                if is_compacted_image_omission_text(text)
                        )
                    });
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct CompactedMediaInventory {
    sanitization: CompactedMediaSanitization,
    has_local_reference: bool,
    has_unavailable: bool,
    omission_count: usize,
}

impl CompactedMediaInventory {
    fn inspect_message_content(&mut self, content: &[ContentItem]) {
        for (index, item) in content.iter().enumerate() {
            match item {
                ContentItem::InputImage { image_url, .. } => {
                    // The wrapper is a persisted model-visible path hint, not an authorization
                    // credential. `view_image` still resolves it through the active environment's
                    // filesystem policy; this check only decides whether retaining the hint is
                    // more useful than claiming no source exists.
                    let has_local_reference = index > 0
                        && matches!(
                            &content[index - 1],
                            ContentItem::InputText { text }
                                if is_local_image_open_tag_with_path_text(text)
                        )
                        && matches!(
                            content.get(index + 1),
                            Some(ContentItem::InputText { text })
                                if is_local_image_close_tag_text(text)
                        );
                    self.record_image(image_url, has_local_reference);
                }
                ContentItem::InputText { text } => self.record_omission_text(text),
                ContentItem::OutputText { .. } => {}
            }
        }
    }

    fn inspect_tool_output_content(&mut self, content: &[FunctionCallOutputContentItem]) {
        for item in content {
            match item {
                FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                    // Structured tool output can be owned by another environment, so it remains
                    // unavailable without typed provenance.
                    self.record_image(image_url, /*has_local_reference*/ false);
                }
                FunctionCallOutputContentItem::InputText { text } => {
                    self.record_omission_text(text);
                }
                FunctionCallOutputContentItem::EncryptedContent { .. } => {}
            }
        }
    }

    fn record_image(&mut self, image_url: &str, has_local_reference: bool) {
        self.sanitization.omitted_image_count =
            self.sanitization.omitted_image_count.saturating_add(1);
        self.sanitization.omitted_inline_media_bytes = self
            .sanitization
            .omitted_inline_media_bytes
            .saturating_add(u64::try_from(image_url.len()).unwrap_or(u64::MAX));
        self.has_local_reference |= has_local_reference;
        self.has_unavailable |= !has_local_reference;
    }

    fn record_omission_text(&mut self, text: &str) {
        let Some(kind) = CompactedImageOmission::kind_from_text(text) else {
            return;
        };
        self.omission_count = self.omission_count.saturating_add(1);
        match kind {
            CompactedImageOmissionKind::ReopenableLocalImage => {
                self.has_local_reference = true;
            }
            CompactedImageOmissionKind::Unavailable => {
                self.has_unavailable = true;
            }
            CompactedImageOmissionKind::Mixed => {
                self.has_local_reference = true;
                self.has_unavailable = true;
            }
        }
    }
}

fn response_item_contains_compacted_media(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| match item {
            ContentItem::InputImage { .. } => true,
            ContentItem::InputText { text } => is_compacted_image_omission_text(text),
            ContentItem::OutputText { .. } => false,
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| match item {
                    FunctionCallOutputContentItem::InputImage { .. } => true,
                    FunctionCallOutputContentItem::InputText { text } => {
                        is_compacted_image_omission_text(text)
                    }
                    FunctionCallOutputContentItem::EncryptedContent { .. } => false,
                })
            })
        }
        _ => false,
    }
}

fn sanitize_message_content(content: &mut Vec<ContentItem>) {
    let mut sanitized = Vec::with_capacity(content.len());
    for item in std::mem::take(content) {
        let omit = match &item {
            ContentItem::InputImage { .. } => true,
            ContentItem::InputText { text } => is_compacted_image_omission_text(text),
            ContentItem::OutputText { .. } => false,
        };
        if !omit {
            sanitized.push(item);
        }
    }
    *content = sanitized;
}

fn sanitize_tool_output_content(content: &mut Vec<FunctionCallOutputContentItem>) {
    let mut sanitized = Vec::with_capacity(content.len());
    for item in std::mem::take(content) {
        let omit = match &item {
            FunctionCallOutputContentItem::InputImage { .. } => true,
            FunctionCallOutputContentItem::InputText { text } => {
                is_compacted_image_omission_text(text)
            }
            FunctionCallOutputContentItem::EncryptedContent { .. } => false,
        };
        if !omit {
            sanitized.push(item);
        }
    }
    *content = sanitized;
}

#[cfg(test)]
#[path = "compacted_media_tests.rs"]
mod tests;
