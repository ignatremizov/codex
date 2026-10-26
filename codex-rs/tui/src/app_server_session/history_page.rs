//! Own older-page network work independently of the TUI input loop.
//!
//! Cursor advancement becomes visible only when the exact admitted request is applied. Dropping
//! or cancelling its pending owner stops background work, including dependent metadata requests.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemsListResponse;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::bail;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::AppServerSession;
use super::HISTORY_ITEM_PAGE_LIMIT;
use super::ThreadHistoryPagination;
use super::advancing_cursor;
use super::thread_items_page_params;

const HISTORY_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OlderHistoryPageRequest {
    pub(crate) cursor: String,
    id: Uuid,
}

#[derive(Debug)]
pub(super) struct PendingOlderHistoryPage {
    request: OlderHistoryPageRequest,
    cancellation: CancellationToken,
}

impl Drop for PendingOlderHistoryPage {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct OlderHistoryPageLoad {
    pub(crate) request: OlderHistoryPageRequest,
    thread_id: ThreadId,
    client: AppServerRequestHandle,
    pagination: ThreadHistoryPagination,
    cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub(crate) struct OlderHistoryPageContents {
    pub(crate) turns: Vec<Turn>,
    pub(crate) items: Vec<ThreadItem>,
    pub(crate) snapshotted_turn_ids: HashSet<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedOlderHistoryPage {
    pagination: ThreadHistoryPagination,
    contents: OlderHistoryPageContents,
}

impl AppServerSession {
    pub(crate) fn begin_older_history_page(
        &mut self,
        thread_id: ThreadId,
    ) -> Option<OlderHistoryPageLoad> {
        let client = self.request_handle();
        let page = self.history_pagination.get_mut(&thread_id)?;
        if page.loading_older.is_some() {
            return None;
        }
        let request = OlderHistoryPageRequest {
            cursor: page.next_item_cursor.clone()?,
            id: Uuid::new_v4(),
        };
        let cancellation = CancellationToken::new();
        let pagination = page.clone();
        page.loading_older = Some(Arc::new(PendingOlderHistoryPage {
            request: request.clone(),
            cancellation: cancellation.clone(),
        }));
        Some(OlderHistoryPageLoad {
            request,
            thread_id,
            client,
            pagination,
            cancellation,
        })
    }

    /// Match the exact admission, including retries that reuse the same cursor.
    pub(crate) fn is_older_history_page_pending(
        &self,
        thread_id: ThreadId,
        request: &OlderHistoryPageRequest,
    ) -> bool {
        self.history_pagination
            .get(&thread_id)
            .and_then(|page| page.loading_older.as_ref())
            .is_some_and(|pending| pending.request == *request)
    }

    pub(crate) fn cancel_older_history_page(
        &mut self,
        thread_id: ThreadId,
        request: &OlderHistoryPageRequest,
    ) {
        if self.is_older_history_page_pending(thread_id, request)
            && let Some(page) = self.history_pagination.get_mut(&thread_id)
        {
            page.loading_older = None;
        }
    }

    pub(crate) fn apply_older_history_page(
        &mut self,
        thread_id: ThreadId,
        request: &OlderHistoryPageRequest,
        page: LoadedOlderHistoryPage,
    ) -> Option<OlderHistoryPageContents> {
        if !self.is_older_history_page_pending(thread_id, request) {
            return None;
        }
        self.history_pagination.insert(thread_id, page.pagination);
        Some(page.contents)
    }
}

impl OlderHistoryPageLoad {
    pub(crate) async fn fetch(self, turns: Vec<Turn>) -> Result<LoadedOlderHistoryPage> {
        let cancellation = self.cancellation.clone();
        let work = async {
            let page = request_history_page(
                &self.client,
                &self.cancellation,
                ClientRequest::ThreadItemsList {
                    request_id: RequestId::String(format!("tui-history-items-{}", self.request.id)),
                    params: thread_items_page_params(
                        self.thread_id,
                        /*turn_id*/ None,
                        Some(self.request.cursor.clone()),
                        HISTORY_ITEM_PAGE_LIMIT,
                    ),
                },
            )
            .await?;
            self.prepare(page, turns).await
        };
        tokio::select! {
            _ = cancellation.cancelled() => {
                bail!("history loading cancelled");
            }
            result = tokio::time::timeout(HISTORY_PAGE_TIMEOUT, work) => {
                result.wrap_err("older history page timed out")?
            }
        }
    }

    /// Resolve metadata for a received item page before exposing any cursor advancement.
    pub(crate) async fn prepare(
        mut self,
        page: ThreadItemsListResponse,
        mut turns: Vec<Turn>,
    ) -> Result<LoadedOlderHistoryPage> {
        let snapshotted_turn_ids = turns.iter().map(|turn| turn.id.clone()).collect();
        let items = merge_thread_item_page(
            &self.client,
            self.thread_id,
            page,
            &mut self.pagination,
            &mut turns,
            &self.cancellation,
        )
        .await?;
        Ok(LoadedOlderHistoryPage {
            pagination: self.pagination,
            contents: OlderHistoryPageContents {
                turns,
                items,
                snapshotted_turn_ids,
            },
        })
    }
}

async fn request_history_page<T: DeserializeOwned>(
    client: &AppServerRequestHandle,
    cancellation: &CancellationToken,
    request: ClientRequest,
) -> Result<T> {
    tokio::select! {
        _ = cancellation.cancelled() => {
            bail!("history loading cancelled");
        }
        result = tokio::time::timeout(HISTORY_PAGE_TIMEOUT, client.request_typed(request)) => {
            result.wrap_err("history request timed out")?.map_err(Into::into)
        }
    }
}

pub(super) async fn merge_thread_item_page(
    client: &AppServerRequestHandle,
    thread_id: ThreadId,
    page: ThreadItemsListResponse,
    state: &mut ThreadHistoryPagination,
    turns: &mut Vec<Turn>,
    cancellation: &CancellationToken,
) -> Result<Vec<ThreadItem>> {
    state.next_item_cursor = advancing_cursor(
        state.next_item_cursor.as_deref(),
        page.next_cursor,
        &mut state.seen_item_cursors,
    );
    let mut missing_turn_ids = page
        .data
        .iter()
        .filter(|entry| !turns.iter().any(|turn| turn.id == entry.turn_id))
        .map(|entry| entry.turn_id.clone())
        .collect::<HashSet<_>>();
    let mut items = Vec::new();
    for entry in page.data {
        while !turns.iter().any(|turn| turn.id == entry.turn_id) {
            let Some(cursor) = state.next_turn_cursor.take() else {
                break;
            };
            // Empty turns between item-backed turns can require another bounded metadata page.
            let limit = missing_turn_ids.len().min(HISTORY_ITEM_PAGE_LIMIT as usize) as u32;
            let page: ThreadTurnsListResponse = request_history_page(
                client,
                cancellation,
                ClientRequest::ThreadTurnsList {
                    request_id: RequestId::String(format!("tui-history-turns-{}", Uuid::new_v4())),
                    params: ThreadTurnsListParams {
                        thread_id: thread_id.to_string(),
                        cursor: Some(cursor.clone()),
                        limit: Some(limit),
                        sort_direction: Some(SortDirection::Desc),
                        items_view: Some(TurnItemsView::NotLoaded),
                    },
                },
            )
            .await?;
            state.next_turn_cursor = advancing_cursor(
                Some(&cursor),
                page.next_cursor,
                &mut state.seen_turn_cursors,
            );
            for turn in &page.data {
                missing_turn_ids.remove(&turn.id);
            }
            turns.splice(0..0, page.data.into_iter().rev());
        }
        if let Some(turn) = turns.iter_mut().find(|turn| turn.id == entry.turn_id)
            && !turn.items.iter().any(|item| item.id() == entry.item.id())
        {
            items.push(entry.item.clone());
            turn.items.insert(/*index*/ 0, entry.item);
            turn.items_view = TurnItemsView::Summary;
        }
    }
    items.reverse();
    Ok(items)
}
