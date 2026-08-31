use super::BottomPaneView;
use super::ColumnWidthMode;
use super::ListSelectionView;
use super::PickerSurface;
use super::SelectionItem;
use super::SelectionListHeight;
use super::SelectionRowDisplay;
use super::SelectionViewParams;
use crate::app_event_sender::AppEventSender;
use crate::keymap::RuntimeKeymap;
use crate::render::renderable::Renderable;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn configured_budget_drives_rendering_and_page_navigation() {
    for row_display in [
        SelectionRowDisplay::Wrapped,
        SelectionRowDisplay::SingleLine,
    ] {
        for budget in [0usize, 3, 8, 24] {
            let (tx, _rx) = unbounded_channel();
            let mut view = ListSelectionView::new(
                SelectionViewParams {
                    max_visible_rows: budget,
                    row_display,
                    items: (0..40)
                        .map(|index| SelectionItem {
                            name: format!("Row {index:02}"),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
                AppEventSender::new(tx),
                RuntimeKeymap::defaults().list,
            );
            let area = Rect::new(0, 0, 80, view.desired_height(/*width*/ 80));
            view.render(area, &mut Buffer::empty(area));
            let expected = budget.max(1);
            assert_eq!(view.rendered_item_count(), expected);
            assert_eq!(view.max_visible_rows(/*len*/ 40), expected);
            view.handle_key_event(KeyEvent::from(KeyCode::PageDown));
            assert_eq!(view.selected_actual_idx(), Some(expected));
            view.handle_key_event(KeyEvent::from(KeyCode::PageUp));
            assert_eq!(view.selected_actual_idx(), Some(0));
        }
    }
}

#[test]
fn capped_page_navigation_tracks_painted_items_after_resizing() {
    for row_display in [
        SelectionRowDisplay::Wrapped,
        SelectionRowDisplay::SingleLine,
    ] {
        for budget in [3, 24] {
            let (tx, _rx) = unbounded_channel();
            let mut view = ListSelectionView::new(
                SelectionViewParams {
                    max_visible_rows: budget,
                    row_display,
                    items: (0..40)
                        .map(|index| SelectionItem {
                            name: format!("Row {index:02}"),
                            description: Some(
                                "Details extend across several wrapped lines at this width."
                                    .to_string(),
                            ),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
                AppEventSender::new(tx),
                RuntimeKeymap::defaults().list,
            );
            assert_eq!(view.max_visible_rows(/*len*/ 40), budget);
            for height in [9, 18, 9] {
                let area = Rect::new(0, 0, 40, height);
                view.render(area, &mut Buffer::empty(area));
                let painted = view.rendered_item_count();
                assert!(painted > 0 && painted <= budget);
                assert_eq!(view.max_visible_rows(/*len*/ 40), painted);
                view.handle_key_event(KeyEvent::from(KeyCode::PageDown));
                assert_eq!(view.selected_actual_idx(), Some(painted));
                view.handle_key_event(KeyEvent::from(KeyCode::PageUp));
                assert_eq!(view.selected_actual_idx(), Some(0));
            }
        }
    }
}

#[test]
fn filtering_preserves_reserved_budget_without_limiting_fill_available_lists() {
    for list_height in [
        SelectionListHeight::Capped,
        SelectionListHeight::FillAvailable,
    ] {
        let (tx, _rx) = unbounded_channel();
        let mut view = ListSelectionView::new(
            SelectionViewParams {
                max_visible_rows: 24,
                reserve_result_rows: true,
                is_searchable: true,
                list_height,
                row_display: SelectionRowDisplay::SingleLine,
                items: (0..40)
                    .map(|index| {
                        let name = format!("Row {index:02}");
                        SelectionItem {
                            search_value: Some(name.clone()),
                            name,
                            ..Default::default()
                        }
                    })
                    .collect(),
                ..Default::default()
            },
            AppEventSender::new(tx),
            RuntimeKeymap::defaults().list,
        );
        let full_height = view.desired_height(/*width*/ 80);
        let area = Rect::new(0, 0, 80, full_height);
        view.render(area, &mut Buffer::empty(area));
        let expected = match list_height {
            SelectionListHeight::Capped => 24,
            SelectionListHeight::FillAvailable => 40,
        };
        assert_eq!(view.rendered_item_count(), expected);
        view.set_search_query("Row 01".to_string());
        let filtered_height = view.desired_height(/*width*/ 80);
        let area = Rect::new(0, 0, 80, filtered_height);
        view.render(area, &mut Buffer::empty(area));
        assert_eq!(view.rendered_item_count(), 1);
        assert_eq!(view.selected_actual_idx(), Some(1));
        assert_eq!(full_height - filtered_height, expected as u16 - 24);
    }
}

#[test]
fn desired_height_keeps_wrapped_description_tails_visible_at_width_boundaries() {
    for list_height in [
        SelectionListHeight::Capped,
        SelectionListHeight::FillAvailable,
    ] {
        for width in 40..100 {
            let (tx, _rx) = unbounded_channel();
            let view = ListSelectionView::new(
                SelectionViewParams {
                    picker_surface: PickerSurface::Panel,
                    title: Some("Choose an action".to_string()),
                    footer_hint: Some("enter select".into()),
                    col_width_mode: ColumnWidthMode::AutoAllRows,
                    list_height,
                    items: ["TAIL_A", "TAIL_B"]
                        .into_iter()
                        .enumerate()
                        .map(|(index, tail)| SelectionItem {
                            name: format!("Choice {index}"),
                            description: Some(format!(
                                "Read and edit workspace files, or resume an owner thread and copy a working directory. {tail}"
                            )),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
                AppEventSender::new(tx),
                RuntimeKeymap::defaults().list,
            );
            let area = Rect::new(0, 0, width, view.desired_height(width));
            let mut buffer = Buffer::empty(area);
            view.render(area, &mut buffer);
            let rendered = buffer
                .content()
                .chunks(usize::from(width))
                .map(|row| {
                    row.iter()
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(view.rendered_item_count(), 2);
            for tail in ["TAIL_A", "TAIL_B"] {
                assert!(
                    rendered.contains(tail),
                    "clipped {tail} at width {width}: {rendered}"
                );
            }
        }
    }
}
