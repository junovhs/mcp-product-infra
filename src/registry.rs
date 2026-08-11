//! Tool registry and op-dispatch helpers.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/registry.rs`.
//! Ishoo-specific capabilities and handlers were removed; the registry shape,
//! mutation classification, and op-dispatch helper were retained.

use crate::types::{ExecutionPolicy, Handler, ToolSpec};
use serde_json::{json, Value};
use std::collections::HashSet;

/// A structural defect that can make registry metadata inert or misleading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryDefect {
    DuplicateToolName { tool: String },
    EmptyToolName { index: usize },
    EmptyDescription { tool: String },
    InputSchemaNotObject { tool: String },
    AnnotationsNotObject { tool: String },
    ReadOnlyMutationConflict { tool: String },
}

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

    /// Scheduling policy for one call. Unknown tools remain concurrent so the
    /// normal request path can return their protocol error without occupying a
    /// serialized worker.
    pub fn execution_policy(&self, name: &str, args: &Value) -> ExecutionPolicy {
        self.get(name)
            .map(|tool| tool.execution_policy(args))
            .unwrap_or(ExecutionPolicy::Concurrent)
    }

    /// Report every structural defect instead of stopping at the first one.
    /// Validation is observational: registration remains infallible and keeps
    /// its existing builder API.
    pub fn validate(&self) -> Result<(), Vec<RegistryDefect>> {
        let mut defects = Vec::new();
        let mut names = HashSet::new();

        for (index, tool) in self.tools.iter().enumerate() {
            if tool.name.trim().is_empty() {
                defects.push(RegistryDefect::EmptyToolName { index });
            } else if !names.insert(tool.name.as_str()) {
                defects.push(RegistryDefect::DuplicateToolName {
                    tool: tool.name.clone(),
                });
            }
            if tool.description.trim().is_empty() {
                defects.push(RegistryDefect::EmptyDescription {
                    tool: tool.name.clone(),
                });
            }
            if !tool.input_schema.is_object() {
                defects.push(RegistryDefect::InputSchemaNotObject {
                    tool: tool.name.clone(),
                });
            }

            match &tool.annotations {
                Some(Value::Object(annotations)) => {
                    let claims_read_only = annotations
                        .get("readOnlyHint")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if claims_read_only
                        && !matches!(tool.mutation, crate::types::MutationKind::Never)
                    {
                        defects.push(RegistryDefect::ReadOnlyMutationConflict {
                            tool: tool.name.clone(),
                        });
                    }
                }
                Some(_) => defects.push(RegistryDefect::AnnotationsNotObject {
                    tool: tool.name.clone(),
                }),
                None => {}
            }
        }

        if defects.is_empty() {
            Ok(())
        } else {
            Err(defects)
        }
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
                    "annotations": tool_annotations(tool),
                })
            })
            .collect();
        json!({ "tools": tools })
    }
}

/// MCP `tools/list` annotations for one tool: `readOnlyHint` derived from the
/// dispatch classification (`MutationKind::Never` → read-only; `Always` and
/// `Dynamic` advertise non-read-only, since a host hint cannot depend on the
/// arguments), then any `ToolSpec::annotations` overrides merged on top
/// (override keys win, including `readOnlyHint`).
fn tool_annotations(tool: &ToolSpec) -> Value {
    let read_only = matches!(tool.mutation, crate::types::MutationKind::Never);
    let mut annotations = serde_json::Map::new();
    annotations.insert("readOnlyHint".to_string(), Value::Bool(read_only));
    if let Some(Value::Object(overrides)) = &tool.annotations {
        for (key, value) in overrides {
            annotations.insert(key.clone(), value.clone());
        }
    }
    Value::Object(annotations)
}

/// Helper for op-dispatched tools. Reads `op`, strips it from the inner args, and
/// calls the matching handler.
pub fn dispatch_op(
    entity: &str,
    table: &[(&str, Handler)],
    ctx: &crate::types::ToolContext,
    args: &Value,
) -> crate::types::ToolResult {
    let op = args.get("op").and_then(Value::as_str).ok_or_else(|| {
        crate::types::ToolError::invalid_params(format!("{entity} requires an `op` field"))
            .with_kind(crate::types::kinds::INVALID_INPUT)
            .with_data(serde_json::json!({ "entity": entity, "missing_field": "op" }))
    })?;

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
    ))
    .with_kind(crate::types::kinds::INVALID_INPUT)
    .with_data(serde_json::json!({ "entity": entity, "op": op, "expected": known })))
}

pub fn op_is_read(args: &Value, read_ops: &[&str]) -> bool {
    args.get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| read_ops.contains(&op))
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::types::ToolSpec;

    fn read(name: &str) -> ToolSpec {
        ToolSpec::read(
            name,
            "Read a value",
            json!({ "type": "object", "properties": {} }),
            |_ctx, _args| Ok(json!({ "ok": true })),
        )
    }

    #[test]
    fn validation_reports_each_silent_registry_defect() {
        let registry = ToolRegistry::new()
            .with(read("duplicate"))
            .with(read("duplicate"))
            .with(ToolSpec::read(
                "bad_schema",
                "Bad schema",
                json!(["not", "an", "object"]),
                |_ctx, _args| Ok(json!({})),
            ))
            .with(
                ToolSpec::write(
                    "misleading_write",
                    "Write",
                    json!({ "type": "object" }),
                    |_ctx, _args| Ok(json!({})),
                )
                .with_annotations(json!({ "readOnlyHint": true })),
            );

        let defects = registry.validate().unwrap_err();
        assert!(defects.contains(&RegistryDefect::DuplicateToolName {
            tool: "duplicate".into()
        }));
        assert!(defects.contains(&RegistryDefect::InputSchemaNotObject {
            tool: "bad_schema".into()
        }));
        assert!(defects.contains(&RegistryDefect::ReadOnlyMutationConflict {
            tool: "misleading_write".into()
        }));
        assert_eq!(defects.len(), 3);
    }

    #[test]
    fn a_normal_registry_validates_cleanly() {
        assert_eq!(
            ToolRegistry::new()
                .with(read("status"))
                .with(ToolSpec::write(
                    "create",
                    "Create a value",
                    json!({ "type": "object", "properties": {} }),
                    |_ctx, _args| Ok(json!({ "created": true })),
                ))
                .validate(),
            Ok(())
        );
    }
}
