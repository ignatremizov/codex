//! Bounded incremental projection of two already-bounded V1 identity authorities.
//!
//! Replayed chains and emitted deltas have independent limits: a sequence of individually small
//! updates cannot accumulate an unbounded mapping or generate an oversized removal list.

use std::collections::HashMap;
use std::collections::HashSet;

use serde_json::Value;
use serde_json::json;

const DELTA_INSTRUCTIONS: &str =
    "Agent identity changes: upsert by ref, remove listed refs; all other mappings stay unchanged.";
const EMPTY_AUTHORITY_INSTRUCTIONS: &str = "This is the current receiving root's agent identity \
    mapping and replaces earlier mappings. Refs identify only the agents listed here; use a \
    canonical agent_id for other agents. Task paths and nicknames are descriptive and may change.";
const OMITTED_IDENTITIES: &str =
    "Additional identities are omitted; use the existing agent directory/discovery for details.";
// Current full authorities are capped at 2,048 approximate tokens. Allow a complete
// delta too, but never turn unbounded/malformed historical refs into a new fragment.
const MAX_FRAGMENT_BYTES: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IdentityMapping {
    entries: Vec<Value>,
    omitted: bool,
}

impl IdentityMapping {
    pub(super) fn from_authority_body(body: &str) -> Option<Self> {
        if body.len() > MAX_FRAGMENT_BYTES {
            return None;
        }
        if body.is_empty() {
            return Some(Self {
                entries: Vec::new(),
                omitted: false,
            });
        }
        let (instructions, payload) = body.split_once('\n')?;
        Self::from_value(
            serde_json::from_str(payload).ok()?,
            instructions.contains(OMITTED_IDENTITIES),
        )
    }

    fn from_value(value: Value, omitted: bool) -> Option<Self> {
        let entries = value.as_array()?.clone();
        let mut refs = HashSet::new();
        for entry in &entries {
            let agent_ref = entry.get("ref")?.as_str()?;
            if !agent_ref.bytes().all(|byte| byte.is_ascii_digit())
                || agent_ref
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|number| number == 0)
                || !refs.insert(agent_ref)
            {
                return None;
            }
        }
        Some(Self { entries, omitted })
    }

    pub(super) fn full_body(authority_body: &str) -> String {
        if authority_body.is_empty() {
            format!("{EMPTY_AUTHORITY_INSTRUCTIONS}\n[]")
        } else {
            authority_body.to_string()
        }
    }

    pub(super) fn delta_from(&self, previous: &Self) -> Option<IdentityDelta> {
        if self.omitted != previous.omitted {
            return None;
        }
        let previous_by_ref = previous
            .entries
            .iter()
            .filter_map(|entry| Some((entry.get("ref")?.as_str()?, entry)))
            .collect::<HashMap<_, _>>();
        let current_by_ref = self
            .entries
            .iter()
            .filter_map(|entry| Some((entry.get("ref")?.as_str()?, entry)))
            .collect::<HashMap<_, _>>();
        let upsert = self
            .entries
            .iter()
            .filter(|entry| {
                entry
                    .get("ref")
                    .and_then(Value::as_str)
                    .is_some_and(|agent_ref| {
                        previous_by_ref
                            .get(agent_ref)
                            .is_none_or(|previous| *previous != *entry)
                    })
            })
            .cloned()
            .collect();
        let removed_refs = previous
            .entries
            .iter()
            .filter_map(|entry| entry.get("ref").and_then(Value::as_str))
            .filter(|agent_ref| !current_by_ref.contains_key(agent_ref))
            .map(str::to_string)
            .collect();
        let delta = IdentityDelta {
            upsert,
            removed_refs,
        };
        let framing = "<agent_identity_context>\n\n</agent_identity_context>".len();
        (delta.body().len() <= MAX_FRAGMENT_BYTES.saturating_sub(framing)).then_some(delta)
    }

    fn apply(&mut self, delta: &IdentityDelta) -> bool {
        for removed_ref in &delta.removed_refs {
            self.entries.retain(|entry| {
                entry.get("ref").and_then(Value::as_str) != Some(removed_ref.as_str())
            });
        }
        for upsert in &delta.upsert {
            let Some(agent_ref) = upsert.get("ref").and_then(Value::as_str) else {
                return false;
            };
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.get("ref").and_then(Value::as_str) == Some(agent_ref))
            {
                *entry = upsert.clone();
            } else {
                self.entries.push(upsert.clone());
            }
        }
        serde_json::to_string(&self.entries).is_ok_and(|entries| {
            entries
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .len()
                <= MAX_FRAGMENT_BYTES
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IdentityDelta {
    upsert: Vec<Value>,
    removed_refs: Vec<String>,
}

impl IdentityDelta {
    pub(super) fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.removed_refs.is_empty()
    }

    pub(super) fn body(&self) -> String {
        let payload = json!({
            "upsert": self.upsert,
            "removed_refs": self.removed_refs,
        })
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
        format!("{DELTA_INSTRUCTIONS}\n{payload}")
    }

    fn from_value(value: Value) -> Option<Self> {
        let object = value.as_object()?;
        if object
            .keys()
            .any(|key| key != "upsert" && key != "removed_refs")
        {
            return None;
        }
        let upsert = object.get("upsert")?.as_array()?.clone();
        IdentityMapping::from_value(Value::Array(upsert.clone()), /*omitted*/ false)?;
        let removed_refs = object
            .get("removed_refs")?
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut unique_removed = HashSet::new();
        if removed_refs
            .iter()
            .any(|agent_ref| !unique_removed.insert(agent_ref.as_str()))
        {
            return None;
        }
        if upsert.iter().any(|entry| {
            entry
                .get("ref")
                .and_then(Value::as_str)
                .is_some_and(|agent_ref| unique_removed.contains(agent_ref))
        }) {
            return None;
        }
        Some(Self {
            upsert,
            removed_refs,
        })
    }
}

enum IdentityFragment {
    Full(IdentityMapping),
    Delta(IdentityDelta),
}

fn parse_fragment_body(body: &str) -> Option<IdentityFragment> {
    if body.len() > MAX_FRAGMENT_BYTES {
        return None;
    }
    let (_, payload) = body.split_once('\n')?;
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.is_array() {
        let (instructions, _) = body.split_once('\n')?;
        IdentityMapping::from_value(value, instructions.contains(OMITTED_IDENTITIES))
            .map(IdentityFragment::Full)
    } else {
        IdentityDelta::from_value(value).map(IdentityFragment::Delta)
    }
}

pub(super) fn body_from_rendered_fragment<'a>(
    text: &'a str,
    markers: (&str, &str),
) -> Option<&'a str> {
    text.strip_prefix(markers.0)?
        .strip_suffix(markers.1)?
        .strip_prefix('\n')?
        .strip_suffix('\n')
}

pub(super) fn resolve_retained_chain<'a>(
    bodies: impl IntoIterator<Item = &'a str>,
) -> Option<IdentityMapping> {
    let mut mapping = None;
    let mut usable = true;
    for body in bodies {
        match parse_fragment_body(body) {
            Some(IdentityFragment::Full(full)) => {
                mapping = Some(full);
                usable = true;
            }
            Some(IdentityFragment::Delta(delta)) => {
                let Some(current) = mapping.as_mut() else {
                    usable = false;
                    continue;
                };
                if !current.apply(&delta) {
                    mapping = None;
                    usable = false;
                }
            }
            None => {
                mapping = None;
                usable = false;
            }
        }
    }
    if usable { mapping } else { None }
}
