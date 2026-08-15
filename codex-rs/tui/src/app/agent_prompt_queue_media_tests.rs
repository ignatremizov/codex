//! A lossy transcript projection must never become an editable replacement for queued media.

use super::*;
use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::ImageReference;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;

#[test]
fn queued_media_keeps_its_source_and_blocks_lossy_composer_recovery() {
    let cases = [
        (
            UserInput::Audio {
                url: "data:audio/wav;base64,YQ==".to_string(),
            },
            true,
        ),
        (
            UserInput::LocalAudio {
                path: "remote-voice.wav".into(),
            },
            true,
        ),
        (
            UserInput::Image {
                image: ImageReference::File {
                    file_id: "file-queued-image".to_string(),
                },
                detail: None,
            },
            true,
        ),
        (
            UserInput::LocalImage {
                path: "remote-image.png".into(),
                detail: None,
            },
            true,
        ),
        (
            UserInput::Image {
                image: ImageReference::Inline {
                    url: "data:image/png;base64,YQ==".to_string(),
                },
                detail: None,
            },
            false,
        ),
    ];
    for (media, uneditable) in cases {
        let input = vec![
            UserInput::Text {
                text: "inspect the original attachment".to_string(),
                text_elements: Vec::new(),
            },
            media,
        ];
        let prompt = queued_agent_prompt_from_entry(AgentQueueEntry {
            id: Uuid::now_v7().to_string(),
            source_thread_id: ThreadId::new().to_string(),
            target_thread_id: ThreadId::new().to_string(),
            input: input.clone(),
            prompt_preview: "inspect the original attachment".to_string(),
            response_handling: AgentResponseHandling::new(
                /*commentary*/ false,
                AgentFinalResponseHandling::Passive,
                /*target_messages*/ false,
                /*queue_input*/ true,
            ),
            authored_selector: Some("worker".to_string()),
        })
        .expect("queued source remains readable");
        assert_eq!(prompt.input, input);
        assert_eq!(prompt.has_uneditable_attachments(), uneditable);
    }
}
