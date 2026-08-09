use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const SERVER_ERROR: i64 = -32000;
/// Generic counterpart to Ishoo's STOR-22 `STORE_SERVICE_UNAVAILABLE` code.
/// Returned when a mutating tool call cannot safely reach the resident owner.
pub const OWNER_SERVICE_UNAVAILABLE: i64 = -32010;

/// Best-effort human text from a caught panic payload (`&str` or `String`
/// payloads; anything else reads "unknown panic").
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Context passed to every tool handler.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub app_name: String,
    pub workspace_root: PathBuf,
}

impl ToolContext {
    pub fn new(app_name: impl Into<String>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            app_name: app_name.into(),
            workspace_root: workspace_root.into(),
        }
    }
}

/// Stable failure kinds the library itself raises (DEC-04). These are strings,
/// not an enum, precisely so an app can add its own vocabulary without waiting
/// for a release here — infrastructure never learns what an app's dependencies
/// are. Consumers branch on the kind; the JSON-RPC code stays a transport fact.
pub mod kinds {
    /// The caller's arguments are wrong: missing, malformed, or unknown.
    pub const INVALID_INPUT: &str = "invalid_input";
    /// The request is well-formed but the world is not in a state that allows it.
    pub const PRECONDITION: &str = "precondition";
    /// A deliberate, correct refusal by policy — not a malfunction.
    pub const POLICY_REFUSAL: &str = "policy_refusal";
    /// Something the call depends on is absent; it may be repairable.
    pub const DEPENDENCY_UNAVAILABLE: &str = "dependency_unavailable";
    /// A dependency exists but is behind the state it should describe.
    pub const DEPENDENCY_STALE: &str = "dependency_stale";
    /// The work is legitimate but the resource is occupied; retry later.
    pub const BUSY: &str = "busy";
    /// A bounded wait elapsed without an answer.
    pub const TIMEOUT: &str = "timeout";
    /// The resident owner is registered but not serving correctly.
    pub const OWNER_UNHEALTHY: &str = "owner_unhealthy";
    /// The channel to the owner failed before the request was delivered.
    pub const TRANSPORT_FAILURE: &str = "transport_failure";
    /// A defect in this process — a panic or a violated internal invariant.
    pub const INTERNAL: &str = "internal";
    /// The request may or may not have been applied; the caller must verify
    /// before retrying, because a retry could double-apply it.
    pub const OUTCOME_UNKNOWN: &str = "outcome_unknown";
}

/// A typed tool failure that becomes a JSON-RPC error.
///
/// `code` stays protocol vocabulary; `kind` carries the meaning a caller can
/// branch on, and `data` carries structured details (DEC-04). Both are optional
/// and additive: an error with neither serializes exactly as it did before they
/// existed, so untagged app errors are unchanged on the wire.
#[derive(Clone, Debug)]
pub struct ToolError {
    pub code: i64,
    pub message: String,
    /// Stable machine-readable classification; see [`kinds`].
    pub kind: Option<String>,
    /// Structured details for the caller, merged into the JSON-RPC `data` member.
    pub data: Option<Value>,
}

impl ToolError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            kind: None,
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(SERVER_ERROR, message)
    }

    /// Tag this failure with a stable kind (one of [`kinds`], or an app's own).
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Attach structured details the caller can read without parsing prose.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// The JSON-RPC `data` member for this failure, or `None` when it carries
    /// neither a kind nor details — which keeps an untagged error byte-identical
    /// to what it serialized to before kinds existed.
    pub fn error_data(&self) -> Option<Value> {
        match (&self.kind, &self.data) {
            (None, None) => None,
            (kind, data) => {
                let mut object = serde_json::Map::new();
                match data {
                    // An object payload merges in flat, so details sit beside
                    // `kind` instead of nesting a redundant wrapper.
                    Some(Value::Object(fields)) => {
                        for (key, value) in fields {
                            object.insert(key.clone(), value.clone());
                        }
                    }
                    Some(other) => {
                        object.insert("details".to_string(), other.clone());
                    }
                    None => {}
                }
                // `kind` is reserved: stamped last so app details can never
                // shadow the classification a caller branches on.
                if let Some(kind) = kind {
                    object.insert("kind".to_string(), Value::String(kind.clone()));
                }
                Some(Value::Object(object))
            }
        }
    }
}

pub type ToolResult = Result<Value, ToolError>;
pub type Handler = Arc<dyn Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static>;
pub type MutationClassifier = Arc<dyn Fn(&Value) -> bool + Send + Sync + 'static>;

/// How the server should classify a tool call for dispatch.
#[derive(Clone)]
pub enum MutationKind {
    Never,
    Always,
    Dynamic(MutationClassifier),
}

impl MutationKind {
    pub fn mutates(&self, args: &Value) -> bool {
        match self {
            MutationKind::Never => false,
            MutationKind::Always => true,
            MutationKind::Dynamic(classifier) => classifier(args),
        }
    }
}

/// A single MCP tool exposed by the app.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub mutation: MutationKind,
    pub handler: Handler,
    /// Extra `tools/list` annotation members merged over the derived set
    /// (override keys win). `readOnlyHint` is always derived from `mutation`
    /// (`Never` → true), so most tools never set this; use it for hints the
    /// dispatch classification cannot know, e.g. `destructiveHint`,
    /// `idempotentHint`, `openWorldHint`, or a display `title`.
    pub annotations: Option<Value>,
}

impl ToolSpec {
    /// Merge extra `tools/list` annotation members over the derived set
    /// (override keys win). See the `annotations` field.
    pub fn with_annotations(mut self, annotations: Value) -> Self {
        self.annotations = Some(annotations);
        self
    }

    pub fn read(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Never,
            handler: Arc::new(handler),
            annotations: None,
        }
    }

    pub fn write(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Always,
            handler: Arc::new(handler),
            annotations: None,
        }
    }

    pub fn dynamic(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mutates: impl Fn(&Value) -> bool + Send + Sync + 'static,
        handler: impl Fn(&ToolContext, &Value) -> ToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            mutation: MutationKind::Dynamic(Arc::new(mutates)),
            handler: Arc::new(handler),
            annotations: None,
        }
    }
}

#[cfg(test)]
mod error_data_tests {
    use super::*;
    use serde_json::json;

    /// ERR-01 review: `kind` is the member every caller branches on, so app
    /// details must never be able to shadow it.
    #[test]
    fn structured_details_cannot_overwrite_the_reserved_kind_member() {
        let error = ToolError::server("boom")
            .with_kind(kinds::INTERNAL)
            .with_data(json!({ "kind": "invalid_input", "attempt": 2 }));
        let data = error.error_data().expect("a tagged error carries data");

        assert_eq!(data["kind"], kinds::INTERNAL);
        assert_eq!(data["attempt"], 2);
    }

    /// A non-object payload is nested rather than dropped or splatted.
    #[test]
    fn a_non_object_detail_payload_is_nested_under_details() {
        let error = ToolError::server("boom")
            .with_kind(kinds::BUSY)
            .with_data(json!(["a", "b"]));
        let data = error.error_data().unwrap();

        assert_eq!(data["kind"], kinds::BUSY);
        assert_eq!(data["details"], json!(["a", "b"]));
    }

    /// The non-breaking guarantee at its source: no kind, no details, no data.
    #[test]
    fn an_untagged_error_has_no_data_member() {
        assert!(ToolError::invalid_params("nope").error_data().is_none());
    }
}
