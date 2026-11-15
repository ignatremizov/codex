use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn targets_preserve_string_and_array_call_shapes() {
    for (value, batch) in [
        (json!("43"), false),
        (json!(["43"]), true),
        (json!(["43", "44"]), true),
    ] {
        let args: SendInputArgs =
            serde_json::from_value(json!({"target": value, "message": "hello"})).unwrap();
        assert_eq!(matches!(args.target, SendInputTarget::Batch(_)), batch);
    }
    for value in [json!(43), json!([43]), json!(null), json!({})] {
        assert!(serde_json::from_value::<SendInputArgs>(json!({"target": value})).is_err());
    }
}
