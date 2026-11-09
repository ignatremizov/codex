use super::App;
use crate::app_server_session::AppServerSession;
use crate::scheduled_input::ScheduleAction;
use crate::scheduled_input::format_schedule_time;
use crate::scheduled_input::schedule_summary;
use crate::scheduled_input::scheduled_result_text;
use crate::session_schedule_commands;
use crate::text_formatting::truncate_text;
use codex_protocol::ThreadId;

impl App {
    pub(super) async fn schedule_input(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        backend_id: Option<crate::backend_id::BackendId>,
        action: ScheduleAction,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id)
            || backend_id.is_some_and(|id| id != app_server.backend_id())
        {
            self.chat_widget.add_error_message(
                "Focus changed before scheduling; return to the original thread and recall /schedule to retry.".into(),
            );
            return;
        }
        if matches!(
            &action,
            ScheduleAction::Create { .. } | ScheduleAction::Cancel(_)
        ) && let Err(error) = self
            .ensure_agent_control_admission(thread_id, /*target*/ None)
            .await
        {
            self.chat_widget.add_error_message(format!("{error:#}"));
            return;
        }
        let result: color_eyre::eyre::Result<String> = async {
            match action {
                ScheduleAction::Create {
                    scheduled_at,
                    message,
                } => {
                    let id = uuid::Uuid::now_v7().to_string();
                    let item = session_schedule_commands::create(
                        app_server,
                        thread_id,
                        id,
                        message,
                        scheduled_at,
                    )
                    .await?;
                    Ok(format!(
                        "Scheduled input {} for {}. Fallback auth: {}.",
                        item.id,
                        format_schedule_time(scheduled_at),
                        item.fallback_profile.as_deref().unwrap_or("server profile")
                    ))
                }
                ScheduleAction::List => {
                    let items = session_schedule_commands::list(app_server, thread_id).await?;
                    if items.is_empty() {
                        Ok("No scheduled input is pending.".into())
                    } else {
                        Ok(items
                            .iter()
                            .map(schedule_summary)
                            .collect::<Vec<_>>()
                            .join("\n"))
                    }
                }
                ScheduleAction::Cancel(id) => {
                    let pending = session_schedule_commands::result(app_server, &id).await?;
                    if pending.queued_submission.is_some() {
                        let target = pending.thread_id.as_deref().ok_or_else(|| {
                            color_eyre::eyre::eyre!("pending input has no thread ID")
                        })?;
                        let target = ThreadId::from_string(target)?;
                        self.ensure_agent_control_admission(thread_id, Some(target))
                            .await?;
                    }
                    session_schedule_commands::cancel_pending(app_server, &id, pending).await
                }
                ScheduleAction::Result(id) => {
                    let result = session_schedule_commands::result(app_server, &id).await?;
                    if let Some(item) = &result.queued_submission {
                        return Ok(format!("Pending: {}", schedule_summary(item)));
                    }
                    let status = result
                        .turn
                        .as_ref()
                        .map(|turn| format!("{:?}", turn.status))
                        .unwrap_or_else(|| "history unavailable or publication pending".into());
                    Ok(format!(
                        "Input {id}: {status}\n{}",
                        scheduled_result_text(&result).unwrap_or("")
                    ))
                }
            }
        }
        .await;
        match result {
            Ok(message) => self.chat_widget.add_info_message(message, /*hint*/ None),
            Err(error) => self.chat_widget.add_error_message(format!("{error:#}")),
        }
        self.refresh_scheduled_input(app_server, thread_id).await;
    }

    pub(super) async fn refresh_scheduled_input(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        match session_schedule_commands::list(app_server, thread_id).await {
            Ok(items) => {
                let mut preview = items
                    .iter()
                    .take(3)
                    .map(|item| truncate_text(&schedule_summary(item), 240))
                    .collect::<Vec<_>>();
                if items.len() > 3 {
                    preview.push(format!("{} more · /schedule list", items.len() - 3));
                }
                self.chat_widget.set_scheduled_input_preview(preview);
            }
            Err(error) => tracing::debug!(%error, "could not refresh scheduled input"),
        }
    }
}
