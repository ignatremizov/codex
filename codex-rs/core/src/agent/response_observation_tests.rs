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
    target_messages: bool,
    queue_input: bool,
) -> ResponseObservationPolicy {
    ResponseObservationPolicy {
        commentary,
        final_response,
        target_messages,
        queue_input,
    }
}

#[test]
fn wire_policies_map_to_target_turn_handling() {
    let cases = [
        (
            None,
            expected_policy(false, FinalResponseObservation::Passive, false, false),
        ),
        (
            Some("c"),
            expected_policy(true, FinalResponseObservation::Passive, false, false),
        ),
        (
            Some("f"),
            expected_policy(false, FinalResponseObservation::Wake, false, false),
        ),
        (
            Some("cf"),
            expected_policy(true, FinalResponseObservation::Wake, false, false),
        ),
        (
            Some("m"),
            expected_policy(false, FinalResponseObservation::Passive, true, false),
        ),
        (
            Some("q"),
            expected_policy(false, FinalResponseObservation::Passive, false, true),
        ),
        (
            Some("fm"),
            expected_policy(false, FinalResponseObservation::Wake, true, false),
        ),
        (
            Some("fq"),
            expected_policy(false, FinalResponseObservation::Wake, false, true),
        ),
        (
            Some("mq"),
            expected_policy(false, FinalResponseObservation::Passive, true, true),
        ),
        (
            Some("cfmq"),
            expected_policy(true, FinalResponseObservation::Wake, true, true),
        ),
        (
            Some("x"),
            expected_policy(
                false,
                FinalResponseObservation::PresentationOnly,
                false,
                false,
            ),
        ),
        (
            Some("cx"),
            expected_policy(
                true,
                FinalResponseObservation::PresentationOnly,
                false,
                false,
            ),
        ),
        (
            Some("fx"),
            expected_policy(false, FinalResponseObservation::Passive, false, false),
        ),
        (
            Some("cfx"),
            expected_policy(true, FinalResponseObservation::Passive, false, false),
        ),
        (
            Some("cfmqx"),
            expected_policy(true, FinalResponseObservation::Passive, true, true),
        ),
    ];

    for (wire, expected) in cases {
        assert_eq!(policy(wire).expect("valid policy"), expected);
    }
}

#[test]
fn malformed_or_noncanonical_wire_policies_are_rejected() {
    for value in ["", "fc", "xf", "mc", "qm", "cc", "mm", "z"] {
        assert!(policy(Some(value)).is_err(), "{value} should be rejected");
    }
}

#[test]
fn explicit_user_none_does_not_become_passive_final_delivery() {
    let handling = UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*target_messages*/ true,
        /*queue_input*/ false,
    );

    let policy = ResponseObservationPolicy::from(handling);
    assert_eq!(
        policy,
        expected_policy(false, FinalResponseObservation::None, true, false)
    );
    assert!(policy.exposes_source_model_context());
}
