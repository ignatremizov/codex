//! Normalize encoded host paths before snapshot clipping fingerprints their metadata.

use regex_lite::Regex;
use serde_json::Value;
use std::sync::OnceLock;

pub(super) fn normalize(text: &str) -> String {
    static METADATA: OnceLock<Regex> = OnceLock::new();
    METADATA
        .get_or_init(|| {
            Regex::new(r"<promoted_skills>([^<]*)</promoted_skills>")
                .expect("promoted skill metadata regex")
        })
        .replace_all(text, |captures: &regex_lite::Captures<'_>| {
            let original = &captures[0];
            let Ok(Value::Array(mut identities)) = serde_json::from_str::<Value>(&captures[1])
            else {
                return original.to_string();
            };
            let mut changed = false;
            for identity in &mut identities {
                for field in ["packageHex", "resourceHex"] {
                    let Some(encoded) = identity.get(field).and_then(Value::as_str) else {
                        continue;
                    };
                    if !encoded.len().is_multiple_of(2) || !encoded.is_ascii() {
                        continue;
                    }
                    let decoded = (0..encoded.len())
                        .step_by(2)
                        .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16))
                        .collect::<Result<Vec<_>, _>>()
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok());
                    let Some(decoded) = decoded else {
                        continue;
                    };
                    let normalized = super::normalize_skill_path(&decoded);
                    if normalized != decoded {
                        identity[field] = Value::String(
                            normalized
                                .bytes()
                                .map(|byte| format!("{byte:02x}"))
                                .collect(),
                        );
                        changed = true;
                    }
                }
            }
            if changed {
                format!(
                    "<promoted_skills>{}</promoted_skills>",
                    Value::Array(identities)
                )
            } else {
                original.to_string()
            }
        })
        .into_owned()
}
