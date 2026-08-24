//! Exact-identity registration for independently controlled user-shell commands.

use codex_utils_path_uri::PathUri;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecProcessManager;
use super::UserShellCommandEntry;
use super::process_manager::reserve_process_id;

impl UnifiedExecProcessManager {
    pub(crate) async fn register_user_shell_command(
        &self,
        call_id: String,
        command: String,
        cwd: PathUri,
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
                command,
                cwd,
                cancellation_token,
            },
        );
        process_id
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
    }
}

#[cfg(test)]
#[path = "user_shell_registry_tests.rs"]
mod tests;
