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

/// A typed tool failure that becomes a JSON-RPC error.
#[derive(Clone, Debug)]
pub struct ToolError {
    pub code: i64,
    pub message: String,
}

impl ToolError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(SERVER_ERROR, message)
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
