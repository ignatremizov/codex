use codex_protocol::ThreadId;
use codex_protocol::protocol::AdditionalContextEntry;
use std::collections::BTreeMap;

use crate::ExtensionData;

/// Context snapshot whose authority remains leased through checkpoint publication.
#[derive(Default)]
pub struct PostCompactionContextContribution {
    items: Vec<codex_protocol::models::ResponseItem>,
    additional_context: BTreeMap<String, AdditionalContextEntry>,
    _lease: Option<Box<dyn Send>>,
    validation: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

impl PostCompactionContextContribution {
    pub fn new(items: Vec<codex_protocol::models::ResponseItem>) -> Self {
        Self {
            items,
            ..Self::default()
        }
    }

    pub fn with_lease_and_validation(
        items: Vec<codex_protocol::models::ResponseItem>,
        lease: impl Send + 'static,
        validation: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            items,
            _lease: Some(Box::new(lease)),
            validation: Some(Box::new(validation)),
            additional_context: BTreeMap::new(),
        }
    }

    /// Adds context with its original trust classification. The host chooses its current
    /// supported representation; these values do not become genuine human instructions.
    pub fn with_additional_context(
        mut self,
        context: BTreeMap<String, AdditionalContextEntry>,
    ) -> Self {
        self.additional_context = context;
        self
    }

    /// Extracts classified context while retaining the same publication lease and validator.
    pub fn take_additional_context(&mut self) -> BTreeMap<String, AdditionalContextEntry> {
        std::mem::take(&mut self.additional_context)
    }

    /// Called under the host publication/configuration fence, before dispatch.
    pub fn is_current(&self) -> bool {
        self.validation
            .as_ref()
            .is_none_or(|validation| validation())
    }

    /// Leaves the lease attached until the host finishes publishing the extracted items.
    pub fn take_items(&mut self) -> Vec<codex_protocol::models::ResponseItem> {
        std::mem::take(&mut self.items)
    }
}

/// Host context available while extensions contribute turn-scoped context fragments.
#[derive(Clone, Copy)]
pub struct TurnContextContributionInput<'a> {
    /// Stable host-owned thread identifier.
    pub thread_id: ThreadId,
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn.
    pub turn_store: &'a ExtensionData,
    /// Usable context window of the captured model for this context build, when known.
    pub model_context_window: Option<i64>,
}
