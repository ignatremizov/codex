use super::super::*;
use crate::app_event::AppEvent;
use crate::keymap::RuntimeKeymap;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

fn agent_params(list_height: SelectionListHeight) -> SelectionViewParams {
    SelectionViewParams {
        is_searchable: true,
        max_visible_rows: 4,
        list_height,
        items: (0..30)
            .map(|index| SelectionItem {
                name: format!("agent {index:02}"),
                search_value: Some(format!("agent {index:02}")),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::UpdateModel(format!("accept-{index}")));
                })],
                ..Default::default()
            })
            .collect(),
        on_selection_changed: Some(Box::new(|index, tx| {
            tx.send(AppEvent::UpdateModel(format!("preview-{index}")));
        })),
        ..Default::default()
    }
}

#[test]
fn refresh_retains_filtered_selection_viewport_and_child_cleanup() {
    for list_height in [
        SelectionListHeight::Capped,
        SelectionListHeight::FillAvailable,
    ] {
        let (tx, mut rx) = unbounded_channel();
        let mut view = ListSelectionView::new(
            agent_params(list_height),
            AppEventSender::new(tx),
            RuntimeKeymap::defaults().list,
        );
        view.set_search_query("agent 1".to_owned());
        // Both list modes must clip the ten matches so End actually scrolls.
        // FillAvailable intentionally ignores max_visible_rows.
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 8,
        );
        view.render(area, &mut Buffer::empty(area));
        assert!(view.last_rendered_visible_rows.get() < view.filtered_indices.len());
        view.handle_key_event(KeyEvent::from(KeyCode::End));
        view.render(area, &mut Buffer::empty(area));
        view.sync_last_rendered_scroll_top();
        assert!(view.state.scroll_top > 0);
        let state = view.state;
        view.dismiss_after_child_accept = true;
        while rx.try_recv().is_ok() {}

        let mut params = agent_params(list_height);
        params.items[19].description = Some("approval pending".to_owned());
        let mut keymap = RuntimeKeymap::defaults().list;
        keymap.accept = vec![crate::key_hint::plain(KeyCode::F(2))];
        assert!(view.refresh_selection_view(params, keymap));

        assert_eq!(
            (
                view.search_query.as_str(),
                view.selected_actual_idx(),
                view.state,
                view.dismiss_after_child_accept,
            ),
            ("agent 1", Some(19), state, true)
        );
        assert_eq!(
            view.items[19].description.as_deref(),
            Some("approval pending")
        );
        assert!(
            rx.try_recv().is_err(),
            "unchanged selection must not reload its preview"
        );
        view.handle_key_event(KeyEvent::from(KeyCode::F(2)));
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::UpdateModel(model)) if model == "accept-19"
        ));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn refresh_refilters_changed_rows_and_only_notifies_the_final_selection() {
    for (query, selected_before, selected_after) in
        [("agent 1", Some(19), 10), ("approval", None, 19)]
    {
        let (tx, mut rx) = unbounded_channel();
        let mut view = ListSelectionView::new(
            agent_params(SelectionListHeight::Capped),
            AppEventSender::new(tx),
            RuntimeKeymap::defaults().list,
        );
        view.set_search_query(query.to_owned());
        view.handle_key_event(KeyEvent::from(KeyCode::End));
        assert_eq!(view.selected_actual_idx(), selected_before);
        while rx.try_recv().is_ok() {}

        let mut params = agent_params(SelectionListHeight::Capped);
        params.items[19].search_value = Some("approval pending".to_owned());
        assert!(view.refresh_selection_view(params, RuntimeKeymap::defaults().list));
        assert_eq!(
            (view.search_query.as_str(), view.selected_actual_idx()),
            (query, Some(selected_after))
        );
        let expected = format!("preview-{selected_after}");
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::UpdateModel(model)) if model == expected
        ));
        assert!(
            rx.try_recv().is_err(),
            "no unfiltered intermediate selection"
        );
    }
}
