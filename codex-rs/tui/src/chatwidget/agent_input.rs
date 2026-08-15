//! Preserve the ordinary input conversion and remote-media boundary for agent dispatch.

use super::*;

impl ChatWidget {
    pub(crate) fn accept_agent_input_without_cursor(&mut self) {
        self.safety_buffering_prompt = None;
        self.input_queue.user_turn_pending_start = false;
        self.clear_safety_buffering();
        self.update_task_running_state();
    }
    pub(crate) async fn agent_user_inputs_from_message(
        &self,
        message: &UserMessage,
    ) -> Result<Vec<UserInput>, String> {
        let mut items = self.user_inputs_from_message(message);
        if self.snapshot_local_images && !message.local_images.is_empty() {
            let images = message.local_images.clone();
            let remote_bytes = message.remote_image_urls.iter().map(String::len).sum();
            let permit = image_submission::IMAGE_PREPARATION.lock().await;
            let prepared = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                image_submission::prepare_images(images, remote_bytes)
            })
            .await
            .map_err(|error| format!("Failed to prepare agent images: {error}"))??;
            items.retain(|item| !matches!(item, UserInput::LocalImage { .. }));
            let image_index = message.remote_image_urls.len();
            items.splice(image_index..image_index, prepared);
        }
        Ok(items)
    }
}
