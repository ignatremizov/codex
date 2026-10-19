//! Complete plan updates keep their explanation and source step order in every presentation.

use super::*;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;

#[test]
fn plan_updates_show_every_step_in_compact_and_detailed_presentations() {
    let cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(crate::history_cell::new_plan_update(
        UpdatePlanArgs {
            explanation: Some("Keep the whole plan visible.".into()),
            plan: vec![
                PlanItemArg {
                    step: "Inspect current behavior".into(),
                    status: StepStatus::Completed,
                },
                PlanItemArg {
                    step: "Update plan presentation".into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Run remote verification".into(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "Build the binaries".into(),
                    status: StepStatus::Pending,
                },
                PlanItemArg {
                    step: "Install the binaries".into(),
                    status: StepStatus::Pending,
                },
            ],
        },
    ))];
    for detailed in [false, true] {
        let mut view = TranscriptView::default();
        view.set_presentation(detailed, HistoryRenderMode::Rich);
        let rendered = text(&render(
            &mut view, &cells, /*width*/ 48, /*height*/ 12,
        ));
        insta::allow_duplicates! {
            insta::assert_snapshot!(
                rendered.trim_end(),
                @"
                • Updated Plan
                  └ Keep the whole plan visible.
                    ✔ Inspect current behavior
                    □ Update plan presentation
                    □ Run remote verification
                    □ Build the binaries
                    □ Install the binaries
                "
            );
        }
    }
}
