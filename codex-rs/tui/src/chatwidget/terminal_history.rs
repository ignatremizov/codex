//! Bounded terminal display history and cold replay cleanup.
use super::*;

const MAX_COMPLETED_TERMINALS: usize = 16;

impl ChatWidget {
    pub(super) fn track_unified_exec_process_end(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
    ) {
        let key = process_id.unwrap_or(call_id);
        let Some(index) = self
            .unified_exec_processes
            .iter()
            .position(|process| process.key == key && process.call_id == call_id)
        else {
            return;
        };
        let process = self.unified_exec_processes.remove(index);
        self.completed_unified_exec_processes
            .push_back(CompletedUnifiedExecProcess {
                key: process.key,
                command_display: process.command_display,
            });
        while self.completed_unified_exec_processes.len() > MAX_COMPLETED_TERMINALS {
            self.completed_unified_exec_processes.pop_front();
        }
        self.sync_unified_exec_footer();
    }

    /// Retains command audit cells without restoring ownership of historical processes.
    pub(crate) fn finalize_replayed_process_tracking(&mut self) {
        if self
            .transcript
            .active_cell
            .as_ref()
            .and_then(|cell| cell.as_any().downcast_ref::<ExecCell>())
            .is_some_and(ExecCell::is_active)
        {
            self.finalize_active_cell_as_failed();
        }
        self.running_commands.clear();
        self.suppressed_exec_calls.clear();
        self.last_unified_wait = None;
        self.clear_unified_exec_wait_tracking();
        self.clear_status_countdown();
        self.unified_exec_processes.clear();
        self.completed_unified_exec_processes.clear();
        self.sync_unified_exec_footer();
        self.update_task_running_state();
    }
}
