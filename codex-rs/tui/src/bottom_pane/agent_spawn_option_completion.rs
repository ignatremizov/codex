//! Completion candidates for user-authored agent spawn model settings.

use codex_protocol::openai_models::ModelPreset;

use super::agent_target_popup::AgentPromptTarget;
use super::agent_target_popup::token_end;

pub(super) fn model_targets(models: &[ModelPreset]) -> Vec<AgentPromptTarget> {
    models
        .iter()
        .filter(|model| model.show_in_picker)
        .map(|model| {
            let label = if model.description.is_empty() {
                model.display_name.clone()
            } else {
                format!("{} · {}", model.display_name, model.description)
            };
            AgentPromptTarget {
                thread_id: None,
                selector: model_selector(model),
                label,
            }
        })
        .collect()
}

pub(super) fn reasoning_effort_targets(
    models: &[ModelPreset],
    authored_options: &str,
) -> Vec<AgentPromptTarget> {
    let Some(selected_model) = authored_model(models, authored_options) else {
        return Vec::new();
    };
    selected_model
        .supported_reasoning_efforts
        .iter()
        .map(|preset| AgentPromptTarget {
            thread_id: None,
            selector: format!("effort:{}", preset.effort),
            label: preset.description.clone(),
        })
        .collect()
}

pub(super) fn authored_model<'a>(
    models: &'a [ModelPreset],
    input: &str,
) -> Option<&'a ModelPreset> {
    let selector = authored_model_selector(input)?;
    model_for_selector(models, &selector)
}

pub(super) fn model_for_selector<'a>(
    models: &'a [ModelPreset],
    selector: &str,
) -> Option<&'a ModelPreset> {
    let authored_model = decode_model_selector(selector)?;
    models.iter().find(|model| model.model == authored_model)
}

fn model_selector(model: &ModelPreset) -> String {
    if model
        .model
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'))
    {
        let model = model.model.replace('\\', "\\\\").replace('"', "\\\"");
        format!("model:\"{model}\"")
    } else {
        format!("model:{}", model.model)
    }
}

fn authored_model_selector(input: &str) -> Option<String> {
    let mut cursor = 0;
    let mut token_index = 0;
    while cursor < input.len() {
        let tail = &input[cursor..];
        cursor += tail.len() - tail.trim_start().len();
        if cursor == input.len() {
            break;
        }
        let end = token_end(input, cursor);
        let token = &input[cursor..end];
        if token_index < 2 {
            token_index += 1;
            cursor = end;
            continue;
        }
        if token == "--"
            || !["fork:", "w:", "model:", "effort:"]
                .iter()
                .any(|prefix| token.starts_with(prefix))
        {
            break;
        }
        if token.starts_with("model:") {
            return Some(token.to_string());
        }
        cursor = end;
    }
    None
}

fn decode_model_selector(selector: &str) -> Option<String> {
    let selector = selector.strip_prefix("model:")?;
    let mut decoded = String::new();
    let mut chars = selector.chars();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if quoted && ch == '\\' {
            let escaped = chars.next()?;
            if !matches!(escaped, '"' | '\\') {
                return None;
            }
            decoded.push(escaped);
        } else if ch == '"' {
            quoted = !quoted;
        } else {
            decoded.push(ch);
        }
    }
    (!quoted && !decoded.is_empty()).then_some(decoded)
}
