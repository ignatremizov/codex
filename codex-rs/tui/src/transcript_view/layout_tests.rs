//! Cache reuse, bounded lifetime, and invalidation across frames.

use super::*;
use crate::diff_model::FileChange;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::new_patch_event;
use pretty_assertions::assert_eq;
use ratatui::text::Line;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Debug, Default)]
struct Cell {
    renders: AtomicUsize,
    tick: AtomicUsize,
    mutable: bool,
}

impl HistoryCell for Cell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.renders.fetch_add(/*val*/ 1, Ordering::Relaxed);
        vec![format!("tick {}", self.tick.load(Ordering::Relaxed)).into()]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![]
    }

    fn has_stable_transcript_height(&self) -> bool {
        !self.mutable
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        Some(self.tick.load(Ordering::Relaxed) as u64)
    }
}

#[test]
fn layouts_refresh_for_width_animation_and_mutable_frames() {
    for mutable in [false, true] {
        let cell = Arc::new(Cell {
            mutable,
            ..Cell::default()
        });
        let history = cell.clone() as Arc<dyn HistoryCell>;
        let mut cache = LayoutCache::default();
        let get = |cache: &mut LayoutCache, width| {
            cache.get(
                &history,
                width,
                CellPresentation {
                    command_directory: CommandDirectoryDisplay::Changes,
                    separated: false,
                    shares_prompt_background: false,
                    turn_tip_space: false,
                    expanded: false,
                    disclosure: false,
                    detailed: false,
                    review: false,
                    image_protocol: None,
                },
                || TextLayout::new(history.transcript_hyperlink_lines(width), width),
            )
        };
        cache.begin_frame();
        get(&mut cache, /*width*/ 20);
        get(&mut cache, /*width*/ 20);
        assert_eq!(cell.renders.load(Ordering::Relaxed), 1);
        cache.begin_frame();
        get(&mut cache, /*width*/ 20);
        assert_eq!(
            cell.renders.load(Ordering::Relaxed),
            1 + usize::from(mutable)
        );
        cell.tick.store(/*val*/ 2, Ordering::Relaxed);
        cache.begin_frame();
        let next = get(&mut cache, /*width*/ 20);
        assert_eq!(next.text(), "tick 2");
        get(&mut cache, /*width*/ 10);
        assert_eq!(
            cell.renders.load(Ordering::Relaxed),
            3 + usize::from(mutable)
        );
        crate::terminal_palette::with_test_default_colors(
            crate::terminal_probe::DefaultColors {
                fg: (12, 34, 56),
                bg: (65, 43, 21),
            },
            || {
                cache.begin_frame();
                get(&mut cache, /*width*/ 10);
                assert_eq!(
                    cell.renders.load(Ordering::Relaxed),
                    4 + usize::from(mutable)
                );
            },
        );
    }
}

#[test]
fn recent_entries_are_reused_and_old_entries_are_evicted() {
    let cells: Vec<_> = (0..100).map(|_| Arc::new(Cell::default())).collect();
    let mut cache = LayoutCache::default();
    cache.begin_frame();
    for cell in &cells {
        let history = cell.clone() as Arc<dyn HistoryCell>;
        cache.get(
            &history,
            /*width*/ 20,
            CellPresentation {
                command_directory: CommandDirectoryDisplay::Changes,
                separated: false,
                shares_prompt_background: false,
                turn_tip_space: false,
                expanded: false,
                disclosure: false,
                detailed: false,
                review: false,
                image_protocol: None,
            },
            || {
                TextLayout::new(
                    history.transcript_hyperlink_lines(/*width*/ 20),
                    /*width*/ 20,
                )
            },
        );
    }
    for index in [99, 98, 0] {
        let history = cells[index].clone() as Arc<dyn HistoryCell>;
        cache.get(
            &history,
            /*width*/ 20,
            CellPresentation {
                command_directory: CommandDirectoryDisplay::Changes,
                separated: false,
                shares_prompt_background: false,
                turn_tip_space: false,
                expanded: false,
                disclosure: false,
                detailed: false,
                review: false,
                image_protocol: None,
            },
            || {
                TextLayout::new(
                    history.transcript_hyperlink_lines(/*width*/ 20),
                    /*width*/ 20,
                )
            },
        );
    }
    assert_eq!(
        [99, 98, 0].map(|index| cells[index].renders.load(Ordering::Relaxed)),
        [1, 1, 2]
    );
}

fn cached_full_layout(
    cache: &mut LayoutCache,
    cell: &Arc<dyn HistoryCell>,
    renders: &AtomicUsize,
) -> Arc<TextLayout> {
    // Exercise the byte budget without generating a huge number of wrapped rows.
    let width = u16::MAX;
    cache.get(
        cell,
        width,
        CellPresentation {
            command_directory: CommandDirectoryDisplay::Changes,
            separated: false,
            shares_prompt_background: false,
            turn_tip_space: false,
            expanded: false,
            disclosure: false,
            detailed: true,
            review: false,
            image_protocol: None,
        },
        || {
            renders.fetch_add(/*val*/ 1, Ordering::Relaxed);
            TextLayout::new(cell.transcript_hyperlink_lines(width), width)
        },
    )
}

