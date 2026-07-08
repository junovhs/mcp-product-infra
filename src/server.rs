//! Stdio MCP server runtime.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/mod.rs`.
//! This module preserves Ishoo's important runtime shape:
//! - hand-written newline-delimited stdio JSON-RPC loop
//! - `initialize`, `tools/list`, `tools/call`, `ping`, and notifications handling
//! - structured tool responses (`content` plus `structuredContent`)
//! - read/write dispatch split: reads run concurrently, mutations are FIFO-serialized
//! - optional resident-owner routing with fail-closed mutation recovery
//! - shutdown drain and parent-death watchdogs

use crate::registry::ToolRegistry;
use crate::response::{error_frame, result_frame, tool_ok};
use crate::sidecar::{self, OwnerEndpoint, OwnerRecovery, SidecarConfig};
use crate::types::{
    ToolContext, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, OWNER_SERVICE_UNAVAILABLE,
    PARSE_ERROR,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_SHUTDOWN_DRAIN: Duration = Duration::from_secs(120);
#[cfg(unix)]
const DEFAULT_PARENT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

pub type MutationHook =
    Arc<dyn Fn(&ToolContext, &str, &mut Value) -> Result<(), String> + Send + Sync + 'static>;

/// The prose (and JSON-RPC code) of the fail-closed owner-unavailable errors,
/// so an app's agents see the app's own vocabulary ("store service", "resident
/// store owner", the app's exact restart command) instead of generic library
/// wording. The message SHAPES are fixed — only the nouns and the remedy vary —
/// so hosts and tests can rely on the structure.
#[derive(Clone, Debug)]
pub struct OwnerProse {
    /// JSON-RPC error code for a refused mutation (server-error range).
    pub code: i64,
    /// Leading clause, e.g. "store service unavailable — write refused; no
    /// changes were made."
    pub prefix: String,
    /// What the owner is called, e.g. "resident store owner".
    pub owner_noun: String,
    /// The remedy clause without the trailing "and retry.", e.g.
    /// "Restart MyApp (or run `myapp owner`)".
    pub restart_hint: String,
}

impl Default for OwnerProse {
    fn default() -> Self {
        Self {
            code: OWNER_SERVICE_UNAVAILABLE,
            prefix: "resident owner unavailable — mutation refused; no changes were made."
                .to_string(),
            owner_noun: "resident owner".to_string(),
            restart_hint: "Restart the app owner process".to_string(),
        }
    }
}

impl OwnerProse {
    fn live_but_unreachable(&self) -> String {
        format!(
            "{} The {} is running but did not respond in time. Retry in a moment.",
            self.prefix, self.owner_noun
        )
    }

    fn reelected_unreachable(&self, error: &str) -> String {
        format!(
            "{} A fresh {} was elected but could not be reached ({error}). {} and retry.",
            self.prefix, self.owner_noun, self.restart_hint
        )
    }

    fn down(&self, error: &str) -> String {
        format!(
            "{} The {} is down and could not be re-elected ({error}). {} and retry.",
            self.prefix, self.owner_noun, self.restart_hint
        )
    }
}

#[derive(Clone)]
pub struct ServerConfig {
    pub app_name: String,
    pub version: String,
    pub instructions: Option<String>,
    pub context: ToolContext,
    pub registry: ToolRegistry,
    pub mutation_hook: Option<MutationHook>,
    /// When present, the stdio MCP process routes tool calls through the resident
    /// owner just like Ishoo. The owner process itself should build its
    /// `McpServer` without this field set, then pass `server.handle_line` to
    /// `sidecar::run_owner_server`.
    pub sidecar: Option<SidecarConfig>,
    /// When false, a failed owner election at startup runs the server WITHOUT
    /// an owner instead of exiting — the transport still initializes and
    /// advertises tools so the agent can explain the setup path (e.g. a
    /// user-scope registration opened in a brand-new repo with no app state
    /// yet). Keep true (the default) when app state exists, because writes
    /// require the resident single owner.
    pub sidecar_required: bool,
    /// The vocabulary of the fail-closed owner-unavailable errors.
    pub owner_prose: OwnerProse,
    /// Which read tools get the `mcp_owner: unreachable` annotation when a read
    /// degrades to in-process handling because the owner did not answer.
    /// `None` = annotate every degraded read; `Some(names)` = only these tools
    /// (e.g. only the app's status/orientation tool).
    pub annotate_degraded_reads: Option<Vec<String>>,
    /// The `source` string inside the `mcp_owner` annotation.
    pub read_annotation_source: String,
    /// Env-var prefix for runtime overrides: `{PREFIX}_MCP_SHUTDOWN_DRAIN_MS`
    /// and `{PREFIX}_MCP_PARENT_WATCHDOG_MS`. Defaults to the app name
    /// uppercased with non-alphanumerics folded to `_`.
    pub env_prefix: String,
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
            env_prefix: default_env_prefix(&app_name),
            app_name,
            version: version.into(),
            instructions: None,
            registry: ToolRegistry::new(),
            mutation_hook: None,
            sidecar: None,
            sidecar_required: true,
            owner_prose: OwnerProse::default(),
            annotate_degraded_reads: None,
            read_annotation_source: "turnkey_mcp_sidecar".to_string(),
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

    /// Route MCP tool calls through a resident owner process. This is the
    /// zero-regression extraction of Ishoo's `mcp` -> `mcp-owner` split.
    pub fn sidecar(mut self, config: SidecarConfig) -> Self {
        self.sidecar = Some(config);
        self
    }

    /// Tolerate a failed owner election at startup (run ownerless) instead of
    /// exiting. See `sidecar_required`.
    pub fn sidecar_optional(mut self) -> Self {
        self.sidecar_required = false;
        self
    }

    pub fn owner_prose(mut self, prose: OwnerProse) -> Self {
        self.owner_prose = prose;
        self
    }

    /// Annotate only these read tools when a read degrades past an unreachable
    /// owner (default: all degraded reads are annotated).
    pub fn annotate_degraded_reads(
        mut self,
        tools: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.annotate_degraded_reads = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn read_annotation_source(mut self, source: impl Into<String>) -> Self {
        self.read_annotation_source = source.into();
        self
    }

    pub fn env_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.env_prefix = prefix.into();
        self
    }
}

fn default_env_prefix(app_name: &str) -> String {
    let folded: String = app_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if folded.is_empty() {
        "TURNKEY".to_string()
    } else {
        folded
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
        let owner = match &self.config.sidecar {
            Some(config) => match sidecar::ensure_owner_process(config) {
                Ok(endpoint) => Some(endpoint),
                Err(error) if !self.config.sidecar_required => {
                    // Fail open by explicit opt-in only: the transport still
                    // initializes and advertises tools so the agent can explain
                    // the setup path; tool calls run in-process.
                    eprintln!(
                        "{} mcp: running without a resident owner ({error})",
                        self.config.app_name
                    );
                    None
                }
                Err(error) => {
                    eprintln!("{} mcp error: {error}", self.config.app_name);
                    return 1;
                }
            },
            None => None,
        };

        let (events_tx, events_rx) = mpsc::channel();
        spawn_stdin_reader(events_tx.clone());
        spawn_parent_watchdog(&self.config.env_prefix, events_tx.clone());

        let dispatch = Dispatch::new(self.clone(), events_tx, owner);
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
                        Err(
                            mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected,
                        ) => break,
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
                    shutdown_deadline =
                        Some(Instant::now() + shutdown_drain(&self.config.env_prefix));
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

    /// Handle one JSON-RPC frame, routing tool calls through `owner` when
    /// present, with the full fail-closed mutation recovery. Exposed for
    /// app-level owner-recovery tests and custom transports; `run_stdio` uses
    /// exactly this path.
    pub fn handle_line_with_owner(
        &self,
        line: &str,
        owner: Option<&OwnerEndpoint>,
    ) -> Option<String> {
        self.handle_line_maybe_remote(line, owner)
    }

    /// Handle one JSON-RPC frame in-process. This is what a resident owner should
    /// call after receiving an owner request.
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
                return is_request
                    .then(|| error_frame(id, INVALID_REQUEST, "Invalid Request: missing method"))
            }
        };

        match method {
            "initialize" => Some(result_frame(id, self.initialize_result(&message))),
            "tools/list" => Some(result_frame(id, self.config.registry.tools_list_result())),
            "tools/call" => Some(self.tools_call(id, &message)),
            "ping" => Some(result_frame(id, json!({}))),
            _ if method.starts_with("notifications/") => None,
            _ => is_request
                .then(|| error_frame(id, METHOD_NOT_FOUND, &format!("Method not found: {method}"))),
        }
    }

    fn handle_line_maybe_remote(
        &self,
        line: &str,
        owner: Option<&OwnerEndpoint>,
    ) -> Option<String> {
        let Some(owner) = owner.filter(|_| line_calls_tool(line)) else {
            return self.handle_line(line);
        };
        let Some(sidecar_config) = self.config.sidecar.as_ref() else {
            return self.handle_line(line);
        };

        let prose = &self.config.owner_prose;
        match sidecar::send_line(owner, line) {
            Ok(response) => response,
            // The resident owner is unreachable. A mutation must NEVER fall back to
            // an in-process write while a *live* owner exists (the second-writer
            // hole). But a *dead* owner must not wedge every write forever, and we
            // must never claim a recovery that did not happen. So re-elect: only
            // when the owner is truly gone is the stale registration cleared and a
            // fresh resident writer spawned, then the write is retried once. If
            // re-election or the retry fails, refuse with an honest, actionable
            // remedy — never a false "restarts automatically".
            Err(_) if self.line_calls_mutating_tool(line) => {
                match sidecar::recover_owner(sidecar_config, owner) {
                    OwnerRecovery::Reelected(fresh) => match sidecar::send_line(&fresh, line) {
                        Ok(response) => response,
                        Err(error) => Some(error_frame(
                            request_id(line),
                            prose.code,
                            &prose.reelected_unreachable(&error),
                        )),
                    },
                    OwnerRecovery::LiveButUnreachable => Some(error_frame(
                        request_id(line),
                        prose.code,
                        &prose.live_but_unreachable(),
                    )),
                    OwnerRecovery::Down(error) => Some(error_frame(
                        request_id(line),
                        prose.code,
                        &prose.down(&error),
                    )),
                }
            }
            // A read MAY degrade gracefully: fall through to an in-process read,
            // which cannot corrupt state — orientation still works when the owner
            // is momentarily unreachable, while writes stay strictly fail-closed.
            // The degraded read is annotated so the agent sees the transport fact.
            Err(error) => {
                let annotate = match &self.config.annotate_degraded_reads {
                    None => true,
                    Some(tools) => tools.iter().any(|tool| line_calls_tool_named(line, tool)),
                };
                if annotate {
                    annotate_read_owner_unreachable(
                        self.handle_line(line),
                        &error,
                        &self.config.read_annotation_source,
                    )
                } else {
                    self.handle_line(line)
                }
            }
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
                "name": self.config.app_name.clone(),
                "version": self.config.version.clone(),
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

fn request_id(line: &str) -> Value {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| message.get("id").cloned())
        .unwrap_or(Value::Null)
}

fn line_calls_tool(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| {
            message
                .get("method")
                .and_then(Value::as_str)
                .map(|method| method == "tools/call")
        })
        .unwrap_or(false)
}

