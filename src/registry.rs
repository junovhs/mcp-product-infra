//! Tool registry and op-dispatch helpers.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/registry.rs`.
//! Ishoo-specific capabilities and handlers were removed; the registry shape,
//! mutation classification, and op-dispatch helper were retained.

use crate::types::{Handler, ToolSpec};
use serde_json::{json, Value};

/// An ordered registry of tools. `tools/list` is rendered from this registry and
/// `tools/call` dispatches through it.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<ToolSpec>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn with(mut self, tool: ToolSpec) -> Self {
        self.add(tool);
        self
    }

    pub fn add(&mut self, tool: ToolSpec) {
        self.tools.push(tool);
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.iter()
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub fn handler(&self, name: &str) -> Option<Handler> {
        self.get(name).map(|tool| tool.handler.clone())
    }

    pub fn mutates(&self, name: &str, args: &Value) -> bool {
        self.get(name)
            .is_some_and(|tool| tool.mutation.mutates(args))
    }

    pub fn tools_list_result(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })
            })
            .collect();
        json!({ "tools": tools })
    }
}

/// Helper for op-dispatched tools. Reads `op`, strips it from the inner args, and
/// calls the matching handler.
pub fn dispatch_op(
    entity: &str,
    table: &[(&str, Handler)],
    ctx: &crate::types::ToolContext,
    args: &Value,
) -> crate::types::ToolResult {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| crate::types::ToolError::invalid_params(format!("{entity} requires an `op` field")))?;

    for (name, handler) in table {
        if *name == op {
            let inner = match args {
                Value::Object(map) => {
                    let mut map = map.clone();
                    map.remove("op");
                    Value::Object(map)
                }
                other => other.clone(),
            };
            return handler(ctx, &inner);
        }
    }

    let known: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
    Err(crate::types::ToolError::invalid_params(format!(
        "{entity}: unknown op '{op}'; expected one of {}",
        known.join("/")
    )))
}

pub fn op_is_read(args: &Value, read_ops: &[&str]) -> bool {
    args.get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| read_ops.contains(&op))
}
