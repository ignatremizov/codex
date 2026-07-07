//! Persisted tool projections retain rich detail, styles, and truthful outcomes.

use super::cells;
use crate::diff_model::FileChange;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::multi_agents::AgentPreviewLineLimits;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::ImageGenerationItem;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::SubAgentActivityKind;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::WebSearchAction;
use codex_app_server_protocol::WebSearchItem;
use codex_utils_path_uri::LegacyAppPathString;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::test]
async fn cold_compaction_projection_respects_preference_and_preserves_detail() -> anyhow::Result<()>
{
    let home = tempfile::tempdir()?;
    let mut config = crate::legacy_core::config::ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(codex_config::LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?;
    let cwd = test_path_buf("/workspace").abs();
    let item = ThreadItem::ContextCompaction {
        id: "compact-1".into(),
        summary: Some("Short summary".into()),
        message: Some("Prompt line 1\n\nPrompt line 2".into()),
        available_skills: vec!["test-tui".into()],
        decode_error: None,
    };
    let mut rendered = Vec::new();
    // No config follows the effective default; explicit opt-out hides all details.
    config.show_compact_summary = false;
    for preference in [None, Some(&config)] {
        let projected = crate::thread_transcript::thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &cwd,
            [item.clone()],
            crate::thread_transcript::RawReasoningVisibility::Hidden,
            preference,
        );
        rendered.push(
            projected
                .into_iter()
                .flat_map(|cell| cell.display_lines(/*width*/ 80))
                .map(|line| line.to_string().trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    insta::assert_snapshot!(rendered.join("\n---\n"), @"
    • Context compacted
      Prompt line 1

      Prompt line 2
    ---
    • Context compacted
    ");
    Ok(())
}

#[test]
fn cold_compaction_decode_error_is_visible_with_content_hidden() {
    let cwd = test_path_buf("/workspace").abs();
    let mut rendered = Vec::new();
    for show_compact_summary in [true, false] {
        let projected = cells(
            ThreadItem::ContextCompaction {
                id: "compact-1".into(),
                summary: Some("summary".into()),
                message: Some("full prompt".into()),
                available_skills: vec!["test-tui".into()],
                decode_error: Some("Decoder unavailable.".into()),
            },
            &cwd,
            show_compact_summary,
            AgentPreviewLineLimits::default(),
        );
        assert_eq!(projected.len(), 1);
        rendered.push(
            projected[0]
                .display_lines(/*width*/ 80)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    insta::assert_snapshot!(rendered.join("\n---\n"), @"
    • Context compacted
      Compacted prompt decoding failed: Decoder unavailable.
      full prompt
    ---
    • Context compacted
      Compacted prompt decoding failed: Decoder unavailable.
    ");
}

#[test]
fn completed_patch_restores_rich_diff_and_styles() {
    let cwd = test_path_buf("/workspace").abs();
    let mut changes = vec![
        FileUpdateChange {
            path: "new.rs".to_string(),
            kind: PatchChangeKind::Add,
            diff: "fn greet() {\n    println!(\"hello\");\n}\n".to_string(),
        },
        FileUpdateChange {
            path: "old.txt".to_string(),
            kind: PatchChangeKind::Delete,
            diff: "outdated\n".to_string(),
        },
        FileUpdateChange {
            path: "src/before.rs".to_string(),
            kind: PatchChangeKind::Update {
                move_path: Some(PathBuf::from("src/after.rs")),
            },
            diff: "@@ -1 +1 @@\n-let before = 1;\n+let after = 2;\n".to_string(),
        },
    ];
    let expected = history_cell::new_patch_event(
        HashMap::from([
            (
                PathBuf::from("new.rs"),
                FileChange::Add {
                    content: changes[0].diff.clone(),
                },
            ),
            (
                PathBuf::from("old.txt"),
                FileChange::Delete {
                    content: changes[1].diff.clone(),
                },
            ),
            (
                PathBuf::from("src/before.rs"),
                FileChange::Update {
                    unified_diff: changes[2].diff.clone(),
                    move_path: Some(PathBuf::from("src/after.rs")),
                },
            ),
        ]),
        cwd.as_path(),
    );
    changes[2].diff.push_str("\n\nMoved to: src/after.rs");
    let actual = cells(
        ThreadItem::FileChange {
            id: "patch-1".to_string(),
            changes,
            status: PatchApplyStatus::Completed,
        },
        &cwd,
        /*show_compact_summary*/ true,
        AgentPreviewLineLimits::default(),
    );

    assert_eq!(actual.len(), 1);
    assert_eq!(
        actual[0].transcript_hyperlink_lines(/*width*/ 80),
        expected.transcript_hyperlink_lines(/*width*/ 80),
    );
}

#[test]
fn unfinished_and_rejected_patches_keep_their_outcome() {
    let cwd = test_path_buf("/workspace").abs();
    let rendered = [
        PatchApplyStatus::InProgress,
        PatchApplyStatus::Declined,
        PatchApplyStatus::Failed,
    ]
    .into_iter()
    .flat_map(|status| {
        cells(
            ThreadItem::FileChange {
                id: "patch-1".to_string(),
                changes: vec![FileUpdateChange {
                    path: "main.rs".to_string(),
                    kind: PatchChangeKind::Add,
                    diff: "fn main() {}\n".to_string(),
                }],
                status,
            },
            &cwd,
            /*show_compact_summary*/ true,
            AgentPreviewLineLimits::default(),
        )
    })
    .flat_map(|cell| cell.display_lines(/*width*/ 80))
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    insta::assert_snapshot!(rendered, @"
    • Patch application in progress
    • Patch application declined
    ✘ Failed to apply patch
    ");
}

#[test]
fn tool_and_notice_projection_uses_normal_transcript_presentation() {
    let cwd = test_path_buf("/workspace").abs();
    let image = ImageGenerationItem {
        id: "image-1".to_string(),
        status: "completed".to_string(),
        revised_prompt: Some("A diagram of the history pages".to_string()),
        result: String::new(),
        transparent_background: None,
        failure: None,
        saved_path: None,
        imagegen_request_id: None,
        generation_id: None,
    };
    let items = vec![
        ThreadItem::EnteredReviewMode {
            id: "review-start".to_string(),
            review: "current changes".to_string(),
        },
        ThreadItem::WebSearch(WebSearchItem {
            id: "search-1".to_string(),
            query: "fallback query".to_string(),
            action: Some(WebSearchAction::FindInPage {
                url: Some("https://example.com".to_string()),
                pattern: Some("pagination".to_string()),
            }),
            results: None,
        }),
        ThreadItem::ImageView {
            id: "view-1".to_string(),
            path: LegacyAppPathString::from_string("diagram.png".to_string()),
        },
        ThreadItem::ImageGeneration(image.clone()),
        ThreadItem::ImageGeneration(ImageGenerationItem {
            status: "in_progress".into(),
            ..image
        }),
        ThreadItem::SubAgentActivity {
            id: "agent-1".to_string(),
            kind: SubAgentActivityKind::Completed,
            agent_thread_id: "01912345-1234-7123-8123-123456789abc".to_string(),
            agent_path: "/root/reviewer".to_string(),
        },
        ThreadItem::ExitedReviewMode {
            id: "review-end".to_string(),
            review: "No findings".to_string(),
        },
        ThreadItem::ContextCompaction {
            id: "compact-1".to_string(),
            summary: None,
            message: None,
            available_skills: Vec::new(),
            decode_error: None,
        },
    ];
    let rendered = items
        .into_iter()
        .flat_map(|item| {
            cells(
                item,
                &cwd,
                /*show_compact_summary*/ true,
                AgentPreviewLineLimits::default(),
            )
        })
        .flat_map(|cell| cell.display_lines(/*width*/ 80))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @"
    >> Code review started: current changes <<
    • Searched for 'pagination' in https://example.com
    • Viewed image diagram.png
    • Generated Image:
      └ A diagram of the history pages
    • Image generation · in_progress
    • Completed `/root/reviewer`
    << Code review finished: No findings >>
    • Context compacted
    ");
}
