//! Text retained for one unified exec output stream and the bytes omitted by its collection cap.

use codex_utils_head_tail_buffer::HeadTailBuffer;
use codex_utils_head_tail_buffer::format_output_omission_marker;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_output_truncation::truncate_text;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CapturedStream {
    pub(crate) text: String,
    pub(crate) omitted_bytes: usize,
}

impl From<&str> for CapturedStream {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            omitted_bytes: 0,
        }
    }
}

impl<const N: usize> From<&HeadTailBuffer<N>> for CapturedStream {
    fn from(buffer: &HeadTailBuffer<N>) -> Self {
        Self {
            text: String::from_utf8_lossy(&buffer.to_bytes_with_omission_marker()).into_owned(),
            omitted_bytes: buffer.omitted_bytes(),
        }
    }
}

impl CapturedStream {
    pub(crate) fn render(
        &self,
        policy: Option<TruncationPolicy>,
        original_tokens: Option<usize>,
    ) -> String {
        let truncated = match policy {
            Some(policy) if self.omitted_bytes == 0 => {
                return formatted_truncate_text(&self.text, policy);
            }
            Some(policy) => truncate_text(&self.text, policy),
            None if self.omitted_bytes == 0 => return self.text.clone(),
            None => self.text.clone(),
        };
        if policy.is_none_or(|policy| self.text.len() <= policy.byte_budget()) {
            // A retained transcript can contain several positioned gaps, each smaller
            // than the total omission count. Do not invent a second aggregate gap.
            let has_omission_notice = self.text.lines().any(|line| {
                line.strip_prefix("... ")
                    .and_then(|line| line.strip_suffix(" bytes omitted ..."))
                    .or_else(|| {
                        line.strip_prefix("Warning: ").and_then(|line| {
                            line.strip_suffix(
                                " bytes were omitted at multiple locations in retained process output.",
                            )
                        })
                    })
                    .is_some_and(|count| count.parse::<usize>().is_ok())
            });
            if has_omission_notice {
                return truncated;
            }
            // Other producers can supply omission metadata without rendered markers.
            let marker = format_output_omission_marker(self.omitted_bytes);
            return format!("{marker}\n{truncated}");
        }
        let tokens = original_tokens.unwrap_or_else(|| approx_token_count(&self.text));
        let omitted_bytes = self.omitted_bytes;
        format!(
            "Warning: truncated output (original token count: {tokens})\n\
             Warning: {omitted_bytes} bytes were omitted while collecting command output.\n\n\
             {truncated}"
        )
    }
}
