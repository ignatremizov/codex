use ratatui::style::Stylize as _;

use super::*;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn agent_control_pane_details_snapshot() {
    let details = AgentControlPaneDetails::new(vec![
        "Anscombe [reviewer]".bold().into(),
        vec!["running".green(), " · ref 2".dim()].into(),
        "".into(),
        "UUID".bold().into(),
        "019ff050-d466-73b0-b133-72ecc7c67269".dim().into(),
        "".into(),
        vec!["Model: ".bold(), "gpt-5.6-sol".into(), " · medium".dim()].into(),
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
        vec!["Running: ".bold(), "4m 12s".into()].into(),
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
        vec!["Approval: ".bold(), "pending".yellow()].into(),
        "".into(),
        "Enter opens this thread".dim().into(),
    ]);

    let rendered = details
        .wrapped_lines(/*width*/ 40)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    Anscombe [reviewer]
    running · ref 2

    UUID
    019ff050-d466-73b0-b133-72ecc7c67269

    Model: gpt-5.6-sol · medium
    Task: Review the response-observation
    lifecycle.
    Latest response: Found one ordering issue
    in completion delivery.
    Fork: last 3 turns
    Running: 4m 12s

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
    00000000-0000-0000-0000-000000000002 · closed
    "
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
            side_content_width: SideContentWidth::Half,
            side_content_min_width: 32,
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
        SideContentWidth::Half,
        /*side_content_min_width*/ 32,
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
