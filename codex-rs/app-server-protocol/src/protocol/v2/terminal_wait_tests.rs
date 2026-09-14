use super::super::TerminalInteractionNotification;
use super::TerminalWait;

#[test]
fn terminal_wait_schema_uses_camel_case_variant_fields() {
    let schema = schemars::schema_for!(TerminalWait);
    let schema = serde_json::to_value(schema).expect("terminal wait schema should serialize");
    let variants = schema["oneOf"]
        .as_array()
        .expect("tagged terminal wait schema should have variants");

    let started = &variants[0]["properties"];
    assert!(started.get("interactionId").is_some());
    assert!(started.get("startedAtMs").is_some());
    assert!(started.get("interaction_id").is_none());
    assert!(started.get("started_at_ms").is_none());

    let finished = &variants[1]["properties"];
    assert!(finished.get("interactionId").is_some());
    assert!(finished.get("elapsedMs").is_some());
    assert!(finished.get("interaction_id").is_none());
    assert!(finished.get("elapsed_ms").is_none());
}

#[test]
fn terminal_interaction_typescript_dependency_includes_terminal_wait() {
    let declaration = <TerminalInteractionNotification as ts_rs::TS>::decl();
    assert!(
        declaration.contains("wait: TerminalWait | null"),
        "terminal wait should remain a named TypeScript dependency: {declaration}"
    );

    let dependencies = <TerminalInteractionNotification as ts_rs::TS>::dependencies();
    assert!(
        dependencies
            .iter()
            .any(|dependency| dependency.ts_name == "TerminalWait"),
        "TerminalWait should be exported as a TypeScript dependency: {dependencies:?}"
    );
}
