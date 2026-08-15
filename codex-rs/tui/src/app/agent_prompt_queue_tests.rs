use super::super::agent_preview::AGENT_PREVIEW_MAX_CHARS;
use super::*;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use crate::render::renderable::Renderable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc::unbounded_channel;

fn render_normalized(params: SelectionViewParams) -> String {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let view = ListSelectionView::new(
        params,
        AppEventSender::new(tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    let width = 120;
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);

    (0..height)
        .filter_map(|row| {
            let line = (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<Vec<_>>()
                .concat();
            let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn queued_prompt_preview_normalizes_and_bounds_text() {
    let message = UserMessage::from(format!(
        "  first\n\tsecond  {}",
        "x".repeat(AGENT_PREVIEW_MAX_CHARS)
    ));

    let preview = queued_agent_prompt_preview(&message);

    assert!(preview.starts_with("first second "));
    assert!(preview.ends_with('…'));
    assert_eq!(preview.chars().count(), AGENT_PREVIEW_MAX_CHARS);
}

#[test]
fn queued_prompt_preview_describes_attachment_only_input() {
    let mut message = UserMessage::from("");
    message.remote_image_urls = vec![
        "data:image/png;base64,AA==".to_string(),
        "https://example.test/image.png".to_string(),
    ];

    assert_eq!(queued_agent_prompt_preview(&message), "2 images");
}

#[test]
fn queued_prompt_rows_snapshot_visible_content() {
    let source_thread_id = ThreadId::new();
    let target_thread_id = ThreadId::new();
    let mut image_message = UserMessage::from("");
    image_message.remote_image_urls = vec![
        "data:image/png;base64,AA==".to_string(),
        "https://example.test/image.png".to_string(),
    ];
    let queued = VecDeque::from([
        QueuedAgentPrompt {
            id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("valid prompt id"),
            source_thread_id,
            target: "2".to_string(),
            authored_selector: "2".to_string(),
            target_thread_id,
            user_message: UserMessage::from("Review the final response ordering."),
            response_handling: Some(AgentResponseHandling::Wake),
        },
        QueuedAgentPrompt {
            id: Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("valid prompt id"),
            source_thread_id,
            target: "2".to_string(),
            authored_selector: "2".to_string(),
            target_thread_id,
            user_message: image_message,
            response_handling: None,
        },
    ]);
    let rendered = queued_agent_prompt_rows(Some(&queued))
        .into_iter()
        .map(|row| format!("{} · {}", row.response, row.preview))
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r"
    w:f · Review the final response ordering.
    passive · 2 images
    ");
}

#[test]
fn queued_prompt_removal_uses_stable_id_after_prior_item_changes() {
    let source_thread_id = ThreadId::new();
    let target_thread_id = ThreadId::new();
    let prompt = |id: &str, text: &str| QueuedAgentPrompt {
        id: Uuid::parse_str(id).expect("valid prompt id"),
        source_thread_id,
        target: "2".to_string(),
        authored_selector: "2".to_string(),
        target_thread_id,
        user_message: UserMessage::from(text),
        response_handling: None,
    };
    let third_id =
        Uuid::parse_str("00000000-0000-0000-0000-000000000003").expect("valid prompt id");
    let mut queued = VecDeque::from([
        prompt("00000000-0000-0000-0000-000000000001", "first prompt"),
        prompt("00000000-0000-0000-0000-000000000002", "second prompt"),
        prompt("00000000-0000-0000-0000-000000000003", "third prompt"),
    ]);
    queued.pop_front();

    let removed = take_queued_agent_prompt_by_id(
        &mut queued,
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").expect("valid prompt id"),
    )
    .expect("selected prompt should still be present");

    assert_eq!(removed.user_message, UserMessage::from("second prompt"));
    assert_eq!(
        queued.iter().map(|prompt| prompt.id).collect::<Vec<_>>(),
        vec![third_id]
    );
}

#[test]
fn closing_a_thread_removes_queues_it_targets_or_authored() {
    let closed_thread_id = ThreadId::new();
    let sibling_thread_id = ThreadId::new();
    let other_source_thread_id = ThreadId::new();
    let other_target_thread_id = ThreadId::new();
    let prompt = |id: &str, source_thread_id: ThreadId, target_thread_id: ThreadId, text: &str| {
        QueuedAgentPrompt {
            id: Uuid::parse_str(id).expect("valid prompt id"),
            source_thread_id,
            target: target_thread_id.to_string(),
            authored_selector: target_thread_id.to_string(),
            target_thread_id,
            user_message: UserMessage::from(text),
            response_handling: None,
        }
    };
    let retained_sibling_prompt = prompt(
        "00000000-0000-0000-0000-000000000002",
        other_source_thread_id,
        sibling_thread_id,
        "retain sibling prompt",
    );
    let retained_other_prompt = prompt(
        "00000000-0000-0000-0000-000000000004",
        other_source_thread_id,
        other_target_thread_id,
        "retain unrelated prompt",
    );
    let mut queues = HashMap::from([
        (
            sibling_thread_id,
            VecDeque::from([
                prompt(
                    "00000000-0000-0000-0000-000000000001",
                    closed_thread_id,
                    sibling_thread_id,
                    "remove closed source prompt",
                ),
                retained_sibling_prompt.clone(),
            ]),
        ),
        (
            closed_thread_id,
            VecDeque::from([prompt(
                "00000000-0000-0000-0000-000000000003",
                other_source_thread_id,
                closed_thread_id,
                "remove closed target prompt",
            )]),
        ),
        (
            other_target_thread_id,
            VecDeque::from([retained_other_prompt.clone()]),
        ),
    ]);

    remove_queued_agent_prompts_for_thread(&mut queues, closed_thread_id);

    assert_eq!(
        queues,
        HashMap::from([
            (sibling_thread_id, VecDeque::from([retained_sibling_prompt])),
            (
                other_target_thread_id,
                VecDeque::from([retained_other_prompt])
            ),
        ])
    );
}

#[test]
fn queued_prompt_view_renders_response_modes_and_explicit_actions() {
    let target_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    let first_prompt_id =
        Uuid::parse_str("00000000-0000-0000-0000-000000000011").expect("valid prompt id");
    let second_prompt_id =
        Uuid::parse_str("00000000-0000-0000-0000-000000000012").expect("valid prompt id");
    let queue = render_normalized(agent_prompt_queue_view_params(
        target_thread_id,
        "Hopper [reviewer]".to_string(),
        vec![
            QueuedAgentPromptRow {
                prompt_id: first_prompt_id,
                preview: "Review the final response ordering.".to_string(),
                response: "w:f",
            },
            QueuedAgentPromptRow {
                prompt_id: second_prompt_id,
                preview: "Check the pagination projection.".to_string(),
                response: "passive",
            },
        ],
        /*initial_selected_idx*/ None,
    ));
    let actions = render_normalized(queued_agent_prompt_actions_view_params(
        target_thread_id,
        first_prompt_id,
        "Review the final response ordering.".to_string(),
    ));

    insta::assert_snapshot!(format!("queue:\n{queue}\n\nactions:\n{actions}"), @r"
    queue:
    Queued for Hopper [reviewer]
    00000000-0000-0000-0000-000000000002
    › 1. Review the final response ordering. w:f
    2. Check the pagination projection. passive
    Enter edits · Tab opens actions
    Press enter to confirm or esc to go back

    actions:
    Queued follow-up
    Review the final response ordering.
    › 1. Edit Restore this prompt to the composer
    2. Remove Delete this queued prompt
    Press enter to confirm or esc to go back
    ");
}
