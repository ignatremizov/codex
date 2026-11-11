use regex::Regex;
use std::sync::LazyLock;

static OPENAI_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"sk-[A-Za-z0-9]{20,}"));
static OPENAI_PROJECT_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"sk-proj-[A-Za-z0-9_-]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r#"(?i)\b(api[_-]?key|token|secret|password)\b(\s*[:=]\s*)(["']?)[^\s"']{8,}"#)
});

/// Remove secret and keys from a String. This is done on best effort basis following some
/// well-known REGEX.
pub fn redact_secrets(input: String) -> String {
    let redacted = BEARER_TOKEN_REGEX.replace_all(&input, "Bearer [REDACTED_SECRET]");
    let redacted = OPENAI_PROJECT_KEY_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = OPENAI_KEY_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = AWS_ACCESS_KEY_ID_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = SECRET_ASSIGNMENT_REGEX.replace_all(&redacted, "$1$2$3[REDACTED_SECRET]");

    redacted.to_string()
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn redacts_project_keys_and_preserves_delimiters() {
        let project_key = format!("sk-proj-{}", "aB_9-".repeat(8));
        let legacy_key = format!("sk-{}", "aB09".repeat(8));
        let cases = [
            (project_key.clone(), "[REDACTED_SECRET]"),
            (legacy_key.clone(), "[REDACTED_SECRET]"),
            (
                format!("OPENAI_API_KEY={project_key}"),
                "OPENAI_API_KEY=[REDACTED_SECRET]",
            ),
            (
                format!("openai_api_key={project_key}"),
                "openai_api_key=[REDACTED_SECRET]",
            ),
            (
                format!("export OPENAI_API_KEY='{project_key}'; echo done"),
                "export OPENAI_API_KEY='[REDACTED_SECRET]'; echo done",
            ),
            (
                format!("({project_key}), followed by {legacy_key}."),
                "([REDACTED_SECRET]), followed by [REDACTED_SECRET].",
            ),
            (format!("Bearer {project_key}"), "Bearer [REDACTED_SECRET]"),
            (
                format!("API_KEY=\"{project_key}\""),
                "API_KEY=\"[REDACTED_SECRET]\"",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(redact_secrets(input), expected);
        }
    }

    #[test]
    fn avoids_short_project_key_false_positives() {
        let cases = [
            "sk-proj-",
            "sk-proj-abc",
            "sk-proj-abcdefghijklmnopqrs",
            "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrs",
            "sk-project-documentation",
            "sk-abcdefghijklmnopqrs",
        ];

        for input in cases {
            assert_eq!(redact_secrets(input.to_string()), input);
        }
    }

    #[test]
    fn redacts_supported_bearer_tokens() {
        let cases = [
            (
                "Bearer abcde+fghijklmnopqrstuvwxyz012345",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer abcdefghijklmnop+secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer sk-abcdefghijklmnopqrst+secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer AKIAABCDEFGHIJKLMNOP/~secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer AbcdefghijklMN09._~+/-==; echo done",
                "Bearer [REDACTED_SECRET]; echo done",
            ),
            (
                "authorization: bEaReR\tabcdefghijklmnop",
                "authorization: Bearer [REDACTED_SECRET]",
            ),
            ("Bearer   abcdefghijklmnop", "Bearer [REDACTED_SECRET]"),
        ];

        for (input, expected) in cases {
            assert_eq!(redact_secrets(input.to_string()), expected);
        }
    }

    #[test]
    fn avoids_bearer_false_positives() {
        let cases = [
            "Bearer of good news",
            "Bearer abcdefghijklmno",
            "NotABearer abcdefghijklmnop",
            "Bearerabcdefghijklmnop",
            "Bearer\nabcdefghijklmnop",
            "Bearer\u{a0}abcdefghijklmnop",
            "Bearer abcdefghijklmno\u{212a}",
        ];

        for input in cases {
            assert_eq!(redact_secrets(input.to_string()), input);
        }
    }
}
