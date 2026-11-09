use super::PendingInputPreview;
use crate::render::renderable::Renderable;
use insta::assert_snapshot;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

#[test]
fn scheduled_input_shares_preview_without_the_local_draft_edit_hint() {
    let mut preview = PendingInputPreview::new();
    preview
        .scheduled_messages
        .push("input-1 · 2026-11-10 17:30:00 +02:00 · Review deployment".into());
    let width = 100;
    let area = Rect::new(0, 0, width, preview.desired_height(width));
    let mut buffer = Buffer::empty(area);
    preview.render(area, &mut buffer);
    let rendered = buffer
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(rendered.trim(), @r"
    • Scheduled inputs · /schedule cancel <id>
      ↳ input-1 · 2026-11-10 17:30:00 +02:00 · Review deployment
    ");
}
