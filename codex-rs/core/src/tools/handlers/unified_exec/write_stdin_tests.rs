use super::WriteStdinArgs;
use pretty_assertions::assert_eq;

#[test]
fn wait_until_exit_defaults_to_false() {
    let args = serde_json::from_str::<WriteStdinArgs>(r#"{"session_id":42}"#)
        .expect("omitted wait_until_exit should deserialize");

    assert_eq!(
        args,
        WriteStdinArgs {
            session_id: 42,
            chars: String::new(),
            yield_time_ms: None,
            wait_until_exit: false,
            max_output_tokens: None,
        }
    );
}

#[test]
fn wait_until_exit_accepts_true() {
    let args =
        serde_json::from_str::<WriteStdinArgs>(r#"{"session_id":42,"wait_until_exit":true}"#)
            .expect("wait_until_exit true should deserialize");

    assert_eq!(
        args,
        WriteStdinArgs {
            session_id: 42,
            chars: String::new(),
            yield_time_ms: None,
            wait_until_exit: true,
            max_output_tokens: None,
        }
    );
}
