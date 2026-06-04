#[derive(Debug, Eq, PartialEq)]
pub(super) enum ContextCompactionRender<'a> {
    DecodeError(String),
    Section {
        title: &'static str,
        content: &'a str,
        empty_label: &'static str,
    },
    Compacted,
}

pub(super) fn context_compaction_render_plan<'a>(
    summary: Option<&'a str>,
    message: Option<&'a str>,
    decode_error: Option<&'a str>,
    show_compact_summary: bool,
) -> Vec<ContextCompactionRender<'a>> {
    let summary = summary.filter(|text| !text.trim().is_empty());
    let message = message.filter(|text| !text.trim().is_empty());
    let decode_error = decode_error.filter(|text| !text.trim().is_empty());
    let mut plan = Vec::new();
    if let Some(error) = decode_error {
        plan.push(ContextCompactionRender::DecodeError(format!(
            "compacted prompt decoding failed: {error}"
        )));
    }

    if !show_compact_summary {
        if plan.is_empty() {
            plan.push(ContextCompactionRender::Compacted);
        }
        return plan;
    }

    if let Some(content) = message {
        plan.push(ContextCompactionRender::Section {
            title: "Compacted prompt",
            content,
            empty_label: "(prompt was empty)",
        });
    } else if let Some(content) = summary {
        plan.push(ContextCompactionRender::Section {
            title: "Compacted summary",
            content,
            empty_label: "(summary was empty)",
        });
    } else if plan.is_empty() {
        plan.push(ContextCompactionRender::Compacted);
    }
    plan
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
