use ratatui::style::Stylize as _;

use super::super::ThreadEventChannel;
use super::*;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use crate::bottom_pane::SelectionRowDisplay;
use crate::multi_agents::SubAgentActivityDisplay;
use codex_app_server_protocol::AgentAlias;
use codex_app_server_protocol::AgentAliasState;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserAgentForkMode;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ReasoningEffort;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use tokio::sync::mpsc::unbounded_channel;
use unicode_width::UnicodeWidthStr;

async fn adaptive_agent_layout_view(initial_selected_idx: Option<usize>) -> ListSelectionView {
    let mut app = super::super::test_support::make_test_app().await;
    let main_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
    let child_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    app.primary_thread_id = Some(main_thread_id);
    app.active_thread_id = Some(main_thread_id);
    app.agent_navigation.upsert(
        main_thread_id,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.agent_navigation.upsert(
        child_thread_id,
        Some("Hume".to_string()),
        Some("reviewer".to_string()),
        /*is_closed*/ false,
    );
    app.agent_navigation
        .set_parent_thread_id(child_thread_id, Some(main_thread_id));
    app.agent_navigation
        .record_sub_agent_activity(SubAgentActivityDisplay {
            thread_id: child_thread_id,
            agent_path: "/root/reviewer".to_string(),
            is_running_hint: false,
        });

    let main_channel = ThreadEventChannel::new(/*capacity*/ 1);
    main_channel.store.lock().await.set_turns(vec![Turn {
        id: "turn-main".to_string(),
        items: vec![ThreadItem::UserAgentControl {
            id: "spawn-child".to_string(),
            action: UserAgentControlAction::Spawn,
            authored_selector: Some("reviewer".to_string()),
            target_thread_id: Some(child_thread_id.to_string()),
            previous_owner_session_id: None,
            new_owner_session_id: None,
            agent_ref: Some("2".to_string()),
            nickname: Some("Hume".to_string()),
            role: Some("reviewer".to_string()),
            model: Some("gpt-5.6-luna".to_string()),
            reasoning_effort: Some(ReasoningEffort::Medium),
            prompt_preview: Some("Inspect adaptive agent pane rendering.".to_string()),
            resumed_target: false,
            fork_mode: Some(UserAgentForkMode::None),
            observe_commentary: Some(false),
            final_response: None,
            target_messages: Some(false),
            queue_input: Some(false),
            status: UserAgentControlStatus::Succeeded,
            error: None,
        }],
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }]);
    app.thread_event_channels
        .insert(main_thread_id, main_channel);

    let child_channel = ThreadEventChannel::new(/*capacity*/ 1);
    child_channel.store.lock().await.set_turns(vec![Turn {
        id: "turn-child".to_string(),
        items: vec![
            ThreadItem::UserMessage {
                id: "user-child".to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: "Inspect adaptive agent pane rendering.".to_string(),
                    text_elements: Vec::new(),
                }],
            },
            ThreadItem::AgentMessage {
                id: "assistant-child".to_string(),
                text: "Found the boundary behavior and preserved the full response preview."
                    .to_string(),
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        ],
        items_view: TurnItemsView::Full,
        status: TurnStatus::Completed,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }]);
    app.thread_event_channels
        .insert(child_thread_id, child_channel);
    app.apply_primary_agent_aliases(vec![
        AgentAlias {
            thread_id: main_thread_id.to_string(),
            agent_ref: "1".to_string(),
            nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
            state: AgentAliasState::Active,
        },
        AgentAlias {
            thread_id: child_thread_id.to_string(),
            agent_ref: "2".to_string(),
            nickname: Some("Hume".to_string()),
            state: AgentAliasState::Active,
        },
    ]);

    let params = app.agent_picker_selection_view_params(initial_selected_idx);
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    ListSelectionView::new(
        params,
        AppEventSender::new(tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    )
}

fn render_agent_layout(view: &ListSelectionView, width: u16, height: u16) -> Vec<String> {
    let buffer = render_agent_layout_buffer(view, width, height);
    render_agent_layout_buffer_region(&buffer, height, /*start*/ 0, /*end*/ width)
}

fn render_agent_layout_buffer(view: &ListSelectionView, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    buffer
}

fn render_agent_layout_buffer_region(
    buffer: &Buffer,
    height: u16,
    start: u16,
    end: u16,
) -> Vec<String> {
    (0..height)
        .map(|row| {
            (start..end)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<Vec<_>>()
                .concat()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn normalized_agent_layout_line(
    line: &str,
    main_thread_id: ThreadId,
    child_thread_id: ThreadId,
) -> String {
    line.replace(&main_thread_id.to_string(), "[root]")
        .replace(&child_thread_id.to_string(), "[child]")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalized_agent_name_line(line: String) -> Option<String> {
    ["Main [default]", "Hume [reviewer]"]
        .into_iter()
        .find_map(|identity| {
            line.find(identity)
                .map(|identity_end| line[..identity_end + identity.len()].to_string())
        })
}

#[tokio::test]
async fn current_agent_uses_markerless_name_emphasis() {
    for initial_selected_idx in [Some(1), Some(0)] {
        let view = adaptive_agent_layout_view(initial_selected_idx).await;
        let width = 160;
        let height = 30;
        let buffer = render_agent_layout_buffer(&view, width, height);
        let rows = render_agent_layout_buffer_region(
            &buffer, height, /*start*/ 0, /*end*/ width,
        );
        let main_row = rows
            .iter()
            .position(|line| line.contains("Main [default]"))
            .expect("current root row should be visible");
        let main_name_byte_start = rows[main_row]
            .find("Main [default]")
            .expect("current root name should be visible");
        let main_name_start =
            UnicodeWidthStr::width(&rows[main_row][..main_name_byte_start]) as u16;
        assert!(!rows.iter().any(|line| line.contains("(current)")));
        let modifiers = buffer[(main_name_start, main_row as u16)]
            .style()
            .add_modifier;
        assert!(modifiers.contains(Modifier::BOLD));
    }
}

#[test]
fn agent_control_pane_details_snapshot() {
    let details = AgentControlPaneDetails::new(vec![
        "Anscombe [reviewer]".bold().into(),
        vec!["running 4m 12s".green(), " · ref 2".dim()].into(),
        vec![
            "UUID: ".bold(),
            "019ff050-d466-73b0-b133-72ecc7c67269".dim(),
        ]
        .into(),
        vec!["Model: ".bold(), "gpt-5.6-sol medium".into()].into(),
        "".into(),
        vec![
            "Task: ".bold(),
            "Review the response-observation lifecycle.".into(),
        ]
        .into(),
        vec![
            "Latest response: ".bold(),
            "Found one ordering issue in completion delivery.".into(),
        ]
        .into(),
        vec!["Fork: ".bold(), "last 3 turns".into()].into(),
        "".into(),
        vec!["Response: ".bold(), "wake".into(), " · current turn".dim()].into(),
        vec!["Commentary: ".bold(), "first item".into()].into(),
        vec!["Queued: ".bold(), "1".into()].into(),
        vec![
            "  1. ".dim(),
            "Check pagination replay.".into(),
            " · w:x".dim(),
        ]
        .into(),
        vec!["Children: ".bold(), "2".into()].into(),
        vec!["Approval: ".bold(), "pending".magenta()].into(),
        "".into(),
        "Enter opens this thread".dim().into(),
    ]);

    let rendered = details
        .wrapped_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    Anscombe [reviewer]
    running 4m 12s · ref 2
    UUID: 019ff050-d466-73b0-b133-72ecc7c67269
    Model: gpt-5.6-sol medium

    Task: Review the response-observation lifecycle.
    Latest response: Found one ordering issue in completion delivery.
    Fork: last 3 turns

    Response: wake · current turn
    Commentary: first item
    Queued: 1
      1. Check pagination replay. · w:x
    Children: 2
    Approval: pending

    Enter opens this thread
    ");
}

#[test]
fn selecting_agent_updates_shared_preview_revision() {
    let preview = AgentControlPanePreview::new(AgentControlPaneDetails::new(vec!["First".into()]));
    let renderable = preview.renderable();
    assert_eq!(renderable.layout_revision(), Some(0));

    preview.select(AgentControlPaneDetails::new(vec!["Second".into()]));

    assert_eq!(renderable.layout_revision(), Some(1));
    assert_eq!(
        renderable
            .selected
            .lock()
            .expect("preview state should not be poisoned")
            .wrapped_lines(/*width*/ 40),
        vec!["Second".into()]
    );
}

#[test]
fn closed_or_external_rows_inspect_without_resuming_the_thread() {
    let thread_id = codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("valid thread id");

    assert!(matches!(
        AgentControlEnterAction::SwitchThread.event(thread_id),
        AppEvent::SelectAgentThread(selected_thread_id) if selected_thread_id == thread_id
    ));
    assert!(matches!(
        AgentControlEnterAction::InspectTranscript.event(thread_id),
        AppEvent::InspectAgentTranscript(selected_thread_id) if selected_thread_id == thread_id
    ));
    assert_eq!(
        AgentControlEnterAction::InspectTranscript.hint(),
        "Enter inspects this transcript"
    );
}

#[tokio::test]
async fn transcript_inspection_keeps_the_agent_pane_under_the_pager() {
    let mut app = super::super::test_support::make_test_app().await;
    let main_thread_id =
        codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid thread id");
    let closed_thread_id =
        codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid thread id");
    app.primary_thread_id = Some(main_thread_id);
    app.active_thread_id = Some(main_thread_id);
    app.agent_navigation.upsert(
        main_thread_id,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.agent_navigation.upsert(
        closed_thread_id,
        Some("Hume".to_string()),
        Some("reviewer".to_string()),
        /*is_closed*/ true,
    );

    let params = app.agent_picker_selection_view_params(/*selected*/ None);

    assert!(params.items[0].dismiss_on_select);
    assert!(!params.items[1].dismiss_on_select);
    assert!(
        params
            .items
            .iter()
            .all(|item| item.global_shortcut_action.is_some())
    );
    assert!(
        params
            .footer_note
            .as_ref()
            .is_some_and(|line| line.to_string().contains("ctrl + t inspects transcript"))
    );
    let (tx, mut rx) = unbounded_channel();
    params.items[0]
        .global_shortcut_action
        .as_ref()
        .expect("agent rows should expose transcript inspection")(&AppEventSender::new(tx));
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::InspectAgentTranscript(thread_id)) if thread_id == main_thread_id
    ));
    let row_descriptions = params
        .items
        .iter()
        .filter_map(|item| item.description.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(
        row_descriptions,
        @r"
    00000000-0000-0000-0000-000000000001 · idle
    00000000-0000-0000-0000-000000000002 · external
    "
    );
}

#[tokio::test]
async fn transcript_tab_binding_replaces_the_agent_controls_hint() {
    let mut app = super::super::test_support::make_test_app().await;
    let main_thread_id =
        codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid thread id");
    app.primary_thread_id = Some(main_thread_id);
    app.active_thread_id = Some(main_thread_id);
    app.agent_navigation.upsert(
        main_thread_id,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
        /*is_closed*/ false,
    );
    app.keymap.app.open_transcript = vec![crate::key_hint::plain(KeyCode::Tab)];

    let params = app.agent_picker_selection_view_params(/*selected*/ None);

    assert_eq!(
        params.footer_note.map(|line| line.to_string()),
        Some("tab inspects transcript.".to_string())
    );
    assert_eq!(
        params.global_shortcut_bindings,
        vec![crate::key_hint::plain(KeyCode::Tab)]
    );
}

#[tokio::test]
async fn child_primary_view_uses_durable_ref_one_as_agent_tree_main() {
    let mut app = super::super::test_support::make_test_app().await;
    let main_thread_id =
        codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid thread id");
    let child_thread_id =
        codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid thread id");
    app.primary_thread_id = Some(child_thread_id);
    app.active_thread_id = Some(child_thread_id);

    app.apply_primary_agent_aliases(vec![
        codex_app_server_protocol::AgentAlias {
            thread_id: main_thread_id.to_string(),
            agent_ref: "1".to_string(),
            nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
            state: codex_app_server_protocol::AgentAliasState::Active,
        },
        codex_app_server_protocol::AgentAlias {
            thread_id: child_thread_id.to_string(),
            agent_ref: "2".to_string(),
            nickname: Some("Hume".to_string()),
            state: codex_app_server_protocol::AgentAliasState::Active,
        },
    ]);

    assert_eq!(app.agent_root_thread_id(), Some(main_thread_id));
    assert_eq!(app.thread_label(main_thread_id), "Main [default]");
    assert_eq!(app.thread_label(child_thread_id), "Hume");
    assert!(
        app.agent_navigation
            .get(&main_thread_id)
            .is_some_and(|entry| entry.is_closed),
        "an alias alone establishes ownership, not a loaded runtime"
    );
    assert!(
        app.agent_navigation
            .get(&child_thread_id)
            .is_some_and(|entry| !entry.is_closed),
        "the displayed thread is known to be loaded"
    );
    assert_eq!(
        app.agent_navigation
            .ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect::<Vec<_>>(),
        vec![main_thread_id, child_thread_id]
    );
}

#[test]
fn wide_agent_pane_renders_the_complete_side_detail_column() {
    let details = AgentControlPaneDetails::new(vec![
        "Hopper [reviewer]".bold().into(),
        "running · ref 2".into(),
        "".into(),
        "Response: wake · current turn".into(),
        "Queued: 2".into(),
        "Enter opens this thread".dim().into(),
    ]);
    let preview = AgentControlPanePreview::new(details);
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let view = ListSelectionView::new(
        SelectionViewParams {
            title: Some("Agents".to_string()),
            items: vec![SelectionItem {
                name: "2 Hopper [reviewer]".to_string(),
                description: Some("running".to_string()),
                ..Default::default()
            }],
            side_content: Box::new(preview.renderable()),
            side_content_width: SideContentWidth::RemainingAfterList(60),
            side_content_min_width: 48,
            ..Default::default()
        },
        AppEventSender::new(tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    let width = 120;
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    let content_width = crate::bottom_pane::popup_content_width(width);
    let (_list_width, side_width) = crate::bottom_pane::side_by_side_layout_widths(
        content_width,
        SideContentWidth::RemainingAfterList(60),
        /*side_content_min_width*/ 48,
    )
    .expect("wide pane should use a split layout");
    let side_x = 2 + content_width - side_width;
    let rendered = (0..height)
        .filter_map(|row| {
            let line = (side_x..side_x + side_width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<Vec<_>>()
                .concat();
            let line = line.trim_end().to_string();
            (!line.is_empty()).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r"
    Hopper [reviewer]
    running · ref 2
    Response: wake · current turn
    Queued: 2
    Enter opens this thread
    ");
}

#[test]
fn wide_agent_pane_uses_side_panel_height_for_agent_rows() {
    let preview = AgentControlPanePreview::new(AgentControlPaneDetails::new(
        (1..=20)
            .map(|row| format!("Detail row {row}").into())
            .collect(),
    ));
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let view = ListSelectionView::new(
        SelectionViewParams {
            title: Some("Agents".to_string()),
            items: (1..=12)
                .map(|row| SelectionItem {
                    name: format!("Worker {row:02}"),
                    ..Default::default()
                })
                .collect(),
            show_row_numbers: false,
            row_display: SelectionRowDisplay::SingleLine,
            list_height: SelectionListHeight::FillAvailable,
            side_content: Box::new(preview.renderable()),
            side_content_width: SideContentWidth::RemainingAfterList(60),
            side_content_min_width: 48,
            initial_selected_idx: Some(11),
            ..Default::default()
        },
        AppEventSender::new(tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    let width = 120;
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    let rendered = buffer
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();

    assert!(
        rendered.contains("Worker 01") && rendered.contains("Worker 12"),
        "all agent rows should use the vertical space already required by the detail panel"
    );
}

#[tokio::test]
async fn agent_pane_uses_split_layout_at_94_columns_snapshot() {
    let view = adaptive_agent_layout_view(Some(1)).await;
    let width = 94;
    let height = 30;
    let content_width = crate::bottom_pane::popup_content_width(width);
    let (list_width, side_width) = crate::bottom_pane::side_by_side_layout_widths(
        content_width,
        SideContentWidth::RemainingAfterList(40),
        /*side_content_min_width*/ 48,
    )
    .expect("wide pane should use a split layout");
    let side_x = 2 + content_width - side_width;
    let list_end = side_x - 2;
    let buffer = render_agent_layout_buffer(&view, width, height);
    let rows =
        render_agent_layout_buffer_region(&buffer, height, /*start*/ 0, /*end*/ width);
    let main_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
    let child_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    let footer_rows = rows
        .iter()
        .enumerate()
        .filter_map(|(row, line)| {
            (line.contains("inspects transcript") || line.trim_start().starts_with("Press "))
                .then_some(row)
        })
        .collect::<Vec<_>>();
    let left =
        render_agent_layout_buffer_region(&buffer, height, /*start*/ 0, /*end*/ list_end)
            .into_iter()
            .map(|line| normalized_agent_layout_line(&line, main_thread_id, child_thread_id))
            .filter_map(normalized_agent_name_line)
            .collect::<Vec<_>>();
    let right = render_agent_layout_buffer_region(
        &buffer, height, /*start*/ side_x, /*end*/ width,
    )
    .into_iter()
    .enumerate()
    .filter(|(row, _)| !footer_rows.contains(row))
    .map(|(_, line)| line)
    .map(|line| normalized_agent_layout_line(&line, main_thread_id, child_thread_id))
    .filter(|line| !line.is_empty())
    .collect::<Vec<_>>();
    let footer = footer_rows
        .into_iter()
        .map(|row| normalized_agent_layout_line(&rows[row], main_thread_id, child_thread_id))
        .collect::<Vec<_>>();
    let rendered = format!(
        "list width {list_width} | detail width {side_width}\nlist:\n{}\ndetail:\n{}\nfooter:\n{}",
        left.join("\n"),
        right.join("\n"),
        footer.join("\n"),
    );

    insta::assert_snapshot!(rendered, @r"
    list width 40 | detail width 48
    list:
    1 • Main [default]
    › 2 ↳ • Hume [reviewer]
    detail:
    Hume [reviewer]
    completed · ref 2
    UUID: [child]
    Model: gpt-5.6-luna medium
    Parent: [root]
    Path: /root/reviewer
    Task: Inspect adaptive agent pane rendering.
    Latest response: Found the boundary behavior and
    preserved the full response preview.
    Fork: none
    Response: none
    Queued: 0
    Children: 0
    Enter opens this thread
    footer:
    ctrl + t inspects transcript · Tab opens controls.
    Press enter to confirm or esc to go back
    ");
}

#[tokio::test]
async fn agent_pane_stacks_details_at_93_columns_snapshot() {
    let view = adaptive_agent_layout_view(Some(1)).await;
    let width = 93;
    let height = 30;
    assert_eq!(
        crate::bottom_pane::side_by_side_layout_widths(
            crate::bottom_pane::popup_content_width(width),
            SideContentWidth::RemainingAfterList(40),
            /*side_content_min_width*/ 48,
        ),
        None
    );
    let main_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
    let child_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
    let rows = render_agent_layout(&view, width, height)
        .into_iter()
        .map(|line| normalized_agent_layout_line(&line, main_thread_id, child_thread_id))
        .collect::<Vec<_>>();
    let main_row = rows
        .iter()
        .position(|line| line.contains("Main [default]") && line.contains("[root]"))
        .expect("root row should be visible");
    let child_row = rows
        .iter()
        .position(|line| line.contains("Hume [reviewer]") && line.contains("[child]"))
        .expect("child row should be visible");
    let detail_start = rows
        .iter()
        .enumerate()
        .skip(child_row + 1)
        .find_map(|(index, line)| (line == "Hume [reviewer]").then_some(index))
        .expect("selected-agent details should be visible");
    let detail_end = rows
        .iter()
        .enumerate()
        .skip(detail_start)
        .find_map(|(index, line)| line.contains("Enter opens this thread").then_some(index))
        .expect("selected-agent details should include the enter action");
    assert!(
        main_row < child_row && child_row < detail_start,
        "stacked details must follow the complete agent list"
    );
    let footer = rows
        .iter()
        .filter(|line| line.contains("inspects transcript") || line.starts_with("Press "))
        .cloned()
        .collect::<Vec<_>>();
    let rendered = format!(
        "layout: stacked\nlist:\n{}\n{}\ndetail:\n{}\nfooter:\n{}",
        rows[main_row],
        rows[child_row],
        rows[detail_start..=detail_end]
            .iter()
            .filter(|line| !line.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
        footer.join("\n"),
    );

    insta::assert_snapshot!(rendered, @r"
    layout: stacked
    list:
    1 • Main [default] [root] · completed
    › 2 ↳ • Hume [reviewer] [child] · completed
    detail:
    Hume [reviewer]
    completed · ref 2
    UUID: [child]
    Model: gpt-5.6-luna medium
    Parent: [root]
    Path: /root/reviewer
    Task: Inspect adaptive agent pane rendering.
    Latest response: Found the boundary behavior and preserved the full response preview.
    Fork: none
    Response: none
    Queued: 0
    Children: 0
    Enter opens this thread
    footer:
    ctrl + t inspects transcript · Tab opens controls.
    Press enter to confirm or esc to go back
    ");
}
