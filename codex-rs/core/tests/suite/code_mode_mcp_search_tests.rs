//! Exact advertised MCP names remain searchable in deferred Code Mode catalogs.

use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(false; "prefixed")]
#[test_case(true; "configured_namespace")]
async fn code_mode_search_finds_one_tool_by_advertised_name(non_prefixed: bool) -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let namespace = if non_prefixed { "rmcp" } else { "mcp__rmcp" };
    let name = format!("{namespace}__echo");
    let code = format!(
        r#"
const queries = [{name:?}, "echo", "Echo back the provided message"];
const matches = [];
for (const query of queries) {{
  const result = await tools.tool_search({{ query, limit: 1 }});
  matches.push(result.tools.map(tool => tool.name));
}}
text(JSON.stringify(matches));
"#
    );
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;
    let environment_id = remote_aware_environment_id();
    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = true)
        .with_config(move |config| {
            for feature in [Feature::CodeModeOnly, Feature::CodeModeToolSearch] {
                config
                    .features
                    .enable(feature)
                    .expect("enable Code Mode search");
            }
            if non_prefixed {
                config
                    .features
                    .enable(Feature::NonPrefixedMcpToolNames)
                    .expect("enable configured MCP namespaces");
            }
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "rmcp".to_string(),
                serde_json::from_value(json!({
                    "command": rmcp_test_server_bin,
                    "environment_id": environment_id,
                    "cwd": config.cwd,
                    "enabled_tools": ["echo"],
                }))
                .expect("valid test MCP config"),
            );
            config
                .mcp_servers
                .set(servers)
                .expect("set test MCP servers");
        });
    let test = builder.build_with_auto_env(&server).await?;
    wait_for_mcp_server(&test.codex, "rmcp").await?;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("search"),
            ev_custom_tool_call("search-call", "exec", &code),
            ev_completed("search"),
        ]),
    )
    .await;
    let follow_up = responses::mount_sse_once(&server, responses::sse_completed("done")).await;
    test.submit_turn("Find the advertised MCP tool.").await?;
    let request = follow_up.single_request();
    let (output, success) = custom_tool_output_body_and_success(&request, "search-call");
    assert_ne!(success, Some(false), "{output}");
    assert_eq!(
        serde_json::from_str::<Value>(&output)?,
        json!([[name], [name], [name]])
    );
    Ok(())
}