fn line_calls_tool_named(line: &str, expected: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|message| {
            if message.get("method").and_then(Value::as_str) != Some("tools/call") {
                return None;
            }
            message
                .get("params")
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str)
                .map(|name| name == expected)
        })
        .unwrap_or(false)
}

fn annotate_read_owner_unreachable(
    response: Option<String>,
    error: &str,
    source: &str,
) -> Option<String> {
    let raw = response?;
    let Ok(mut frame) = serde_json::from_str::<Value>(&raw) else {
        return Some(raw);
    };
    let Some(structured) = frame
        .get_mut("result")
        .and_then(|result| result.get_mut("structuredContent"))
        .and_then(Value::as_object_mut)
    else {
        return Some(raw);
    };

    structured.insert(
        "mcp_owner".to_string(),
        json!({
            "state": "unreachable",
            "source": source,
            "error": error,
            "write_behavior": "fail_closed_or_reattach",
            "system_action": "next_mutation_recovers_if_owner_is_dead"
        }),
    );
    let structured_text = serde_json::to_string_pretty(&Value::Object(structured.clone())).ok();
    if let Some(content) = frame
        .get_mut("result")
        .and_then(|result| result.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|items| items.first_mut())
        .and_then(Value::as_object_mut)
    {
        if let Some(text) = structured_text {
            content.insert("text".to_string(), Value::String(text));
        }
    }
    Some(frame.to_string())
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

/// Runtime events of the stdio loop. `#[doc(hidden)]`-public so apps porting
/// their MCP regression suites can drive `Dispatch` directly and assert on the
/// completions it emits; not part of the stability contract.
#[doc(hidden)]
#[derive(Debug)]
pub enum ServerEvent {
    Line(String),
    InputClosed,
    ParentGone,
    Completed(Option<String>),
}

/// Routes each request to the path that keeps the control surface correct:
/// mutating tool calls go to a single ordered worker so they execute in strict
/// arrival order and pipelined dependent mutations can never reorder; read-only
/// calls are spawned concurrently so a slow mutation never wedges reads.
/// `#[doc(hidden)]`-public for app regression suites.
#[doc(hidden)]
pub struct Dispatch {
    server: McpServer,
    events_tx: mpsc::Sender<ServerEvent>,
    mutations_tx: mpsc::Sender<String>,
    owner: Option<OwnerEndpoint>,
}

impl Dispatch {
    pub fn new(
        server: McpServer,
        events_tx: mpsc::Sender<ServerEvent>,
        owner: Option<OwnerEndpoint>,
    ) -> Self {
        let (mutations_tx, mutations_rx) = mpsc::channel::<String>();
        let worker_server = server.clone();
        let worker_events = events_tx.clone();
        let worker_owner = owner.clone();
        thread::spawn(move || {
            for line in mutations_rx {
                let response = worker_server.handle_line_maybe_remote(&line, worker_owner.as_ref());
                let _ = worker_events.send(ServerEvent::Completed(response));
            }
        });
        Self {
            server,
            events_tx,
            mutations_tx,
            owner,
        }
    }

    pub fn dispatch(&self, line: String) {
        if self.server.line_calls_mutating_tool(&line) {
            let _ = self.mutations_tx.send(line);
        } else {
            let server = self.server.clone();
            let tx = self.events_tx.clone();
            let owner = self.owner.clone();
            thread::spawn(move || {
                let response = server.handle_line_maybe_remote(&line, owner.as_ref());
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

#[doc(hidden)]
pub fn shutdown_drain(env_prefix: &str) -> Duration {
    std::env::var(format!("{env_prefix}_MCP_SHUTDOWN_DRAIN_MS"))
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_SHUTDOWN_DRAIN)
}

#[cfg(unix)]
fn parent_watchdog_interval(env_prefix: &str) -> Duration {
    std::env::var(format!("{env_prefix}_MCP_PARENT_WATCHDOG_MS"))
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PARENT_WATCHDOG_INTERVAL)
}

#[cfg(unix)]
fn current_parent_pid() -> libc::pid_t {
    unsafe { libc::getppid() }
}

#[cfg(target_os = "linux")]
fn arm_parent_death_signal() {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn arm_parent_death_signal() {}

#[cfg(unix)]
#[doc(hidden)]
pub fn parent_disappeared(original_parent: libc::pid_t, current_parent: libc::pid_t) -> bool {
    current_parent <= 1 || current_parent != original_parent
}

#[cfg(unix)]
fn spawn_parent_watchdog(env_prefix: &str, tx: mpsc::Sender<ServerEvent>) {
    let original_parent = current_parent_pid();
    arm_parent_death_signal();
    spawn_parent_watchdog_with(
        tx,
        parent_watchdog_interval(env_prefix),
        original_parent,
        current_parent_pid,
    );
}

#[cfg(unix)]
#[doc(hidden)]
pub fn spawn_parent_watchdog_with(
    tx: mpsc::Sender<ServerEvent>,
    interval: Duration,
    original_parent: libc::pid_t,
    current_parent: impl Fn() -> libc::pid_t + Send + 'static,
) {
    thread::spawn(move || loop {
        if parent_disappeared(original_parent, current_parent()) {
            let _ = tx.send(ServerEvent::ParentGone);
            break;
        }
        thread::sleep(interval);
    });
}

#[cfg(windows)]
fn spawn_parent_watchdog(_env_prefix: &str, tx: mpsc::Sender<ServerEvent>) {
    let Some(parent_pid) = windows_parent_pid() else {
        return;
    };
    thread::spawn(move || {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const INFINITE: u32 = u32::MAX;
        unsafe {
            let handle = OpenProcess(SYNCHRONIZE, 0, parent_pid);
            if handle.is_null() {
                return;
            }
            WaitForSingleObject(handle, INFINITE);
            CloseHandle(handle);
        }
        let _ = tx.send(ServerEvent::ParentGone);
    });
}

#[cfg(windows)]
fn windows_parent_pid() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let me = std::process::id();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == me {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        parent
    }
}

#[cfg(not(any(unix, windows)))]
fn spawn_parent_watchdog(_env_prefix: &str, _tx: mpsc::Sender<ServerEvent>) {}

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
        assert!(init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("todo_"));

        let tools = server
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap();
        let tools: Value = serde_json::from_str(&tools).unwrap();
        assert_eq!(tools["result"]["tools"][0]["name"], "todo_status");
    }

    #[test]
    fn tool_call_returns_structured_content() {
        let server = McpServer::new(ServerConfig::new("todo", "0.1.0", ".").tool(ToolSpec::read(
            "todo_status",
            "Return status",
            json!({ "type": "object", "properties": {} }),
            |_ctx, _args| Ok(json!({ "ok": true })),
        )));
        let raw = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"todo_status","arguments":{}}}"#)
            .unwrap();
        let response: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["structuredContent"]["ok"], true);
    }

    #[test]
    fn mutating_tool_is_serialized_by_dispatch() {
        let server = McpServer::new(
            ServerConfig::new("todo", "0.1.0", ".").tool(ToolSpec::write(
                "todo_create",
                "Create",
                json!({ "type": "object", "properties": {} }),
                |_ctx, _args| Ok(json!({ "created": true })),
            )),
        );
        assert!(server.line_calls_mutating_tool(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"todo_create","arguments":{}}}"#
        ));
    }

    #[cfg(unix)]
    #[test]
    fn parent_disappeared_when_ppid_changes_or_reparents_to_init() {
        assert!(!parent_disappeared(42, 42));
        assert!(parent_disappeared(42, 43));
        assert!(parent_disappeared(42, 1));
    }
}
