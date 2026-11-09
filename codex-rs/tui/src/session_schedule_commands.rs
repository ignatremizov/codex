//! Headless scheduling uses the same app-server connection and queue as the TUI.

use crate::app_server_session::AppServerSession;
use crate::named_session_lookup::SessionCollection;
use crate::named_session_lookup::lookup;
use crate::session_archive_commands::SessionArchiveCommandOptions;
use crate::session_archive_commands::start_app_server_for_session_command;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::QueuedSubmission;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadQueueDeleteParams;
use codex_app_server_protocol::ThreadQueueDeleteResponse;
use codex_app_server_protocol::ThreadQueueListParams;
use codex_app_server_protocol::ThreadQueueListResponse;
use codex_app_server_protocol::ThreadQueueResultParams;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_app_server_protocol::ThreadQueueScheduleParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_utils_home_dir::find_codex_home;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use color_eyre::eyre::eyre;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

pub struct SessionScheduleClient {
    app_server: AppServerSession,
    codex_home: PathBuf,
}

impl SessionScheduleClient {
    pub async fn connect(options: SessionArchiveCommandOptions) -> Result<Self> {
        let codex_home = find_codex_home()
            .wrap_err("failed to find Codex home")?
            .to_path_buf();
        let app_server = start_app_server_for_session_command(options, codex_home.clone())
            .await?
            .server;
        Ok(Self {
            app_server,
            codex_home,
        })
    }

    pub async fn resolve_thread(&mut self, target: &str) -> Result<ThreadId> {
        if let Ok(id) = ThreadId::from_string(target) {
            return Ok(id);
        }
        let thread = lookup(
            &mut self.app_server,
            &self.codex_home,
            target,
            &[SessionCollection::Active],
            &[
                crate::resume_source_kinds(/*include_non_interactive*/ true),
                Vec::new(),
            ],
            /*model_provider*/ None,
        )
        .await?
        .ok_or_else(|| eyre!("No active session found matching '{target}'."))?;
        ThreadId::from_string(&thread.id).wrap_err("app server returned an invalid thread ID")
    }

    pub async fn latest_thread(&mut self, cwd: Option<PathBuf>) -> Result<ThreadId> {
        use codex_app_server_protocol::ThreadListCwdFilter;
        use codex_app_server_protocol::ThreadListParams;
        use codex_app_server_protocol::ThreadSortKey;
        let page = self
            .app_server
            .thread_list(ThreadListParams {
                cursor: None,
                limit: Some(1),
                excluded_thread_ids: None,
                sort_key: Some(ThreadSortKey::CreatedAt),
                sort_direction: None,
                model_providers: None,
                source_kinds: Some(crate::resume_source_kinds(
                    /*include_non_interactive*/ true,
                )),
                originators: None,
                archived: Some(false),
                section_id: None,
                project_id: None,
                cwd: cwd.map(|cwd| ThreadListCwdFilter::One(cwd.to_string_lossy().into_owned())),
                use_state_db_only: false,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
                attached_to_thread_id: None,
            })
            .await?;
        let thread = page
            .data
            .first()
            .ok_or_else(|| eyre!("No saved session found."))?;
        ThreadId::from_string(&thread.id).wrap_err("app server returned an invalid thread ID")
    }

    /// Generate the input identity before sending. A lost reply includes that ID
    /// for reconciliation; callers must not automatically retry creation.
    pub async fn create(
        &mut self,
        thread_id: ThreadId,
        message: String,
        scheduled_at: i64,
    ) -> Result<QueuedSubmission> {
        create(
            &mut self.app_server,
            thread_id,
            uuid::Uuid::now_v7().to_string(),
            message,
            scheduled_at,
        )
        .await
    }

    pub async fn list(&mut self, thread_id: ThreadId) -> Result<Vec<QueuedSubmission>> {
        list(&mut self.app_server, thread_id).await
    }

    pub async fn result(&mut self, id: &str) -> Result<ThreadQueueResultResponse> {
        result(&mut self.app_server, id).await
    }

    pub async fn cancel(&mut self, id: &str) -> Result<String> {
        cancel(&mut self.app_server, id).await
    }

