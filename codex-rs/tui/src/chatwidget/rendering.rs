//! Render composition for the main chat widget surface.

use super::transcript::ActiveCellLayoutCache;
use super::transcript::ActiveCellLayoutCacheKey;
use super::*;
use crate::terminal_hyperlinks::HyperlinkParagraph;
use std::cell::Cell;

impl ChatWidget {
    pub(crate) fn as_renderable(&self) -> RenderableItem<'_> {
        if self
            .bottom_pane
            .selected_index_for_active_view(crate::app::AGENTS_OVERVIEW_VIEW_ID)
            .is_some()
        {
            return self
                .bottom_pane
                .as_renderable_with_composer_right_reserve(/*composer_right_reserve*/ 0);
        }

        let active_cell_right_reserve = self.ambient_pet_wrap_reserved_cols();
        let active_cell_renderable = match &self.transcript.active_cell {
            Some(cell) => RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                child: cell.as_ref(),
                // The initial header becomes the first history cell, which has no leading separator.
                top: if cell.as_any().is::<history_cell::SessionHeaderHistoryCell>() {
                    0
                } else {
                    1
                },
                right: active_cell_right_reserve,
                // Externally backed transcript cells can also change viewport height without an
                // active-cell revision. Spinner cells remain safe because their indicator width
                // is stable and their display lines are still rebuilt on every frame.
                persistent_layout: cell.has_stable_transcript_height().then_some(
                    PersistentActiveCellLayout {
                        cache: &self.transcript.active_cell_layout,
                        cell_identity: cell.as_ref() as *const dyn HistoryCell as *const ()
                            as usize,
                        revision: self.transcript.active_cell_revision,
                        render_mode: self.history_render_mode(),
                    },
                ),
            })),
            None => RenderableItem::Owned(Box::new(())),
        };
        let mut flex = FlexRenderable::new();
        flex.push(/*flex*/ 1, active_cell_renderable);
        if let Some(cell) = self.pending_token_activity_output() {
            flex.push(
                /*flex*/ 1,
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                    persistent_layout: None,
                })),
            );
        }
        if let Some(cell) = self.pending_rate_limit_reset_hint() {
            flex.push(
                /*flex*/ 1,
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                    persistent_layout: None,
                })),
            );
        }
        flex.push(
            /*flex*/ 0,
            self.bottom_pane
                .as_renderable_with_composer_right_reserve(active_cell_right_reserve)
                .inset(Insets::tlbr(
                    /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
                )),
        );
        RenderableItem::Owned(Box::new(flex))
    }

    pub(crate) fn note_rendered_width(&self, width: u16) {
        self.last_rendered_width.set(Some(width));
    }
}

struct TranscriptAreaRenderable<'a> {
    child: &'a dyn HistoryCell,
    top: u16,
    right: u16,
    persistent_layout: Option<PersistentActiveCellLayout<'a>>,
}

struct PersistentActiveCellLayout<'a> {
    cache: &'a Cell<Option<ActiveCellLayoutCache>>,
    cell_identity: usize,
    revision: u64,
    render_mode: HistoryRenderMode,
}

impl Renderable for TranscriptAreaRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let area = self.child_area(area);
        Clear.render(area, buf);
        let is_exec = self.child.as_any().is::<ExecCell>();
        if is_exec {
            let lines = self.child.display_lines(area.width);
            let lines = if area.height > 1 {
            let wrapped = crate::wrapping::word_wrap_lines(
                lines,
                crate::wrapping::RtOptions::new(usize::from(area.width.max(1))),
            );
            if wrapped.len() <= usize::from(area.height) {
                wrapped
            } else {
                let retained = usize::from(area.height.saturating_sub(1));
                let head_count = retained.div_ceil(2);
                let tail_count = retained.saturating_sub(head_count);
                let omitted_rows = wrapped.len().saturating_sub(retained);
                let mut clipped = Vec::with_capacity(usize::from(area.height));
                clipped.extend(wrapped.iter().take(head_count).cloned());
                clipped.push(
                    crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                        Line::from(
                            format!(
                                "… +{omitted_rows} rows ({})",
                                crate::ui_consts::TRANSCRIPT_HINT
                            )
                            .dim()
                            .italic(),
                        ),
                        usize::from(area.width),
                    ),
                );
                if tail_count > 0 {
                    clipped.extend(
                        wrapped[wrapped.len().saturating_sub(tail_count)..]
                            .iter()
                            .cloned(),
                    );
                }
                clipped
            }
            } else {
                lines
            };
            Paragraph::new(Text::from(lines))
                .wrap(Wrap { trim: false })
                .render(area, buf);
            return;
        }

        let lines = self.child.display_hyperlink_lines(area.width);
        let paragraph = HyperlinkParagraph::new(&lines, Style::default());
        let y = if area.height == 0 {
            0
        } else {
            let rendered_height = if let Some((cache, mut layout)) = self.layout(area.width) {
                if let Some(height) = layout.rendered_height {
                    height
                } else {
                    let height = paragraph.line_count(area.width);
                    layout.rendered_height = Some(height);
                    cache.set(Some(layout));
                    height
                }
            } else {
                paragraph.line_count(area.width)
            };
            let overflow = rendered_height.saturating_sub(usize::from(area.height));
            u16::try_from(overflow).unwrap_or(u16::MAX)
        };
        paragraph.scroll(y).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let child_width = width.saturating_sub(self.right).max(1);
        let desired_height = if let Some((cache, mut layout)) = self.layout(child_width) {
            if let Some(height) = layout.desired_height {
                height
            } else {
                let height = HistoryCell::desired_height(self.child, child_width);
                layout.desired_height = Some(height);
                cache.set(Some(layout));
                height
            }
        } else {
            HistoryCell::desired_height(self.child, child_width)
        };
        desired_height + self.top
    }
}

