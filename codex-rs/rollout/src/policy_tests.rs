use codex_extension_items::ExtensionItem;
use codex_extension_items::image_generation::GeneratedImageRef;
use codex_extension_items::image_generation::ImageGenerationItem;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::new_attributed_agent_message_response_item_id;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use std::time::Duration;

use super::persisted_event_msg;

fn image_item(image: GeneratedImageRef) -> ImageGenerationItem {
    ImageGenerationItem {
        id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("paint a blue whale".to_string()),
        image,
        transparent_background: Some(false),
        failure: None,
        saved_path: None,
        input_images: Vec::new(),
        imagegen_request_id: None,
        generation_id: None,
    }
}

#[test]
fn paginated_image_generation_preserves_file_reference() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::Extension(ExtensionItem::ImageGeneration(image_item(
            GeneratedImageRef::File {
                file_id: "file-1".to_string(),
            },
        ))),
        started_at_ms: Some(1),
        completed_at_ms: 2,
    });

    let persisted = persisted_event_msg(&event, ThreadHistoryMode::Paginated)
        .expect("expected persisted image-generation item");
    let EventMsg::ItemCompleted(persisted) = persisted.as_ref() else {
        panic!("expected persisted image-generation item");
    };
    let TurnItem::Extension(ExtensionItem::ImageGeneration(image)) = &persisted.item else {
        panic!("expected image-generation extension item");
    };

    assert_eq!(
        image,
        &image_item(GeneratedImageRef::File {
            file_id: "file-1".to_string(),
        })
    );
}

#[test]
fn legacy_image_generation_preserves_file_reference() {
    let event = EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
        call_id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("paint a blue whale".to_string()),
        image: GeneratedImageRef::File {
            file_id: "file-1".to_string(),
        },
        transparent_background: Some(false),
        failure: None,
        saved_path: None,
        input_images: Vec::new(),
    });

    let persisted = persisted_event_msg(&event, ThreadHistoryMode::Legacy)
        .expect("expected persisted legacy image-generation event");
    let EventMsg::ImageGenerationEnd(persisted) = persisted.as_ref() else {
        panic!("expected persisted legacy image-generation event");
    };

    assert_eq!(
        persisted.image,
        GeneratedImageRef::File {
            file_id: "file-1".to_string(),
        }
    );
}

#[test]
fn inline_image_generation_result_is_preserved() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::Extension(ExtensionItem::ImageGeneration(image_item(
            GeneratedImageRef::Inline {
                result: "base64-image".to_string(),
            },
        ))),
        started_at_ms: Some(1),
        completed_at_ms: 2,
    });

    let persisted = persisted_event_msg(&event, ThreadHistoryMode::Paginated)
        .expect("expected persisted inline image-generation item");
    let EventMsg::ItemCompleted(persisted) = persisted.as_ref() else {
        panic!("expected persisted inline image-generation item");
    };
    let TurnItem::Extension(ExtensionItem::ImageGeneration(image)) = &persisted.item else {
        panic!("expected inline image-generation extension item");
    };

    assert_eq!(
        image,
        &image_item(GeneratedImageRef::Inline {
            result: "base64-image".to_string(),
        })
    );
}

#[test]
fn attributed_agent_input_presentation_is_persisted_in_every_history_mode() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "turn-1".to_string(),
        item: TurnItem::AgentMessage(AgentMessageItem {
            id: new_attributed_agent_message_response_item_id().to_string(),
            content: vec![AgentMessageContent::Text {
                text: "Agent message from `agent-id`:\n\nReview this.".to_string(),
            }],
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
            sub_agent_completion: None,
        }),
        started_at_ms: None,
        completed_at_ms: 0,
    });

    assert_eq!(
        [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated,]
            .map(|mode| persisted_event_msg(&event, mode).is_some()),
        [true, true]
    );
}

#[test]
fn user_shell_completion_is_persisted_in_every_history_mode() {
    let event = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::new(),
        turn_id: "shell-turn".to_string(),
        item: TurnItem::CommandExecution(CommandExecutionItem {
            model_context: None,
            sandbox_type: None,
            id: "shell-turn".to_string(),
            deadline_at_ms: None,
            plugin_id: None,
            script_path: None,
            process_id: Some("12345".to_string()),
            command: vec!["sleep".to_string(), "60".to_string()],
            cwd: test_path_buf("/tmp").abs().into(),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::UserShell,
            user_shell_response_handling: None,
            interaction_input: None,
            status: CommandExecutionStatus::Completed,
            aggregated_output: Some(String::new()),
            exit_code: Some(0),
            duration: Some(Duration::from_secs(1)),
        }),
        started_at_ms: Some(100),
        completed_at_ms: 200,
    });

    assert_eq!(
        [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated]
            .map(|mode| persisted_event_msg(&event, mode).is_some()),
        [true, true]
    );

    // Detached activity is retained in a bounded replay suffix but cannot replace its semantic
    // compaction boundary with an obsolete user-turn scan or a fabricated turn completion.
    let completion = codex_history::RolloutItem::EventMsg(event);
    let checkpoint =
        codex_history::RolloutItem::Compacted(Box::new(codex_history::CompactedItem {
            replacement_history: Some(Vec::new()),
            window_number: Some(1),
            ..Default::default()
        }));
    let mut scan = crate::ModelContextScan::default();
    assert_eq!(
        scan.push(completion.clone()),
        crate::ModelContextScanProgress::Continue
    );
    assert_eq!(
        scan.push(checkpoint.clone()),
        crate::ModelContextScanProgress::Complete
    );
    assert_eq!(
        serde_json::to_value(scan.finish()).expect("serialize replay scan"),
        serde_json::to_value(vec![checkpoint, completion]).expect("serialize expected replay")
    );
}
