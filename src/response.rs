use serde_json::{json, Value};

/// Wrap a typed tool result as an MCP tools/call success payload.
///
/// An object (or other non-string) result is pretty-printed into the text
/// block and mirrored as `structuredContent`. A `Value::String` result is
/// treated as plain text — the raw string becomes the text block and no
/// `structuredContent` is attached (the spec's `structuredContent` is an
/// object, and quoting/escaping prose would mangle it) — so print-first apps
/// can return captured CLI output as-is.
pub fn tool_ok(value: Value) -> Value {
    if let Value::String(text) = value {
        return json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        });
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    })
}

pub fn result_frame(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// Upgrade a successful legacy-shaped result frame to the 2026-07-28 result
/// envelope. Error frames pass through unchanged.
pub fn modernize_result_frame(
    frame: &str,
    server_name: &str,
    server_version: &str,
    cacheable_list: bool,
) -> String {
    let Ok(mut frame) = serde_json::from_str::<Value>(frame) else {
        return frame.to_string();
    };
    let Some(result) = frame.get_mut("result").and_then(Value::as_object_mut) else {
        return frame.to_string();
    };

    result
        .entry("resultType")
        .or_insert_with(|| Value::String("complete".to_string()));
    let metadata = result
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    if let Some(metadata) = metadata {
        metadata.insert(
            "io.modelcontextprotocol/serverInfo".to_string(),
            json!({ "name": server_name, "version": server_version }),
        );
    }
    if cacheable_list {
        // No freshness policy is implied: zero means immediately stale, and
        // private prevents sharing app-specific lists between clients.
        result.entry("ttlMs").or_insert_with(|| json!(0));
        result
            .entry("cacheScope")
            .or_insert_with(|| Value::String("private".to_string()));
    }
    frame.to_string()
}

pub fn error_frame(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

/// `error_frame` plus DEC-04 classification: the stable `kind` and any
/// structured details ride in the JSON-RPC `data` member, the slot the spec
/// reserves for exactly this. An error carrying neither produces the same
/// bytes as [`error_frame`], so tagging is strictly additive.
pub fn error_frame_kinded(
    id: Value,
    code: i64,
    message: &str,
    kind: Option<&str>,
    data: Option<&Value>,
) -> String {
    let error = crate::types::ToolError {
        code,
        message: message.to_string(),
        kind: kind.map(str::to_string),
        data: data.cloned(),
    };
    error_frame_for(id, &error)
}

/// Render a [`ToolError`](crate::types::ToolError) as its JSON-RPC error frame.
pub fn error_frame_for(id: Value, error: &crate::types::ToolError) -> String {
    let Some(data) = error.error_data() else {
        return error_frame(id, error.code, &error.message);
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": error.code, "message": error.message, "data": data }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_result_carries_structured_content() {
        let payload = tool_ok(json!({ "ok": true }));
        assert_eq!(payload["structuredContent"]["ok"], true);
        assert!(payload["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"ok\": true"));
    }

    #[test]
    fn string_result_is_plain_text_without_structured_content() {
        let payload = tool_ok(Value::String("line one\nline two".to_string()));
        assert_eq!(payload["content"][0]["text"], "line one\nline two");
        assert_eq!(payload.get("structuredContent"), None);
        assert_eq!(payload["isError"], false);
    }

    #[test]
    fn modern_result_metadata_preserves_the_payload() {
        let frame = result_frame(json!(7), json!({ "tools": [] }));
        let modern: Value =
            serde_json::from_str(&modernize_result_frame(&frame, "todo", "1.2.3", true)).unwrap();
        assert_eq!(modern["result"]["resultType"], "complete");
        assert_eq!(modern["result"]["tools"], json!([]));
        assert_eq!(modern["result"]["ttlMs"], 0);
        assert_eq!(modern["result"]["cacheScope"], "private");
        assert_eq!(
            modern["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "todo"
        );
    }
}
