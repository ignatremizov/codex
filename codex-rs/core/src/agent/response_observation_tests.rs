use super::FinalResponseObservation;
use super::ResponseObservationPolicy;
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

#[test]
fn wire_policies_map_to_commentary_and_final_dispositions() {
    let cases = [
        (
            None,
            ResponseObservationPolicy {
                commentary: false,
                final_response: FinalResponseObservation::Passive,
            },
        ),
        (
            Some("c"),
            ResponseObservationPolicy {
                commentary: true,
                final_response: FinalResponseObservation::Passive,
            },
        ),
        (
            Some("f"),
            ResponseObservationPolicy {
                commentary: false,
                final_response: FinalResponseObservation::Wake,
            },
        ),
        (
            Some("cf"),
            ResponseObservationPolicy {
                commentary: true,
                final_response: FinalResponseObservation::Wake,
            },
        ),
        (
            Some("x"),
            ResponseObservationPolicy {
                commentary: false,
                final_response: FinalResponseObservation::None,
            },
        ),
        (
            Some("cx"),
            ResponseObservationPolicy {
                commentary: true,
                final_response: FinalResponseObservation::None,
            },
        ),
        (
            Some("fx"),
            ResponseObservationPolicy {
                commentary: false,
                final_response: FinalResponseObservation::Passive,
            },
        ),
        (
            Some("cfx"),
            ResponseObservationPolicy {
                commentary: true,
                final_response: FinalResponseObservation::Passive,
            },
        ),
    ];

    for (wire, expected) in cases {
        assert_eq!(policy(wire).expect("valid policy"), expected);
    }
}

#[test]
fn malformed_or_noncanonical_wire_policies_are_rejected() {
    for value in ["", "fc", "xf", "cc", "z"] {
        assert!(policy(Some(value)).is_err(), "{value} should be rejected");
    }
}