#[test]
fn oversized_deleted_patch_survives_neighbor_lookups_and_expires_through_lru() {
    let mut content = format!("{}\n", "x".repeat(/*n*/ 1023)).repeat(/*n*/ 8192);
    content.push_str("last deleted line\n");
    let patch: Arc<dyn HistoryCell> = Arc::new(new_patch_event(
        HashMap::from([(PathBuf::from("huge.txt"), FileChange::Delete { content })]),
        Path::new("/project"),
    ));
    let following: Arc<dyn HistoryCell> =
        Arc::new(PlainHistoryCell::new(vec!["Following message".into()]));
    let patch_renders = AtomicUsize::new(0);
    let following_renders = AtomicUsize::new(0);
    let mut cache = LayoutCache::default();
    cache.begin_frame();
    let patch_layout = cached_full_layout(&mut cache, &patch, &patch_renders);
    assert!(patch_layout.text().len() > MAX_CACHED_LAYOUT_BYTES);
    assert_eq!(
        (
            patch_layout.row_count(),
            patch_layout.text().lines().next(),
            patch_layout.text().lines().last(),
        ),
        (
            8194,
            Some("• Deleted huge.txt (+0 -8193)"),
            Some("-last deleted line"),
        ),
    );
    let following_layout = cached_full_layout(&mut cache, &following, &following_renders);
    for _ in 0..4 {
        cache.begin_frame();
        assert!(Arc::ptr_eq(
            &patch_layout,
            &cached_full_layout(&mut cache, &patch, &patch_renders),
        ));
        assert!(Arc::ptr_eq(
            &following_layout,
            &cached_full_layout(&mut cache, &following, &following_renders),
        ));
    }
    assert_eq!(
        (
            patch_renders.load(Ordering::Relaxed),
            following_renders.load(Ordering::Relaxed),
        ),
        (1, 1),
    );

    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        /*width*/ u16::MAX,
        /*height*/ 2,
    );
    let mut buffer = Buffer::empty(area);
    patch_layout.render(
        Rect { height: 1, ..area },
        &mut buffer,
        patch_layout.row_count() - 1,
    );
    following_layout.render(
        Rect {
            y: 1,
            height: 1,
            ..area
        },
        &mut buffer,
        /*start_row*/ 0,
    );
    insta::assert_snapshot!(
        crate::transcript_view::tests::text(&buffer),
        @"
            8193 -last deleted line
        Following message
        "
    );

    let expired = Arc::downgrade(&patch_layout);
    drop(patch_layout);
    for _ in 0..MAX_CACHED_ENTRIES {
        let cell = Arc::new(Cell::default()) as Arc<dyn HistoryCell>;
        cached_full_layout(&mut cache, &cell, &following_renders);
    }
    assert!(expired.upgrade().is_none());
}

#[test]
fn ordinary_layouts_still_share_the_original_byte_budget() {
    let cells: Vec<Arc<dyn HistoryCell>> = (0..2)
        .map(|_| -> Arc<dyn HistoryCell> {
            Arc::new(PlainHistoryCell::new(vec![
                "x".repeat(MAX_CACHED_LAYOUT_BYTES / 2 + 1).into(),
            ]))
        })
        .collect();
    let renders = AtomicUsize::new(0);
    let mut cache = LayoutCache::default();
    cache.begin_frame();
    let first = cached_full_layout(&mut cache, &cells[0], &renders);
    let expired = Arc::downgrade(&first);
    drop(first);
    cached_full_layout(&mut cache, &cells[1], &renders);
    assert!(expired.upgrade().is_none());
}

#[test]
fn upward_scrolling_does_not_materialize_the_offscreen_tail() {
    let tracked: Vec<_> = (0..200).map(|_| Arc::new(Cell::default())).collect();
    let cells: Vec<_> = tracked
        .iter()
        .map(|cell| Arc::clone(cell) as Arc<dyn HistoryCell>)
        .collect();
    let mut view = TranscriptView::default();
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 40, /*height*/ 10,
    );
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer, &cells);
    view.jump_to_entry(&cells, /*index*/ 64);
    view.render(area, &mut buffer, &cells);
    // A cold cache must not turn a wheel step in older history into a tail render.
    view.cache.clear();
    for cell in &tracked {
        cell.renders.store(/*val*/ 0, Ordering::Relaxed);
    }

    // The normalized anchor starts on content, after its leading separator.
    view.scroll(&cells, /*rows*/ -2);

    assert_eq!(
        tracked
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                let renders = cell.renders.load(Ordering::Relaxed);
                (renders > 0).then_some((index, renders))
            })
            .collect::<Vec<_>>(),
        vec![(63, 1), (64, 1)],
    );
}

