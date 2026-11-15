use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn shared_batch_flags_keep_normalized_single_recipient_semantics() {
    for (flags, expected) in [
        ("cf", "cf"),
        ("qq", "q"),
        ("ffx", "f"),
        ("fx", ""),
        ("xxf", "x"),
        ("z", "z"),
        ("fz", "zf"),
        ("zffx", "zf"),
        ("zfx", "z"),
        ("zfxx", "z"),
        ("zz", "z"),
    ] {
        let mode: SendInputMode = serde_json::from_value(json!(flags)).unwrap();
        assert_eq!(mode.normalized_flags(), expected);
    }
    for flags in ["", "m", "zc", "zq", "unknown"] {
        assert!(serde_json::from_value::<SendInputMode>(json!(flags)).is_err());
    }
}

#[test]
fn omitted_policy_is_not_an_explicit_passive_subscription() {
    #[derive(Deserialize)]
    struct InputPolicy {
        #[serde(default)]
        w: SendInputMode,
    }
    let omitted: InputPolicy = serde_json::from_value(json!({})).expect("omitted policy");
    assert!(matches!(omitted.w, SendInputMode::Default));
    let explicit: InputPolicy = serde_json::from_value(json!({"w": "fx"})).expect("passive policy");
    let SendInputMode::Response(policy) = explicit.w else {
        panic!("explicit flags must not become an omitted policy");
    };
    assert_eq!(policy.final_response(), FinalResponseObservation::Passive);
    assert!(!policy.commentary());
    assert!(!policy.queue_input());
}
