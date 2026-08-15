//! Read-only compatibility for route notices written by earlier fork versions.

pub(crate) struct LegacyAgentReplyRoute;

impl LegacyAgentReplyRoute {
    pub(crate) fn matches_text(text: &str) -> bool {
        let text = text.trim();
        let (start, end) = Self::type_markers();
        text.get(..start.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(start))
            && text
                .get(text.len().saturating_sub(end.len())..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(end))
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<agent_reply_route>", "</agent_reply_route>")
    }
}
