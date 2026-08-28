//! Exact-identity registration for independently controlled user-shell commands.

use codex_protocol::protocol::UserShellCommandResponseHandling;
use codex_utils_path_uri::PathUri;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecProcessManager;
use super::UserShellCommandEntry;
use super::UserShellCommandRetirement;
use super::process_manager::reserve_process_id;

impl UnifiedExecProcessManager {
    pub(crate) async fn register_user_shell_command(
        &self,
        call_id: String,
        submission_id: u64,
        command: String,
        cwd: PathUri,
        response_handling: UserShellCommandResponseHandling,
        cancellation_token: CancellationToken,
    ) -> i32 {
        let mut store = self.process_store.lock().await;
        if store.user_shell_shutdown {
            cancellation_token.cancel();
        }
        let process_id = reserve_process_id(&mut store);
        store.user_shell_commands.insert(
            process_id,
            UserShellCommandEntry {
                call_id,
                process_id,
                submission_id,
                command,
                cwd,
                response_handling,
                cancellation_token,
            },
        );
        process_id
    }

    /// Removes an exact process registration, arbitrating completion against stop under one lock.
    pub(crate) async fn retire_user_shell_command(
        &self,
        process_id: i32,
        call_id: &str,
    ) -> Option<UserShellCommandRetirement> {
        let mut store = self.process_store.lock().await;
        let entry = store
            .user_shell_commands
            .get(&process_id)
            .filter(|entry| entry.call_id == call_id)?;
        let retirement = if entry.cancellation_token.is_cancelled() {
            UserShellCommandRetirement::Stopped
        } else {
            UserShellCommandRetirement::Completed
        };
        store.user_shell_commands.remove(&process_id);
        store.reserved_process_ids.remove(&process_id);
        Some(retirement)
    }

    pub(crate) async fn unregister_user_shell_command(&self, process_id: i32, call_id: &str) {
        let mut store = self.process_store.lock().await;
        if store
            .user_shell_commands
            .get(&process_id)
            .is_none_or(|entry| entry.call_id != call_id)
        {
            return;
        }
        store.user_shell_commands.remove(&process_id);
        store.reserved_process_ids.remove(&process_id);
    }

    /// Cancels current commands and fences commands still preparing their local shell.
    /// This requests cancellation only; it does not acknowledge child process exit.
    pub(crate) async fn shutdown_user_shell_commands(&self) {
        let mut store = self.process_store.lock().await;
        store.user_shell_shutdown = true;
        for entry in store.user_shell_commands.values() {
            entry.cancellation_token.cancel();
        }
        store.pending_user_shell_submissions.clear();
        drop(store);
        self.user_shell_submission_changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "user_shell_registry_tests.rs"]
mod tests;
