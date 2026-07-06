//! Stdio MCP server runtime.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/mod.rs`.
//! This module keeps Ishoo's small hand-written JSON-RPC loop, read/write
//! dispatch split, shutdown drain, and parent-watchdog shape, with Ishoo product
//! hooks replaced by app-provided configuration.

use crate::registry::ToolRegistry;
use crate::response::{error_frame, result_frame, tool_ok};
use crate::types::{
    ToolContext, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_SHUTDOWN_DRAIN: Duration = Duration::from_secs(120);

pub type MutationHook =
    Arc<dyn Fn(&ToolContext, &str, &mut Value) -> Result<(), String> + Send + Sync + 'static>;

#[derive(Clone)]
pub struct ServerConfig {
    pub app_name: String,
    pub version: String,
    pub instructions: Option<String>,
    pub context: ToolContext,
    pub registry: ToolRegistry,
    pub mutation_hook: Option<MutationHook>,
}

impl ServerConfig {
    pub fn new(
        app_name: impl Into<String>,
        version: impl Into<String>,
        workspace_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        let app_name = app_name.into();
        Self {
            context: ToolContext::new(app_name.clone(), workspace_root),
            app_name,
            version: version.into(),
            instructions: None,
            registry: ToolRegistry::new(),
            mutation_hook: None,
        }
    }

    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn tool(mut self, tool: crate::types::ToolSpec) -> Self {
        self.registry.add(tool);
        self
    }

    pub fn mutation_hook(
        mut self,
        hook: impl Fn(&ToolContext, &str, &mut Value) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.mutation_hook = Some(Arc::new(hook));
        self
    }
}

#[derive(Clone)]
pub struct McpServer {
    config: Arc<ServerConfig>,
}

impl McpServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Run a newline-delimited stdio JSON-RPC MCP server until stdin closes or the
    /// parent process disappears.
    pub fn run_stdio(self) -> i32 {
        let (events_tx, events_rx) = mpsc::channel();
        spawn_stdin_reader(events_tx.clone());
        spawn_parent_watchdog(events_tx.clone());

        let dispatch = Dispatch::new(self.clone(), events_tx);
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let mut active_requests = 0usize;
        let mut input_closed = false;
        let mut shutdown_deadline: Option<Instant> = None;

        loop {
            let event = match shutdown_deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    match events_rx.recv_timeout(deadline.saturating_duration_since(now)) {
                        Ok(event) => event,
                        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match events_rx.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };

            match event {
                ServerEvent::Line(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    active_requests += 1;
                    dispatch.dispatch(line);
                }
                ServerEvent::InputClosed => {
                    input_closed = true;
                    if active_requests == 0 {
                        break;
                    }
                    shutdown_deadline = Some(Instant::now() + shutdown_drain());
                }
                ServerEvent::ParentGone => break,
                ServerEvent::Completed(response) => {
                    active_requests = active_requests.saturating_sub(1);
                    if let Some(response) = response {
                        if writeln!(out, "{response}").is_err() || out.flush().is_err() {
                            break;
                        }
                    }
                    if input_closed && active_requests == 0 {
                        break;
                    }
                }
            }
        }
        0
    }

    /// Handle one JSON-RPC frame. Useful for sidecar owner servers and tests.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let message: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return Some(error_frame(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("Parse error: {error}"),
                ))
            }
        };

        let is_request = message.get("id").is_some();
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return is_request.then(|| {
                    error_frame(id, INVALID_REQUEST, "Invalid Request: missing method")
                })
            }
        };

        match method {
            "initialize" => Some(result_frame(id, self.initialize_result(&message))),
            "tools/list" => Some(result_frame(id, self.config.registry.tools_list_result())),
            "tools/call" => Some(self.tools_call(id, &message)),
            "ping" => Some(result_frame(id, json!({}))),
            _ if method.starts_with("notifications/") => None,
            _ => is_request.then(|| {
                error_frame(id, METHOD_NOT_FOUND, &format!("Method not found: {method}"))
            }),
        }
    }

    fn initialize_result(&self, message: &Value) -> Value {
        let protocol_version = message
            .get("params")
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);

        let mut result = json!({
            "protocolVersion": protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": self.config.app_name,
                "version": self.config.version,
            }
        });
        if let Some(instructions) = &self.config.instructions {
            result["instructions"] = Value::String(instructions.clone());
        }
        result
    }

    fn tools_call(&self, id: Value, message: &Value) -> String {
        let params = message.get("params");
        let name = match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
            Some(name) => name,
            None => return error_frame(id, INVALID_PARAMS, "Missing tool name in params"),
        };
        let tool = match self.config.registry.get(name) {
            Some(tool) => tool.clone(),
            None => return error_frame(id, INVALID_PARAMS, &format!("Unknown tool: {name}")),
        };
        let args = params
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({}));

        match (tool.handler)(&self.config.context, &args) {
            Ok(mut value) => {
                if tool.mutation.mutates(&args) {
                    if let Some(hook) = &self.config.mutation_hook {
                        if let Err(error) = hook(&self.config.context, name, &mut value) {
                            attach_mutation_warning(&mut value, error);
                        }
                    }
                }
                result_frame(id, tool_ok(value))
            }
            Err(error) => error_frame(id, error.code, &error.message),
        }
    }

    fn line_calls_mutating_tool(&self, line: &str) -> bool {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        if message.get("method").and_then(Value::as_str) != Some("tools/call") {
            return false;
        }
        let Some(name) = message
            .get("params")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        let args = message
            .get("params")
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or(Value::Null);
        self.config.registry.mutates(name, &args)
    }
}

