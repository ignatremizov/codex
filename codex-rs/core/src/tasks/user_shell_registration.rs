//! Cancellation-safe ownership of a shell submission through final result delivery.

use crate::session::session::Session;
use crate::unified_exec::UserShellCommandRetirement;
use std::sync::Arc;
use tracing::error;

pub(super) struct UserShellCommandRegistration {
    pub(super) session: Arc<Session>,
    pub(super) process_id: Option<i32>,
    pub(super) call_id: Option<String>,
    pub(super) submission_id: Option<u64>,
}

impl UserShellCommandRegistration {
    pub(super) async fn unregister(&mut self) {
        if let (Some(process_id), Some(call_id)) = (self.process_id, self.call_id.clone()) {
            self.session
                .services
                .unified_exec_manager
                .unregister_user_shell_command(process_id, &call_id)
                .await;
            self.process_id = None;
            self.call_id = None;
        }
        self.release_submission().await;
    }

    pub(super) async fn retire_process_for_delivery(&mut self) -> UserShellCommandRetirement {
        let (Some(process_id), Some(call_id)) = (self.process_id, self.call_id.as_deref()) else {
            error!("user shell process registration disappeared before result delivery");
            return UserShellCommandRetirement::Stopped;
        };
        let retirement = self
            .session
            .services
            .unified_exec_manager
            .retire_user_shell_command(process_id, call_id)
            .await;
        if retirement.is_some() {
            self.process_id = None;
            self.call_id = None;
        }
        retirement.unwrap_or_else(|| {
            error!("user shell process registry rejected exact result-delivery retirement");
            UserShellCommandRetirement::Stopped
        })
    }

    pub(super) async fn release_submission(&mut self) {
        let Some(submission_id) = self.submission_id else {
            return;
        };
        self.session
            .services
            .unified_exec_manager
            .release_user_shell_submission(submission_id)
            .await;
        self.submission_id = None;
    }
}

impl Drop for UserShellCommandRegistration {
    fn drop(&mut self) {
        let process = self.process_id.take().zip(self.call_id.take());
        let submission_id = self.submission_id.take();
        if process.is_none() && submission_id.is_none() {
            return;
        }
        let session = Arc::clone(&self.session);
        self.session.services.runtime_handle.spawn(async move {
            if let Some((process_id, call_id)) = process {
                session
                    .services
                    .unified_exec_manager
                    .unregister_user_shell_command(process_id, &call_id)
                    .await;
            }
            if let Some(submission_id) = submission_id {
                session
                    .services
                    .unified_exec_manager
                    .release_user_shell_submission(submission_id)
                    .await;
            }
        });
    }
}
