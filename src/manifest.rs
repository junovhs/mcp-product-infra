use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A structural defect in a manifest or its registry projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestDefect {
    EmptyManifestName,
    EmptyHandlerCommand,
    DuplicateToolName { tool: String },
    EmptyToolName { index: usize },
    EmptyDescription { tool: String },
    InputSchemaNotObject { tool: String },
    MissingRegistryTool { tool: String },
}

/// Language-agnostic app description for the future `mcp-product-infra serve --manifest` path.
///
/// The manifest mode is intentionally simple: Rust owns MCP framing and lifecycle;
/// the app-owned handler process owns behavior.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    pub handler: HandlerCommand,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
}

impl Manifest {
    /// Validate the language-neutral manifest shape without inspecting app
    /// source or executing its handler command.
    pub fn validate(&self) -> Result<(), Vec<ManifestDefect>> {
        let mut defects = Vec::new();
        let mut names = std::collections::HashSet::new();
        if self.name.trim().is_empty() {
            defects.push(ManifestDefect::EmptyManifestName);
        }
        if self.handler.command.trim().is_empty() {
            defects.push(ManifestDefect::EmptyHandlerCommand);
        }
        for (index, tool) in self.tools.iter().enumerate() {
            if tool.name.trim().is_empty() {
                defects.push(ManifestDefect::EmptyToolName { index });
            } else if !names.insert(tool.name.as_str()) {
                defects.push(ManifestDefect::DuplicateToolName {
                    tool: tool.name.clone(),
                });
            }
            if tool.description.trim().is_empty() {
                defects.push(ManifestDefect::EmptyDescription {
                    tool: tool.name.clone(),
                });
            }
            if !tool.input_schema.is_object() {
                defects.push(ManifestDefect::InputSchemaNotObject {
                    tool: tool.name.clone(),
                });
            }
        }
        finish_validation(defects)
    }

    /// Validate the manifest and confirm every declared tool resolves to a
    /// registered handler by the exact name advertised to MCP clients.
    pub fn validate_against_registry(
        &self,
        registry: &crate::registry::ToolRegistry,
    ) -> Result<(), Vec<ManifestDefect>> {
        let mut defects = self.validate().err().unwrap_or_default();
        for tool in &self.tools {
            if !tool.name.trim().is_empty() && registry.handler(&tool.name).is_none() {
                defects.push(ManifestDefect::MissingRegistryTool {
                    tool: tool.name.clone(),
                });
            }
        }
        finish_validation(defects)
    }
}

fn finish_validation(defects: Vec<ManifestDefect>) -> Result<(), Vec<ManifestDefect>> {
    if defects.is_empty() {
        Ok(())
    } else {
        Err(defects)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub mutation: ManifestMutation,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestMutation {
    #[default]
    Never,
    Always,
    Dynamic,
}

/// Wire request sent from the mcp-product-infra sidecar to a language-specific handler.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerRequest {
    pub tool: String,
    pub arguments: Value,
    pub workspace: String,
}

/// Wire response returned by a language-specific handler.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandlerResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
    /// DEC-04: stable machine-readable failure classification, so a handler in
    /// any language can say *what kind* of failure this is, not just its code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Structured failure details carried alongside the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl HandlerResponse {
    pub fn success(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            code: None,
            message: None,
            kind: None,
            data: None,
        }
    }

    pub fn error(code: i64, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            code: Some(code),
            message: Some(message.into()),
            kind: None,
            data: None,
        }
    }

    /// `error` plus its DEC-04 classification.
    pub fn error_kinded(code: i64, message: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            kind: Some(kind.into()),
            ..Self::error(code, message)
        }
    }

    /// Attach structured details to a failure response.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// The typed failure this response describes, or `None` when it is a success.
    pub fn as_tool_error(&self) -> Option<crate::types::ToolError> {
        if self.ok {
            return None;
        }
        Some(crate::types::ToolError {
            code: self.code.unwrap_or(crate::types::SERVER_ERROR),
            message: self
                .message
                .clone()
                .unwrap_or_else(|| "handler failed".to_string()),
            kind: self.kind.clone(),
            data: self.data.clone(),
        })
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use crate::types::ToolSpec;
    use serde_json::json;

    fn manifest(tools: Vec<ManifestTool>) -> Manifest {
        Manifest {
            name: "example".into(),
            version: Some("1.0.0".into()),
            instructions: None,
            handler: HandlerCommand {
                command: "example-handler".into(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
            },
            tools,
        }
    }

    fn tool(name: &str) -> ManifestTool {
        ManifestTool {
            name: name.into(),
            description: "A tool".into(),
            input_schema: json!({ "type": "object" }),
            mutation: ManifestMutation::Never,
        }
    }

    #[test]
    fn a_manifest_tool_without_a_registered_handler_is_reported() {
        let manifest = manifest(vec![tool("declared_only")]);
        let defects = manifest
            .validate_against_registry(&ToolRegistry::new())
            .unwrap_err();
        assert_eq!(
            defects,
            vec![ManifestDefect::MissingRegistryTool {
                tool: "declared_only".into()
            }]
        );
    }

    #[test]
    fn a_manifest_and_registry_with_the_same_surface_validate_cleanly() {
        let manifest = manifest(vec![tool("status")]);
        let registry = ToolRegistry::new().with(ToolSpec::read(
            "status",
            "Status",
            json!({ "type": "object" }),
            |_ctx, _args| Ok(json!({ "ok": true })),
        ));
        assert_eq!(manifest.validate_against_registry(&registry), Ok(()));
    }
}