fn attach_mutation_warning(value: &mut Value, warning: String) {
    match value {
        Value::Object(map) => {
            map.insert(
                "mutation_warning".to_string(),
                json!({
                    "state": "failed_after_success",
                    "reason": warning,
                    "recovery": "The tool handler completed. Do not blindly retry unless your app-level result says the operation is idempotent."
                }),
            );
        }
        other => {
            let original = other.take();
            *other = json!({
                "value": original,
                "mutation_warning": {
                    "state": "failed_after_success",
                    "reason": warning
                }
            });
        }
    }
}

#[derive(Debug)]
enum ServerEvent {
    Line(String),
    InputClosed,
    ParentGone,
    Completed(Option<String>),
}

struct Dispatch {
    server: McpServer,
    events_tx: mpsc::Sender<ServerEvent>,
    mutations_tx: mpsc::Sender<String>,
}

impl Dispatch {
    fn new(server: McpServer, events_tx: mpsc::Sender<ServerEvent>) -> Self {
        let (mutations_tx, mutations_rx) = mpsc::channel::<String>();
        let worker_server = server.clone();
        let worker_events = events_tx.clone();
        thread::spawn(move || {
            for line in mutations_rx {
                let response = worker_server.handle_line(&line);
                let _ = worker_events.send(ServerEvent::Completed(response));
            }
        });
        Self {
            server,
            events_tx,
            mutations_tx,
        }
    }

    fn dispatch(&self, line: String) {
        if self.server.line_calls_mutating_tool(&line) {
            let _ = self.mutations_tx.send(line);
        } else {
            let server = self.server.clone();
            let tx = self.events_tx.clone();
            thread::spawn(move || {
                let response = server.handle_line(&line);
                let _ = tx.send(ServerEvent::Completed(response));
            });
        }
    }
}

fn spawn_stdin_reader(tx: mpsc::Sender<ServerEvent>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(ServerEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(ServerEvent::InputClosed);
    });
}

fn shutdown_drain() -> Duration {
    std::env::var("TURNKEY_MCP_SHUTDOWN_DRAIN_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_SHUTDOWN_DRAIN)
}

#[cfg(unix)]
fn spawn_parent_watchdog(tx: mpsc::Sender<ServerEvent>) {
    let original_parent = unsafe { libc::getppid() };
    thread::spawn(move || loop {
        let current_parent = unsafe { libc::getppid() };
        if current_parent <= 1 || current_parent != original_parent {
            let _ = tx.send(ServerEvent::ParentGone);
            break;
        }
        thread::sleep(Duration::from_secs(1));
    });
}

#[cfg(not(unix))]
fn spawn_parent_watchdog(_tx: mpsc::Sender<ServerEvent>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolSpec;
    use serde_json::json;

    #[test]
    fn initialize_echoes_protocol_and_lists_tools() {
        let server = McpServer::new(
            ServerConfig::new("todo", "0.1.0", ".")
                .instructions("Use todo_* tools.")
                .tool(ToolSpec::read(
                    "todo_status",
                    "Return status",
                    json!({ "type": "object", "properties": {} }),
                    |_ctx, _args| Ok(json!({ "ok": true })),
                )),
        );
        let init = server.handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        ).unwrap();
        let init: Value = serde_json::from_str(&init).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(init["result"]["serverInfo"]["name"], "todo");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("todo_"));

        let tools = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap();
        let tools: Value = serde_json::from_str(&tools).unwrap();
        assert_eq!(tools["result"]["tools"][0]["name"], "todo_status");
    }

    #[test]
    fn tool_call_returns_structured_content() {
        let server = McpServer::new(ServerConfig::new("todo", "0.1.0", ".").tool(
            ToolSpec::read(
                "todo_status",
                "Return status",
                json!({ "type": "object", "properties": {} }),
                |_ctx, _args| Ok(json!({ "ok": true })),
            ),
        ));
        let raw = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"todo_status","arguments":{}}}"#)
            .unwrap();
        let response: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["structuredContent"]["ok"], true);
    }
}
