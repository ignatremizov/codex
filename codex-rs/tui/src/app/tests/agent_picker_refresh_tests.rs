use super::*;
use crate::app::agent_picker::AGENT_PICKER_VIEW_ID;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn approval_refresh_preserves_agent_filter_and_stacked_controls() -> Result<()> {
    for query in ["Beta", "approval"] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        let observer = ThreadId::new();
        let alpha = ThreadId::new();
        let beta = ThreadId::new();
        app.primary_thread_id = Some(observer);
        app.active_thread_id = Some(observer);
        for (thread_id, nickname) in [(alpha, "Alpha"), (beta, "Beta")] {
            app.agent_navigation.upsert(
                thread_id,
                Some(nickname.to_owned()),
                /*agent_role*/ None,
                /*is_closed*/ false,
            );
            app.thread_event_channels
                .insert(thread_id, ThreadEventChannel::new(/*capacity*/ 1));
        }
        let params = app.agent_picker_selection_view_params(/*selected*/ None);
        app.chat_widget.show_selection_view(params);
        for character in query.chars() {
            app.chat_widget
                .handle_key_event(KeyEvent::from(KeyCode::Char(character)));
        }
        let beta_index = app
            .agent_navigation
            .ordered_threads()
            .iter()
            .position(|(thread_id, _)| *thread_id == beta);
        assert_eq!(
            app.chat_widget
                .selected_index_for_present_view(AGENT_PICKER_VIEW_ID),
            (query == "Beta").then_some(beta_index).flatten()
        );

        app.chat_widget.show_selection_view(SelectionViewParams {
            view_id: Some("test-agent-controls"),
            title: Some("Agent controls".to_owned()),
            items: vec![SelectionItem {
                name: "Keep these controls open".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let controls = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        app.thread_event_channels[&beta]
            .store
            .lock()
            .await
            .push_request(exec_approval_request(
                beta, "turn", "approval", /*approval_id*/ None,
            ));
        app.refresh_pending_thread_approvals().await;
        assert_eq!(
            render_bottom_popup(&app.chat_widget, /*width*/ 100),
            controls
        );
        assert!(
            app.chat_widget
                .dismiss_selection_view("test-agent-controls")
        );
        assert_eq!(
            app.chat_widget
                .selected_index_for_present_view(AGENT_PICKER_VIEW_ID),
            beta_index
        );
        let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        assert!(rendered.contains(query), "{rendered}");
        assert!(rendered.contains("Beta (approval)"), "{rendered}");
        assert!(rendered.contains("Approval: pending"), "{rendered}");
        assert!(!rendered.contains("Alpha"), "{rendered}");

        while events.try_recv().is_ok() {}
        app.refresh_pending_thread_approvals().await;
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::LoadAgentMailboxInventory { .. }))
        );

        app.thread_event_channels[&beta]
            .store
            .lock()
            .await
            .push_notification(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    thread_id: beta.to_string(),
                    request_id: AppServerRequestId::Integer(1),
                },
            ));
        app.refresh_pending_thread_approvals().await;
        assert_eq!(
            app.chat_widget
                .selected_index_for_present_view(AGENT_PICKER_VIEW_ID),
            (query == "Beta").then_some(beta_index).flatten()
        );
        let rendered = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        assert!(rendered.contains(query), "{rendered}");
        assert!(!rendered.contains("approval pending"), "{rendered}");
        assert!(!rendered.contains("Approval: pending"), "{rendered}");
        assert!(!rendered.contains("Alpha"), "{rendered}");

        assert!(app.chat_widget.dismiss_selection_view(AGENT_PICKER_VIEW_ID));
        while events.try_recv().is_ok() {}
        app.thread_event_channels[&beta]
            .store
            .lock()
            .await
            .push_request(exec_approval_request(
                beta,
                "next-turn",
                "next-approval",
                /*approval_id*/ None,
            ));
        app.refresh_pending_thread_approvals().await;
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, AppEvent::LoadAgentMailboxInventory { .. }))
        );
    }
    Ok(())
}
