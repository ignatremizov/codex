use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn ps_retention_is_independent_of_local_display_limit_and_chunk_boundaries() {
    let (mut chat, mut events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.command_output_preview_lines = 1;
    chat.track_unified_exec_process_begin(
        "call",
        Some("process"),
        "printf output",
        /*user_shell_response_handling*/ None,
    );
    for chunk in ["first", " line\r", "\nsecond\n", "third\nfourth\nfifth\n"] {
        chat.track_unified_exec_output_chunk("call", chunk.as_bytes());
    }
    let expected = vec!["first line", "second", "third", "fourth", "fifth"];
    assert_eq!(
        chat.unified_exec_processes[0]
            .recent_chunks
            .transcript_lines()
            .collect::<Vec<_>>(),
        expected,
    );
    let mut snapshots = Vec::new();
    for limit in [1, 2, 0, 30] {
        chat.local_settings.tui.command_output_preview_lines = limit;
        chat.add_ps_output();
        let cell = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|event| match event {
                AppEvent::InsertHistoryCell(cell) => Some(cell),
                _ => None,
            })
            .expect("/ps history");
        let full = lines_to_single_string(&cell.transcript_lines(/*width*/ 100));
        assert!(full.contains("first line\n"));
        assert!(full.contains("fifth\n"));
        let rendered = lines_to_single_string(&cell.display_lines(/*width*/ 100));
        if limit == 0 || limit == 30 {
            assert_eq!(rendered, full);
        }
        snapshots.push(format!("limit {limit}\n{}", rendered.trim_end()));
    }
    insta::assert_snapshot!(snapshots.join("\n\n"), @r"
    limit 1
    /ps

    Background terminals

      • printf output
        ↳ … +5 rows (ctrl+t to view transcript)

    limit 2
    /ps

    Background terminals

      • printf output
        ↳ first line
          … +4 rows (ctrl+t to view transcript)

    limit 0
    /ps

    Background terminals

      • printf output
        ↳ first line
          second
          third
          fourth
          fifth

    limit 30
    /ps

    Background terminals

      • printf output
        ↳ first line
          second
          third
          fourth
          fifth
    ");
    assert_eq!(
        chat.unified_exec_processes[0]
            .recent_chunks
            .transcript_lines()
            .collect::<Vec<_>>(),
        expected,
    );
}

#[tokio::test]
async fn ps_zero_limit_remains_bounded_and_exposes_storage_omissions() {
    let (mut chat, mut events, _operations) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.command_output_preview_lines = 0;
    chat.track_unified_exec_process_begin(
        "call",
        Some("process"),
        "printf output",
        /*user_shell_response_handling*/ None,
    );
    for n in 0..100_000 {
        chat.track_unified_exec_output_chunk("call", format!("output line {n}\n").as_bytes());
    }
    let retained = &chat.unified_exec_processes[0].recent_chunks;
    assert!(retained.retained_lines() <= 100);
    assert_eq!(retained.total_lines(), 100_000);
    chat.add_ps_output();
    let cell = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(cell),
            _ => None,
        })
        .expect("/ps history");
    assert_eq!(
        cell.display_lines(/*width*/ 100),
        cell.transcript_lines(/*width*/ 100),
    );
    let rendered = lines_to_single_string(&cell.display_lines(/*width*/ 100));
    assert!(rendered.contains("… +99900 lines"));
    assert!(rendered.contains("output line 0\n"));
    assert!(rendered.contains("output line 99999\n"));
}
