use crate::QueuedItemService;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ThreadIdleCause;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_protocol::ThreadId;
use std::sync::Arc;

/// Only the lowest-priority idle callback; all other queue callbacks stay single.
pub(crate) struct MailboxInventoryFallback(pub(crate) Arc<QueuedItemService>);

impl<C> ThreadLifecycleContributor<C> for MailboxInventoryFallback
where
    C: Send + Sync + 'static,
{
    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if input.cause == ThreadIdleCause::Interrupted {
                return;
            }
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            if let Err(error) = self.0.dispatch_inventory_if_idle(thread_id).await {
                tracing::warn!(%thread_id, %error, "failed to dispatch mailbox inventory");
            }
        })
    }
}
