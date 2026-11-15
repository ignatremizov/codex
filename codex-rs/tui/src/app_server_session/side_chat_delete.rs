//! Plan a saved-side-chat cascade before mutation, then revalidate each deletion boundary.
//!
//! Each delete is still a server-owned single-thread operation. This is not a transaction:
//! a failed response leaves the last requested deletion uncertain, not safe to retry blindly.

use super::ATTACHED_SIDE_CHAT_SOURCE;
use super::AppServerSession;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadSource;
use codex_protocol::ThreadId;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::ensure;
use color_eyre::eyre::eyre;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug)]
struct SavedSideChat {
    id: ThreadId,
    parent: ThreadId,
}

impl SavedSideChat {
    fn verify(self, thread: &Thread) -> Result<()> {
        let parent = self.parent.to_string();
        ensure!(
            thread.id == self.id.to_string()
                && !thread.ephemeral
                && thread.attached_to_thread_id.as_deref() == Some(parent.as_str())
                && thread.forked_from_id.as_deref() == Some(parent.as_str())
                && matches!(thread.thread_source.as_ref(), Some(ThreadSource::Feature(source))
                    if source == ATTACHED_SIDE_CHAT_SOURCE),
            "Attached thread {} is not a current saved TUI side chat of {}. No unrelated attachment will be deleted.",
            self.id,
            self.parent,
        );
        Ok(())
    }
}

impl AppServerSession {
    /// Delete verified saved side chats, including archived and nested ones, before the parent.
    pub(crate) async fn delete_thread_with_saved_side_chats(
        &mut self,
        parent: ThreadId,
    ) -> Result<()> {
        if !self.can_list_saved_side_chats(parent) {
            // These backends have no local saved-sidebar traversal. Preserve their existing
            // single-thread operation and server-side attachment/reference vetoes.
            return self.thread_delete(parent).await;
        }
        let query = self
            .attached_side_chat_query(parent)
            .await?
            .ok_or_else(|| {
                eyre!("Cannot safely enumerate saved side chats; no delete requests were sent.")
            })?;
        let original = self.thread_read(parent, /*include_turns*/ false).await?;
        ensure!(
            original.id == parent.to_string() && !original.ephemeral,
            "Cannot verify the selected persisted parent; no delete requests were sent."
        );
        let children = self
            .saved_side_chat_deletion_order(parent, query)
            .await
            .wrap_err(
                "Cannot verify the complete saved-side-chat cascade; no delete requests were sent",
            )?;
        let mut deleted = Vec::new();
        for child in children {
            let progress = confirmed_deletions(&deleted);
            let current = self
                .thread_read(child.id, /*include_turns*/ false)
                .await
                .wrap_err_with(|| format!("Cannot revalidate saved side chat {}. {progress} Parent deletion was not requested.", child.id))?;
            child.verify(&current).wrap_err_with(|| {
                format!("Cascade stopped before deleting {}. {progress} Parent deletion was not requested.", child.id)
            })?;
            self.thread_delete(child.id).await.wrap_err_with(|| {
                format!("Deletion of saved side chat {} was not confirmed. {progress} Parent deletion was not requested. Refresh before retrying.", child.id)
            })?;
            deleted.push(child.id);
        }
        let progress = confirmed_deletions(&deleted);
        let current = self
            .thread_read(parent, /*include_turns*/ false)
            .await
            .wrap_err_with(|| format!("Cannot revalidate parent {parent}. {progress} Parent deletion was not requested."))?;
        ensure!(
            current.id == original.id
                && current.ephemeral == original.ephemeral
                && current.attached_to_thread_id == original.attached_to_thread_id
                && current.forked_from_id == original.forked_from_id
                && current.parent_thread_id == original.parent_thread_id
                && current.thread_source == original.thread_source,
            "Parent identity or attachment changed during the cascade. {progress} Parent deletion was not requested."
        );
        self.thread_delete(parent).await.wrap_err_with(|| {
            format!(
                "Deletion of parent {parent} was not confirmed. {progress} Refresh before retrying."
            )
        })
    }

    async fn saved_side_chat_deletion_order(
        &mut self,
        parent: ThreadId,
        mut query: ThreadListParams,
    ) -> Result<Vec<SavedSideChat>> {
        let mut seen = HashSet::from([parent]);
        let mut pending = vec![parent];
        let mut children = Vec::new();
        while let Some(parent) = pending.pop() {
            query.attached_to_thread_id = Some(parent.to_string());
            query.archived = Some(false);
            let mut attached = self.list_attached_threads(query.clone()).await?;
            query.archived = Some(true);
            attached.extend(self.list_attached_threads(query.clone()).await?);
            attached.sort_unstable_by(|a, b| a.id.cmp(&b.id));
            attached.dedup_by(|a, b| a.id == b.id);
            for thread in attached {
                let id = ThreadId::from_string(&thread.id)?;
                ensure!(
                    seen.insert(id),
                    "Cycle or conflicting saved-side-chat parent for {id}"
                );
                let child = SavedSideChat { id, parent };
                // Read current canonical metadata, not a stale list entry, first rollout header,
                // visible tab, or spawn ancestry. Listing cannot grant delete authority.
                child.verify(&self.thread_read(id, /*include_turns*/ false).await?)?;
                children.push(child);
                pending.push(id);
            }
        }
        // Parents precede their descendants in discovery, so reversal is child-first.
        children.reverse();
        Ok(children)
    }
}

fn confirmed_deletions(deleted: &[ThreadId]) -> String {
    let ids = deleted
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("Saved side chats confirmed deleted: [{ids}].")
}

#[cfg(test)]
#[path = "side_chat_delete_tests.rs"]
mod tests;
