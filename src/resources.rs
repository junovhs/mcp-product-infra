//! MCP resources: app artifacts a host can read or attach as context (e.g. an
//! @-mention) without a tool call.
//!
//! Mined from semmap's `src/mcp/resources.rs`, generalized: the app supplies a
//! [`ResourceProvider`] that enumerates the artifacts that currently exist, and
//! `resources/read` serves **only** a URI the provider enumerated — there is no
//! arbitrary-path read. The provider runs per request, so artifacts may appear
//! and disappear between calls without the server holding stale state.

use crate::types::ToolContext;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Enumerate the resources that currently exist. Called on every
/// `resources/list` and `resources/read`.
pub type ResourceProvider = Arc<dyn Fn(&ToolContext) -> Vec<ResourceEntry> + Send + Sync + 'static>;

/// Where a resource's text lives.
#[derive(Clone, Debug)]
pub enum ResourceContent {
    /// Read the file at serve time (missing/unreadable file → invalid-params).
    File(PathBuf),
    /// Serve this text directly.
    Inline(String),
}

/// One exposed artifact: its URI plus the metadata `resources/list` reports.
#[derive(Clone, Debug)]
pub struct ResourceEntry {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
    pub content: ResourceContent,
}

impl ResourceEntry {
    /// A file-backed entry with a `file://` URI derived from the path
    /// (best-effort; non-UTF-8 paths are lossy).
    pub fn file(
        path: impl Into<PathBuf>,
        name: impl Into<String>,
        description: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Self {
        let path = path.into();
        Self {
            uri: file_uri(&path),
            name: name.into(),
            description: description.into(),
            mime_type: mime_type.into(),
            content: ResourceContent::File(path),
        }
    }
}

/// A `file://` URI for a path (best-effort; non-UTF-8 paths are lossy).
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

/// The `resources/list` result.
pub fn list(provider: &ResourceProvider, ctx: &ToolContext) -> Value {
    let resources: Vec<Value> = provider(ctx)
        .into_iter()
        .map(|entry| {
            json!({
                "uri": entry.uri,
                "name": entry.name,
                "description": entry.description,
                "mimeType": entry.mime_type,
            })
        })
        .collect();
    json!({ "resources": resources })
}

/// The `resources/read` result for `uri`, or an error message. Only a URI the
/// provider enumerates is served, so the server never reads an arbitrary path.
pub fn read(provider: &ResourceProvider, ctx: &ToolContext, uri: &str) -> Result<Value, String> {
    let entry = provider(ctx)
        .into_iter()
        .find(|entry| entry.uri == uri)
        .ok_or_else(|| format!("unknown resource uri: {uri}"))?;
    let text = match &entry.content {
        ResourceContent::File(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
        ResourceContent::Inline(text) => text.clone(),
    };
    Ok(json!({
        "contents": [ {
            "uri": entry.uri,
            "mimeType": entry.mime_type,
            "text": text,
        } ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(dir: &Path) -> ResourceProvider {
        let map = dir.join("MAP.md");
        Arc::new(move |_ctx| {
            let mut entries = Vec::new();
            if map.is_file() {
                entries.push(ResourceEntry::file(
                    map.clone(),
                    "MAP.md",
                    "The rendered map.",
                    "text/markdown",
                ));
            }
            entries.push(ResourceEntry {
                uri: "app://brief".to_string(),
                name: "brief".to_string(),
                description: "The agent protocol.".to_string(),
                mime_type: "text/plain".to_string(),
                content: ResourceContent::Inline("be excellent".to_string()),
            });
            entries
        })
    }

    fn ctx() -> ToolContext {
        ToolContext::new("app", ".")
    }

    #[test]
    fn list_enumerates_only_existing_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(dir.path());

        let listed = list(&provider, &ctx());
        let resources = listed["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 1, "file absent: only the inline entry");

        std::fs::write(dir.path().join("MAP.md"), "# map").unwrap();
        let listed = list(&provider, &ctx());
        let resources = listed["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2, "file present: both entries");
        assert_eq!(resources[0]["mimeType"], "text/markdown");
    }

    #[test]
    fn read_serves_only_enumerated_uris() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MAP.md"), "# map").unwrap();
        let provider = provider(dir.path());

        let uri = file_uri(&dir.path().join("MAP.md"));
        let value = read(&provider, &ctx(), &uri).unwrap();
        assert_eq!(value["contents"][0]["text"], "# map");

        let value = read(&provider, &ctx(), "app://brief").unwrap();
        assert_eq!(value["contents"][0]["text"], "be excellent");

        let err = read(&provider, &ctx(), "file:///etc/passwd").unwrap_err();
        assert!(err.contains("unknown resource uri"), "{err}");
    }
}
