use super::WriteStdinArgs;
use crate::tools::context::ToolPayload;
use pretty_assertions::assert_eq;

#[test]
fn wait_until_exit_defaults_to_false() {
    let args = WriteStdinArgs::parse(&ToolPayload::Function {
        arguments: r#"{"session_id":42}"#.to_string(),
    })
    .expect("omitted wait_until_exit should deserialize");

    assert_eq!(
        args,
        WriteStdinArgs {
            chars: String::new(),
            yield_time_ms: None,
            wait_until_exit: false,
            max_output_tokens: None,
        }
    );
}

#[test]
fn wait_until_exit_accepts_true() {
    let args = WriteStdinArgs::parse(&ToolPayload::Function {
        arguments: r#"{"session_id":42,"wait_until_exit":true}"#.to_string(),
    })
    .expect("wait_until_exit true should deserialize");

    assert_eq!(
        args,
        WriteStdinArgs {
            chars: String::new(),
            yield_time_ms: None,
            wait_until_exit: true,
            max_output_tokens: None,
        }
    );
}