#[test]
fn fullscreen_image_previews_follow_layout_without_entering_text_surfaces() {
    use crate::history_cell::new_image_generation_call;
    use crate::history_cell::new_view_image_tool_call;
    use crate::pets::ImageProtocol;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::LegacyAppPathString;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artifact.png");
    image::RgbaImage::from_pixel(480, 160, image::Rgba([255, 0, 0, 255]))
        .save(&path)
        .unwrap();
    let cells: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(new_view_image_tool_call(
            "view-call".to_string(),
            LegacyAppPathString::from_string(path.to_string_lossy().into_owned()),
        )),
        Arc::new(new_image_generation_call(
            "image-call".to_string(),
            "completed",
            Some("A red rectangle".to_string()),
            Some(AbsolutePathBuf::try_from(path.clone()).unwrap()),
        )),
    ];
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 140, /*height*/ 40,
    );
    let frame = |buf: &Buffer| {
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        crate::transcript_view::tests::text(buf)
            .replace(&uri, "file:///workspace/artifact.png")
            .replace(path.to_str().unwrap(), "/workspace/artifact.png")
            .trim_end()
            .to_string()
    };
    let mut view = TranscriptView::default();
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf, &cells);
    let text_only = buf.clone();
    view.image_protocol = Some(ImageProtocol::Kitty);
    view.render(area, &mut buf, &cells);
    let collapsed = frame(&buf);
    let collapsed_areas: Vec<_> = view.image_placements().iter().map(|p| p.area).collect();
    // Find may request full text, but only manual expansion may reveal inspected pixels.
    assert!(
        view.entry_layout(&cells, /*index*/ 0, /*full_content*/ true)
            .unwrap()
            .image
            .is_none()
    );

    // An inspected image only appears after its existing activity control is expanded.
    assert!(view.toggle_disclosure_at(&cells, /*column*/ 4, /*row*/ 1));
    view.image_protocol = None;
    view.render(area, &mut buf, &cells);
    let text: Vec<_> = (0..cells.len())
        .map(|index| view.layout(&cells, index).unwrap().text().to_string())
        .collect();
    view.image_protocol = Some(ImageProtocol::Kitty);
    view.render(area, &mut buf, &cells);
    let expanded_areas: Vec<_> = view.image_placements().iter().map(|p| p.area).collect();
    insta::assert_snapshot!(
        "fullscreen_image_artifact_rows",
        format!(
            "Collapsed inspection / generated output\n{collapsed}\nGraphics: {collapsed_areas:?}\n\nExpanded inspection / generated output\n{}\nGraphics: {expanded_areas:?}",
            frame(&buf)
        )
    );
    for (index, expected) in text.iter().enumerate() {
        let layout = view.layout(&cells, index).unwrap();
        assert_eq!(layout.text(), expected);
        let pinned_narrow = layout.rewrap(/*width*/ 8);
        assert!(pinned_narrow.visible_image().is_none());
        let pinned_restored = pinned_narrow.rewrap(area.width);
        assert_eq!(pinned_restored.row_count(), layout.row_count());
        assert!(pinned_restored.visible_image().is_some());
    }
    // Short, inset viewports may begin in an image. Its placement stays inside the viewport.
    let short = Rect::new(
        /*x*/ 7, /*y*/ 5, /*width*/ 16, /*height*/ 3,
    );
    view.jump_to_latest();
    view.render(short, &mut Buffer::empty(short), &cells);
    let placements = view.image_placements();
    assert!(!placements.is_empty());
    assert!(
        placements
            .iter()
            .all(|placement| short.contains(placement.area.as_position())
                && placement.area.right() <= short.right()
                && placement.area.bottom() <= short.bottom())
    );

    view.render(area, &mut buf, &cells);
    let control_row = view
        .visible
        .iter()
        .position(|visible| visible.layout.disclosure_row() == Some(visible.row))
        .unwrap() as u16;
    assert!(view.toggle_disclosure_at(&cells, /*column*/ 4, control_row));
    view.render(area, &mut buf, &cells);

    // Raw mode and unsupported terminals reserve no rows, even after rich previews were cached.
    view.jump_to_latest();
    view.set_presentation(/*detailed*/ false, HistoryRenderMode::Raw);
    view.render(area, &mut buf, &cells);
    assert!((0..cells.len()).all(|index| view.layout(&cells, index).unwrap().image.is_none()));
    view.set_presentation(/*detailed*/ false, HistoryRenderMode::Rich);
    view.image_protocol = None;
    view.render(area, &mut buf, &cells);
    assert_eq!(buf, text_only);

    // Resumed sessions with a deleted file fall back to the original text entry.
    std::fs::remove_file(&path).unwrap();
    view.image_protocol = Some(ImageProtocol::Kitty);
    view.render(area, &mut buf, &cells);
    assert_eq!(buf, text_only);
    assert!(view.image_placements().is_empty());

    // A tiny compressed file with a large decoded raster must also fall back to text.
    image::RgbImage::new(4000, 3000).save(&path).unwrap();
    view.image_protocol = None;
    view.render(area, &mut buf, &cells);
    view.image_protocol = Some(ImageProtocol::Kitty);
    view.render(area, &mut buf, &cells);
    assert_eq!(buf, text_only);
    assert!(view.image_placements().is_empty());
}
