use super::*;

#[test]
fn rejects_more_than_the_bounded_json_node_count() {
    let json = format!(
        "[{}]",
        (0..MAX_JSON_SPAN_NODES)
            .map(|_| "0")
            .collect::<Vec<_>>()
            .join(",")
    );

    assert!(matches!(
        parse_json_spans(json.as_bytes()),
        Err("JSON node count exceeds media-vacuum limit")
    ));
}

#[test]
fn rejects_json_nested_beyond_the_depth_limit() {
    let json = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH.saturating_add(2)),
        "]".repeat(MAX_JSON_DEPTH.saturating_add(2))
    );

    assert!(matches!(
        parse_json_spans(json.as_bytes()),
        Err("JSON nesting exceeds media-vacuum limit")
    ));
}
