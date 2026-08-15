//! Canonical transcript inspection from the `/agent` control pane.

use super::App;
use crate::app_server_session::AppServerSession;
use crate::pager_overlay::Overlay;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::load_session_transcript;
use crate::tui;
use codex_protocol::ThreadId;

impl App {
    pub(super) async fn inspect_agent_transcript(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        let raw_reasoning_visibility = if self.config.show_raw_agent_reasoning {
            RawReasoningVisibility::Visible
        } else {
            RawReasoningVisibility::Hidden
        };
        let transcript = load_session_transcript(
            app_server,
            thread_id,
            raw_reasoning_visibility,
            Some(&self.config),
        )
        .await;
        let cells = match transcript {
            Ok(cells) => cells,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to inspect agent {thread_id}: {error}"));
                return;
            }
        };

        let _ = tui.enter_alt_screen();
        self.overlay = Some(Overlay::new_transcript(cells, self.keymap.pager.clone()));
        tui.frame_requester().schedule_frame();
    }
}
