use super::FinalResponseObservation;
use super::ResponseObservationPolicy;
use crate::agent::UserAgentFinalResponseHandling;
use crate::agent::UserAgentResponseHandling;
use pretty_assertions::assert_eq;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Args {
    #[serde(default)]
    w: ResponseObservationPolicy,
}

fn policy(w: Option<&str>) -> Result<ResponseObservationPolicy, serde_json::Error> {
    let value = match w {
        Some(w) => serde_json::json!({ "w": w }),
        None => serde_json::json!({}),
    };
    serde_json::from_value::<Args>(value).map(|args| args.w)
}

fn expected_policy(
    commentary: bool,
    final_response: FinalResponseObservation,
    queue_input: bool,
) -> ResponseObservationPolicy {
    ResponseObservationPolicy {
        commentary,
        final_response,
        queue_input,
    }
}

#[test]
fn wire_policies_map_to_target_turn_handling() {
    let cases = [
        (
            None,
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::Passive,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("c"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::Passive,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("f"),
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("cf"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::Wake,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("q"),
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::Passive,
                /*queue_input*/ true,
            ),
        ),
        (
            Some("fq"),
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
                /*queue_input*/ true,
            ),
        ),
        (
            Some("cfq"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::Wake,
                /*queue_input*/ true,
            ),
        ),
        (
            Some("x"),
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::PresentationOnly,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("cx"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::PresentationOnly,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("fx"),
            expected_policy(
                /*commentary*/ false,
                FinalResponseObservation::Passive,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("cfx"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::Passive,
                /*queue_input*/ false,
            ),
        ),
        (
            Some("cfqx"),
            expected_policy(
                /*commentary*/ true,
                FinalResponseObservation::Passive,
                /*queue_input*/ true,
            ),
        ),
    ];

    for (wire, expected) in cases {
        assert_eq!(policy(wire).expect("valid policy"), expected);
    }
}

#[test]
fn malformed_wire_policies_are_rejected() {
    for value in ["", "z", "qfz", "f x", "F"] {
        assert!(policy(Some(value)).is_err(), "{value} should be rejected");
    }
}

#[test]
fn wire_policy_order_and_repetitions_normalize_by_count() {
    for (input, canonical) in [
        ("fc", "cf"),
        ("qfx", "q"),
        ("qfxx", "qx"),
        ("xqc", "cqx"),
        ("ccqqxff", "cfq"),
        ("xxff", "fx"),
        ("xq", "qx"),
    ] {
        assert_eq!(
            policy(Some(input)).unwrap(),
            policy(Some(canonical)).unwrap()
        );
    }
    assert_eq!(policy(Some("xxff")).unwrap(), policy(None).unwrap());
}

#[test]
fn explicit_user_none_does_not_become_passive_final_delivery() {
    let handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*queue_input*/ false,
    );

    let policy = ResponseObservationPolicy::from(handling);
    assert_eq!(
        policy,
        expected_policy(
            /*commentary*/ false,
            FinalResponseObservation::None,
            /*queue_input*/ false,
        )
    );
    assert!(!policy.exposes_source_model_context());
}
