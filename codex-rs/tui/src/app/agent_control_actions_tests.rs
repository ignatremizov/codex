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

fn action_matrix(state: AgentControlTargetState) -> String {
    AgentControlActionKind::ALL
        .into_iter()
        .map(|kind| {
            let availability = kind.disabled_reason(state).unwrap_or("enabled");
            format!("{}: {availability}", kind.label())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn action_availability_snapshot() {
    let running_child = action_matrix(AgentControlTargetState {
        is_current: false,
        is_primary: false,
        is_running: true,
        is_closed: false,
        needs_adoption: false,
        is_side_thread: false,
    });
    let closed_child = action_matrix(AgentControlTargetState {
        is_current: false,
        is_primary: false,
        is_running: false,
        is_closed: true,
        needs_adoption: false,
        is_side_thread: false,
    });
    let current_main = action_matrix(AgentControlTargetState {
        is_current: true,
        is_primary: true,
        is_running: false,
        is_closed: false,
        needs_adoption: false,
        is_side_thread: false,
    });
    let transferred_child = action_matrix(AgentControlTargetState {
        is_current: false,
        is_primary: false,
        is_running: false,
        is_closed: false,
        needs_adoption: true,
        is_side_thread: false,
    });
    let side_thread = action_matrix(AgentControlTargetState {
        is_current: false,
        is_primary: false,
        is_running: true,
        is_closed: false,
        needs_adoption: true,
        is_side_thread: true,
    });

    insta::assert_snapshot!(format!(
        "running child:\n{running_child}\n\nclosed child:\n{closed_child}\n\ncurrent main:\n{current_main}\n\ntransferred child:\n{transferred_child}\n\nside thread:\n{side_thread}"
    ), @r"
    running child:
    Inspect transcript: enabled
    Prompt: enabled
    Queue follow-up: enabled
    Interrupt turn: enabled
    Resume agent: Agent is already open.
    Observe response: enabled
    Close agent: enabled

    closed child:
    Inspect transcript: enabled
    Prompt: enabled
    Queue follow-up: enabled
    Interrupt turn: Agent is closed.
    Resume agent: enabled
    Observe response: Resume the closed agent first.
    Close agent: Agent is already closed.

    current main:
    Inspect transcript: enabled
    Prompt: Use the normal composer for the current agent.
    Queue follow-up: Use the normal composer for the current agent.
    Interrupt turn: Use the normal interrupt shortcut for the current agent.
    Resume agent: Agent is already open.
    Observe response: An agent cannot observe itself.
    Close agent: Main cannot be closed.

    transferred child:
    Inspect transcript: enabled
    Prompt: Resume by UUID to adopt this agent first.
    Queue follow-up: Resume by UUID to adopt this agent first.
    Interrupt turn: Agent is not controlled by this root.
    Resume agent: enabled
    Observe response: Agent is not controlled by this root.
    Close agent: Agent is not controlled by this root.

    side thread:
    Inspect transcript: enabled
    Prompt: Use the normal composer inside this side conversation.
    Queue follow-up: Use the normal composer inside this side conversation.
    Interrupt turn: Switch to the side conversation to interrupt it.
    Resume agent: Side conversations use the normal TUI lifecycle.
    Observe response: Side conversations do not use agent response observation.
    Close agent: Side conversations use the normal TUI lifecycle.
    ");
}

#[test]
fn contextual_controls_render_labels_disabled_reasons_and_confirmation_hint() {
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    let rendered = render_normalized(agent_control_actions_view_params(
        thread_id,
        "2",
        "Hopper [reviewer]".to_string(),
        AgentControlTargetState {
            is_current: false,
            is_primary: false,
            is_running: true,
            is_closed: false,
            needs_adoption: false,
            is_side_thread: false,
        },
    ));

    insta::assert_snapshot!(rendered, @r"
    Controls: Hopper [reviewer]
    00000000-0000-0000-0000-000000000002
    › 1. Inspect transcript Open its canonical transcript over this pane
    2. Prompt Send user input without switching threads
    3. Queue follow-up Run after the active turn, or immediately while idle
    4. Interrupt turn Stop the active turn, optionally with a follow-up
    Resume agent (disabled) Reopen this controlled agent (disabled: Agent is already open.)
    5. Observe response Choose passive, wake, or presentation delivery
    6. Close agent End the agent runtime and revoke observation
    Prepared commands return to the current composer for confirmation.
    Press enter to confirm or esc to go back
    ");
}

#[test]
fn inspect_action_keeps_thread_switch_distinct() {
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut view = ListSelectionView::new(
        agent_control_actions_view_params(
            thread_id,
            "2",
            "Hopper [reviewer]".to_string(),
            AgentControlTargetState {
                is_current: false,
                is_primary: false,
                is_running: true,
                is_closed: false,
                needs_adoption: false,
                is_side_thread: false,
            },
        ),
        AppEventSender::new(tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );

    crate::bottom_pane::BottomPaneView::handle_key_event(
        &mut view,
        crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter),
    );

    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::InspectAgentTranscript(selected_thread_id))
            if selected_thread_id == thread_id
    ));
}