impl TranscriptAreaRenderable<'_> {
    fn layout(
        &self,
        width: u16,
    ) -> Option<(&Cell<Option<ActiveCellLayoutCache>>, ActiveCellLayoutCache)> {
        let persistent = self.persistent_layout.as_ref()?;
        let key = ActiveCellLayoutCacheKey {
            cell_identity: persistent.cell_identity,
            revision: persistent.revision,
            width,
            render_mode: persistent.render_mode,
            syntax_theme_revision: crate::render::highlight::syntax_theme_revision(),
        };
        let layout = persistent
            .cache
            .get()
            .filter(|layout| layout.key == key)
            .unwrap_or(ActiveCellLayoutCache {
                key,
                desired_height: None,
                rendered_height: None,
            });
        Some((persistent.cache, layout))
    }

    fn child_area(&self, area: Rect) -> Rect {
        let y = area.y.saturating_add(self.top);
        let height = area.height.saturating_sub(self.top);
        Rect::new(
            area.x,
            y,
            area.width.saturating_sub(self.right).max(1),
            height,
        )
    }
}

#[cfg(test)]
#[path = "rendering_tests.rs"]
mod tests;

impl Renderable for ChatWidget {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_renderable().render(area, buf);
        self.note_rendered_width(area.width);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable().desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.as_renderable().cursor_pos(area)
    }

    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        self.as_renderable().cursor_style(area)
    }
}

#[cfg(test)]
mod persistent_layout_tests {
    use super::*;
    use crate::exec_cell::ExecCall;
    use crate::render::renderable::Renderable;

    fn buffer_rows(buf: &Buffer, area: Rect) -> Vec<String> {
        let mut rendered_rows: Vec<String> = Vec::new();
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf.cell((x, y)).expect("cell should exist").symbol());
            }
            rendered_rows.push(row);
        }
        rendered_rows
    }

    #[test]
    fn active_transcript_area_preserves_cell_start_instead_of_bottom_scrolling() {
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        let cell = PlainHistoryCell::new(vec![
            Line::from("active line 1"),
            Line::from("active line 2"),
            Line::from("active line 3"),
            Line::from("active line 4"),
            Line::from("active line 5"),
        ]);
        let renderable = TranscriptAreaRenderable {
            child: &cell,
            top: 1,
            right: 0,
            persistent_layout: None,
        };

        renderable.render(area, &mut buf);

        let rendered_rows = buffer_rows(&buf, area);
        assert!(
            rendered_rows[1].contains("active line 1")
                && rendered_rows[2].contains("active line 2")
                && rendered_rows[3].contains("active line 3"),
            "expected active-cell rendering to stay top-anchored: {rendered_rows:?}",
        );
        assert!(
            rendered_rows
                .iter()
                .all(|row| !row.contains("active line 5")),
            "bottom-scrolled rendering would hide the command header/output prefix: {rendered_rows:?}",
        );
    }

    #[test]
    fn clipped_active_exec_cell_keeps_omission_hint_visible() {
        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);
        let call_id = "exec-overflow".to_string();
        let mut cell = ExecCell::new(
            ExecCall {
                call_id: call_id.clone(),
                command: vec![
                    "bash".to_string(),
                    "-lc".to_string(),
                    "seq 1 40".to_string(),
                ],
                parsed: Vec::new(),
                output: None,
                source: ExecCommandSource::Agent,
                start_time: Some(Instant::now()),
                duration: None,
                interaction_input: None,
            },
            /*animations_enabled*/ false,
        );
        assert!(
            cell.complete_call(
                &call_id,
                CommandOutput::new(
                    0,
                    (1..=40)
                        .map(|line| format!("output line {line:02}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Duration::from_millis(1),
            )
        );
        let renderable = TranscriptAreaRenderable {
            child: &cell,
            top: 0,
            right: 0,
            persistent_layout: None,
        };

        renderable.render(area, &mut buf);

        let rendered = buffer_rows(&buf, area)
            .into_iter()
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);

        let narrow_area = Rect::new(0, 0, 24, 6);
        let mut narrow_buf = Buffer::empty(narrow_area);
        renderable.render(narrow_area, &mut narrow_buf);
        let narrow_rendered = buffer_rows(&narrow_buf, narrow_area)
            .into_iter()
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            narrow_rendered.contains("… +") && narrow_rendered.contains("output line 40"),
            "narrow clipping should retain its hint and output tail: {narrow_rendered:?}"
        );

        let url_call_id = "exec-url-overflow".to_string();
        let mut url_cell = ExecCell::new(
            ExecCall {
                call_id: url_call_id.clone(),
                command: vec!["curl".to_string(), "https://example.com".to_string()],
                parsed: Vec::new(),
                output: None,
                source: ExecCommandSource::Agent,
                start_time: Some(Instant::now()),
                duration: None,
                interaction_input: None,
            },
            /*animations_enabled*/ false,
        );
        assert!(url_cell.complete_call(
            &url_call_id,
            CommandOutput::new(
                0,
                format!("https://example.com/{}\nlast output", "a".repeat(120)),
            ),
            Duration::from_millis(1),
        ));
        let url_renderable = TranscriptAreaRenderable {
            child: &url_cell,
            top: 0,
            right: 0,
            persistent_layout: None,
        };
        let mut url_buf = Buffer::empty(narrow_area);
        url_renderable.render(narrow_area, &mut url_buf);
        let url_rendered = buffer_rows(&url_buf, narrow_area)
            .into_iter()
            .map(|row| row.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            url_rendered.contains("… +") && url_rendered.contains("last output"),
            "long URL clipping should retain its hint and output tail: {url_rendered:?}"
        );
    }
}