    pub async fn wait(&mut self, id: &str) -> Result<ThreadQueueResultResponse> {
        let mut missing_since = None;
        loop {
            let result = self.result(id).await?;
            if result
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status != TurnStatus::InProgress)
            {
                return Ok(result);
            }
            if result.queued_submission.is_none() && result.turn.is_none() {
                let since = missing_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(/*secs*/ 30) {
                    return Err(eyre!(
                        "Input {id} was admitted to turn {}, but that turn's history is unavailable. It may have been rolled back or publication may have failed; no input was resent.",
                        result.turn_id.as_deref().unwrap_or("unknown"),
                    ));
                }
            } else {
                missing_since = None;
            }
            tokio::time::sleep(Duration::from_secs(/*secs*/ 10)).await;
        }
    }

    /// A starter may request interruption; steering callers only stop observing.
    /// A different hosting server can reject this ordinary interrupt request.
    pub async fn interrupt_started(&mut self, id: &str) -> Result<String> {
        let result = self.result(id).await?;
        if result.started != Some(true)
            || result
                .turn
                .as_ref()
                .is_some_and(|turn| turn.status != TurnStatus::InProgress)
        {
            return Ok(format!(
                "Stopped observing input {id}; accepted input may still execute."
            ));
        }
        let thread_id = result
            .thread_id
            .ok_or_else(|| eyre!("input has no receiving thread"))?;
        let turn_id = result
            .turn_id
            .ok_or_else(|| eyre!("input has no receiving turn"))?;
        self.app_server.turn_interrupt(ThreadId::from_string(&thread_id)?, turn_id).await
            .wrap_err("could not interrupt through this connection; use the hosting thread's interrupt controls")?;
        Ok(format!("Requested interruption for input {id}."))
    }
}

pub(crate) async fn create(
    app_server: &mut AppServerSession,
    thread_id: ThreadId,
    id: String,
    message: String,
    scheduled_at: i64,
) -> Result<QueuedSubmission> {
    let request_id = app_server.next_request_id();
    let response: ThreadQueueAddResponse = app_server.request_handle().request_typed(
        ClientRequest::ThreadQueueSchedule {
            request_id,
            params: ThreadQueueScheduleParams {
                thread_id: thread_id.to_string(),
                client_user_message_id: id.clone(),
                input: vec![UserInput::Text { text: message, text_elements: Vec::new() }],
                scheduled_at,
            },
        },
    ).await.wrap_err_with(|| format!(
        "Schedule creation was not confirmed. Input ID: {id}. Inspect `codex schedule result {id}` or the pending list before creating it again"
    ))?;
    Ok(response.queued_submission)
}

pub(crate) async fn list(
    app_server: &mut AppServerSession,
    thread_id: ThreadId,
) -> Result<Vec<QueuedSubmission>> {
    let mut cursor = None;
    let mut items = Vec::new();
    loop {
        let request_id = app_server.next_request_id();
        let page: ThreadQueueListResponse = app_server
            .request_handle()
            .request_typed(ClientRequest::ThreadQueueList {
                request_id,
                params: ThreadQueueListParams {
                    thread_id: thread_id.to_string(),
                    cursor,
                    limit: Some(100),
                },
            })
            .await?;
        items.extend(
            page.data
                .into_iter()
                .filter(|item| item.scheduled_at.is_some()),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(items);
        }
    }
}

pub(crate) async fn result(
    app_server: &mut AppServerSession,
    id: &str,
) -> Result<ThreadQueueResultResponse> {
    let request_id = app_server.next_request_id();
    Ok(app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadQueueResult {
            request_id,
            params: ThreadQueueResultParams {
                queued_submission_id: id.to_string(),
            },
        })
        .await?)
}

pub(crate) async fn cancel(app_server: &mut AppServerSession, id: &str) -> Result<String> {
    let pending = result(app_server, id).await?;
    cancel_pending(app_server, id, pending).await
}

pub(crate) async fn cancel_pending(
    app_server: &mut AppServerSession,
    id: &str,
    pending: ThreadQueueResultResponse,
) -> Result<String> {
    let Some(item) = pending.queued_submission else {
        return Ok(format!(
            "Input {id} has already been admitted; use the thread's interrupt controls to stop it."
        ));
    };
    let thread_id = pending
        .thread_id
        .ok_or_else(|| eyre!("pending input has no thread ID"))?;
    let request_id = app_server.next_request_id();
    let response: ThreadQueueDeleteResponse = app_server
        .request_handle()
        .request_typed(ClientRequest::ThreadQueueDelete {
            request_id,
            params: ThreadQueueDeleteParams {
                thread_id,
                queued_submission_id: id.to_string(),
            },
        })
        .await?;
    if !response.deleted {
        return Ok(format!(
            "Input {id} is no longer pending; it may already have been admitted."
        ));
    }
    if item
        .scheduled_at
        .is_some_and(|due| due > chrono::Utc::now().timestamp())
    {
        Ok(format!("Cancelled input {id}."))
    } else {
        Ok(format!(
            "Removed pending input {id}. Due dispatch may already have begun; it may still run."
        ))
    }
}
