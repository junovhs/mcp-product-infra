//! `ishoo mcp` — a Model Context Protocol server over stdio (DEC-49).
//!
//! This is the agent front-end: a hand-rolled JSON-RPC 2.0 loop reading
//! newline-delimited requests on stdin and writing responses on stdout. It
//! answers the MCP handshake (`initialize`), advertises tools (`tools/list`),
//! and dispatches tool calls (`tools/call`). Every tool handler calls ishoo's
//! own core functions and serializes the typed result — it never invokes or
//! parses the CLI (DEC-49: one core, three front-ends).
//!
//! MCP-02 lands the scaffold plus a single seed tool, `ishoo_status`, proving
//! the transport end-to-end. MCP-03 replaces the inline tool list with a
//! registry diffed against the capability inventory; MCP-04/05/06 add the
//! authoring, read, and transition tools.

pub(crate) mod registry;
mod transport;

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The MCP protocol revision this server speaks when a client does not pin one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

// JSON-RPC 2.0 error codes (https://www.jsonrpc.org/specification#error_object).
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// STOR-22 (DEC-83): the resident store owner (the single writer) is unreachable, so a
/// store mutation is refused rather than executed by a second same-process writer. A
/// JSON-RPC server-error-range code, distinct from a client/protocol error.
const STORE_SERVICE_UNAVAILABLE: i64 = -32010;

const DEFAULT_SHUTDOWN_DRAIN: Duration = Duration::from_secs(120);
#[cfg(unix)]
const DEFAULT_PARENT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

static MCP_STARTUP_STORE_SYNC: OnceLock<Mutex<HashMap<PathBuf, StartupStoreSyncReport>>> =
    OnceLock::new();

#[derive(Clone, Debug, Serialize)]
struct StartupStoreSyncReport {
    store_ref: &'static str,
    state: String,
    reconcile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
}

#[derive(Debug)]
enum ServerEvent {
    Line(String),
    InputClosed,
    /// The host process (our parent) exited — stop the server instead of
    /// orphaning. Emitted by the unix and Windows parent-death watchdogs (FIX-123/FIX-126).
    ParentGone,
    Completed(Option<String>),
}

/// Run the stdio MCP loop until stdin closes. Returns a process exit code.
pub fn run_server(path: PathBuf) -> i32 {
    let path = Arc::new(path);
    let owner = match transport::ensure_owner_process(path.as_path()) {
        Ok(owner) => Some(owner),
        Err(error) => {
            // ADPT-03: a user-scope MCP registration can be opened from a brand-new
            // repo that has no Ishoo store yet. The transport must still initialize
            // and advertise tools so the agent can explain the setup path; tool calls
            // will return the normal "ishoo init" store error. If a store exists,
            // keep the old fail-closed behavior because writes require the resident
            // single owner.
            if !crate::model::workspace_exists(path.as_path()) {
                None
            } else {
                eprintln!("ishoo mcp error: {error}");
                return 1;
            }
        }
    };
    let (events_tx, events_rx) = mpsc::channel();
    spawn_stdin_reader(events_tx.clone());
    spawn_parent_watchdog(events_tx.clone());
    // FIX-124 (DEC-77): one serial writer for mutations, concurrent reads.
    let dispatch = Dispatch::new(path.clone(), events_tx.clone(), owner);

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
                    Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
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
                    // A single line of JSON per message (the MCP stdio framing).
                    // A write failure means the host is gone, so there is nothing
                    // left to do.
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

pub fn run_owner_server(path: PathBuf) -> i32 {
    match transport::run_owner_server(path) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ishoo mcp-owner error: {error}");
            1
        }
    }
}

fn run_startup_store_sync(path: &Path) {
    let root = crate::model::git_remote::canonical_workspace_root(path);
    let mut report = match crate::model::git_remote::sync_store_ref(&root) {
        Ok(outcome) => startup_sync_report_for(outcome),
        Err(reason) => StartupStoreSyncReport {
            store_ref: crate::model::git_remote::STORE_REF,
            state: "error".to_string(),
            reconcile: "not_run".to_string(),
            reason: Some(reason),
            paths: Vec::new(),
        },
    };
    match crate::model::git_remote::reconcile_store_if_behind(&root) {
        Ok(outcome) => report.reconcile = reconcile_label(outcome).to_string(),
        Err(reason) => {
            report.reconcile = "error".to_string();
            report.reason = Some(match report.reason.take() {
                Some(sync_reason) => format!("{sync_reason}; reconcile: {reason}"),
                None => reason,
            });
        }
    }
    startup_store_sync_reports()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(root, report);
}

fn startup_store_sync_reports() -> &'static Mutex<HashMap<PathBuf, StartupStoreSyncReport>> {
    MCP_STARTUP_STORE_SYNC.get_or_init(|| Mutex::new(HashMap::new()))
}

fn startup_sync_report_for(
    outcome: crate::model::git_remote::StoreSyncOutcome,
) -> StartupStoreSyncReport {
    use crate::model::git_remote::StoreSyncOutcome;
    let mut report = StartupStoreSyncReport {
        store_ref: crate::model::git_remote::STORE_REF,
        state: String::new(),
        reconcile: "not_run".to_string(),
        reason: None,
        paths: Vec::new(),
    };
    match outcome {
        StoreSyncOutcome::NoRemote => report.state = "no_remote".to_string(),
        StoreSyncOutcome::UpToDate => report.state = "up_to_date".to_string(),
        StoreSyncOutcome::Pushed => report.state = "pushed".to_string(),
        StoreSyncOutcome::Queued => report.state = "queued".to_string(),
        StoreSyncOutcome::Pulled => report.state = "pulled".to_string(),
        StoreSyncOutcome::Deferred(reason) => {
            report.state = "deferred".to_string();
            report.reason = Some(reason);
        }
        StoreSyncOutcome::Conflict(paths) => {
            report.state = "conflict".to_string();
            report.paths = paths;
        }
    }
    report
}

fn reconcile_label(outcome: crate::model::git_remote::StoreReconcile) -> &'static str {
    use crate::model::git_remote::StoreReconcile;
    match outcome {
        StoreReconcile::NoRef => "no_ref",
        StoreReconcile::InSync => "in_sync",
        StoreReconcile::Reconciled => "reconciled",
        StoreReconcile::LocalAhead => "local_ahead",
    }
}

pub(super) fn mcp_startup_store_sync_for(path: &Path) -> Option<Value> {
    let root = crate::model::git_remote::canonical_workspace_root(path);
    let report = startup_store_sync_reports()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&root)
        .cloned()?;
    serde_json::to_value(report).ok()
}

fn shutdown_drain() -> Duration {
    std::env::var("ISHOO_MCP_SHUTDOWN_DRAIN_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_SHUTDOWN_DRAIN)
}

#[cfg(unix)]
fn parent_watchdog_interval() -> Duration {
    std::env::var("ISHOO_MCP_PARENT_WATCHDOG_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_PARENT_WATCHDOG_INTERVAL)
}

#[cfg(unix)]
fn current_parent_pid() -> libc::pid_t {
    // SAFETY: getppid has no preconditions and does not mutate Rust-owned memory.
    unsafe { libc::getppid() }
}

#[cfg(target_os = "linux")]
fn arm_parent_death_signal() {
    // FIX-123 (DEC-77): bind the server lifetime to the host process. If the
    // parent dies, Linux asks the kernel to deliver SIGTERM; the polling
    // watchdog below covers platforms without PR_SET_PDEATHSIG and the tiny race
    // where the parent exits before the prctl takes effect.
    // SAFETY: prctl is called with the documented PR_SET_PDEATHSIG operation and
    // a valid signal number; unused varargs are zeroed as required by the libc
    // ABI.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn arm_parent_death_signal() {}

#[cfg(unix)]
fn parent_disappeared(original_parent: libc::pid_t, current_parent: libc::pid_t) -> bool {
    current_parent <= 1 || current_parent != original_parent
}

#[cfg(unix)]
fn spawn_parent_watchdog(tx: mpsc::Sender<ServerEvent>) {
    let original_parent = current_parent_pid();
    arm_parent_death_signal();
    spawn_parent_watchdog_with(
        tx,
        parent_watchdog_interval(),
        original_parent,
        current_parent_pid,
    );
}

// FIX-126 (DEC-77): Windows has neither PR_SET_PDEATHSIG nor reparenting, so an
// `ishoo mcp` whose host (e.g. Claude Code) exits without closing our stdin would
// orphan and run for days — exactly what was observed (a stale server lingered
// ~18h holding the installed binary locked). Mirror the unix watchdog by waiting
// on a handle to the parent process: when it exits, the wait signals and we break.
#[cfg(windows)]
fn spawn_parent_watchdog(tx: mpsc::Sender<ServerEvent>) {
    let Some(parent_pid) = windows_parent_pid() else {
        return; // can't resolve the parent; degrade to no watchdog, never crash
    };
    thread::spawn(move || {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const INFINITE: u32 = u32::MAX;
        // SAFETY: OpenProcess/WaitForSingleObject/CloseHandle take a valid access
        // mask and pid; a failed open returns a null handle we bail on, and the
        // handle is closed once the wait returns.
        unsafe {
            let handle = OpenProcess(SYNCHRONIZE, 0, parent_pid);
            if handle.is_null() {
                return; // parent already gone or inaccessible — nothing to watch
            }
            WaitForSingleObject(handle, INFINITE); // blocks until the parent exits
            CloseHandle(handle);
        }
        let _ = tx.send(ServerEvent::ParentGone);
    });
}

/// The PID that spawned this process, via a ToolHelp snapshot. `None` if it can't
/// be resolved (so the watchdog simply does not arm rather than guessing).
#[cfg(windows)]
fn windows_parent_pid() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let me = std::process::id();
    // SAFETY: a standard ToolHelp process-snapshot walk; the entry is zeroed with
    // its dwSize set as the API requires, and the snapshot handle is always closed.
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
fn spawn_parent_watchdog(_tx: mpsc::Sender<ServerEvent>) {}

#[cfg(unix)]
fn spawn_parent_watchdog_with(
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

/// Routes each request to the path that keeps the control surface correct:
/// mutating tool calls go to a single ordered worker (FIX-124 / DEC-77) so they
/// execute in strict arrival order and pipelined dependent mutations (e.g.
/// `ishoo_resolve` then `ishoo_done`) can never reorder; read-only calls are
/// spawned concurrently so a slow mutation never wedges reads.
struct Dispatch {
    path: Arc<PathBuf>,
    events_tx: mpsc::Sender<ServerEvent>,
    mutations_tx: mpsc::Sender<String>,
    owner: Option<transport::OwnerEndpoint>,
}

impl Dispatch {
    fn new(
        path: Arc<PathBuf>,
        events_tx: mpsc::Sender<ServerEvent>,
        owner: Option<transport::OwnerEndpoint>,
    ) -> Self {
        // The single serial writer: one worker drains an ordered FIFO, handling
        // one mutation at a time. Because the main loop reads stdin lines in
        // order and forwards each mutating line to this one channel, mutations
        // apply in exactly the order they arrived — exclusion AND ordering, where
        // the old global lock gave exclusion only. The worker exits when the last
        // `mutations_tx` (held by this struct) is dropped at shutdown.
        let (mutations_tx, mutations_rx) = mpsc::channel::<String>();
        let worker_path = path.clone();
        let worker_events = events_tx.clone();
        let worker_owner = owner.clone();
        thread::spawn(move || {
            for line in mutations_rx {
                let response =
                    handle_line_maybe_remote(worker_path.as_path(), &line, worker_owner.as_ref());
                let _ = worker_events.send(ServerEvent::Completed(response));
            }
        });
        Self {
            path,
            events_tx,
            mutations_tx,
            owner,
        }
    }

    fn dispatch(&self, line: String) {
        if line_calls_mutating_tool(&line) {
            // Ordered, serial. Send failure only happens if the worker is gone
            // (shutdown), in which case there is nothing left to do.
            let _ = self.mutations_tx.send(line);
        } else {
            spawn_response(
                self.path.clone(),
                line,
                self.events_tx.clone(),
                self.owner.clone(),
            );
        }
    }
}

/// Handle one read-only request on its own thread so concurrent reads never
/// block one another or wait behind a mutation (mutations are serialized
/// elsewhere via `Dispatch`).
fn spawn_response(
    path: Arc<PathBuf>,
    line: String,
    tx: mpsc::Sender<ServerEvent>,
    owner: Option<transport::OwnerEndpoint>,
) {
    thread::spawn(move || {
        let response = handle_line_maybe_remote(path.as_path(), &line, owner.as_ref());
        let _ = tx.send(ServerEvent::Completed(response));
    });
}

fn handle_line_maybe_remote(
    path: &std::path::Path,
    line: &str,
    owner: Option<&transport::OwnerEndpoint>,
) -> Option<String> {
    if let Some(owner) = owner.filter(|_| line_calls_tool(line)) {
        match transport::send_line(owner, line) {
            Ok(response) => return response,
            // FIX-150 (DEC-53 "crashes heal"): the resident owner is unreachable. A
            // store mutation must NEVER fall back to an in-process same-uid write while
            // a *live* owner exists (the STOR-22 second-writer hole). But a *dead* owner
            // must not wedge every write forever, and we must never claim a recovery that
            // does not happen (DEC-36). So re-elect: `ensure_owner_process` pings the
            // registration, and only when the owner is truly gone does it clear the stale
            // `mcp-owner.json` and spawn a fresh resident writer. Then retry the write once
            // against the fresh owner. If re-election or the retry fails, refuse with an
            // honest, actionable remedy — never a false "restarts automatically".
            Err(_) if line_calls_mutating_tool(line) => {
                match transport::recover_owner(path, owner) {
                    // A live owner (fresh-spawned or a restarted app's) is reachable —
                    // retry the write once against it.
                    transport::OwnerRecovery::Reelected(fresh) => {
                        match transport::send_line(&fresh, line) {
                            Ok(response) => return response,
                            Err(e) => {
                                return Some(error_frame(
                                    request_id(line),
                                    STORE_SERVICE_UNAVAILABLE,
                                    &format!(
                                        "store service unavailable — write refused; no changes \
                                         were made. A fresh resident store owner was elected but \
                                         could not be reached ({e}). Restart Ishoo (or run \
                                         `ishoo --path . mcp-owner`) and retry."
                                    ),
                                ));
                            }
                        }
                    }
                    // The owner is alive but did not answer — never start a rival writer.
                    transport::OwnerRecovery::LiveButUnreachable => {
                        return Some(error_frame(
                            request_id(line),
                            STORE_SERVICE_UNAVAILABLE,
                            "store service unavailable — write refused; no changes were made. \
                             The resident store owner is running but did not respond in time. \
                             Retry in a moment.",
                        ));
                    }
                    // The owner is gone and could not be re-elected — an honest remedy,
                    // never a false promise of automatic restart (DEC-36).
                    transport::OwnerRecovery::Down(e) => {
                        return Some(error_frame(
                            request_id(line),
                            STORE_SERVICE_UNAVAILABLE,
                            &format!(
                                "store service unavailable — write refused; no changes were \
                                 made. The resident store owner is down and could not be \
                                 re-elected ({e}). Restart Ishoo (or run \
                                 `ishoo --path . mcp-owner`) and retry."
                            ),
                        ));
                    }
                }
            }
            // A read MAY degrade gracefully (STOR-22): fall through to an in-process
            // read, which cannot corrupt the store. So orientation still works when the
            // owner is momentarily unreachable, while writes stay strictly fail-closed.
            Err(e) => {
                if line_calls_tool_named(line, "ishoo_status") {
                    return annotate_status_owner_unreachable(handle_line(path, line), &e);
                }
            }
        }
    }
    handle_line(path, line)
}

/// The JSON-RPC request id of a raw frame, or `Null` when absent/unparseable, so a
/// fail-closed refusal correlates with the exact call the host made.
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

fn line_calls_mutating_tool(line: &str) -> bool {
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
    // Mutation can depend on the arguments (an op-dispatched tool's `op` — DEC-86),
    // so classify against the call's arguments, not just the tool.
    let arguments = message
        .get("params")
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    registry::registry()
        .iter()
        .find(|tool| tool.name == name)
        .is_some_and(|tool| (tool.mutates_store)(&arguments))
}

fn annotate_status_owner_unreachable(response: Option<String>, error: &str) -> Option<String> {
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

    // STOR-27: this fact exists only for MCP status reads that tried the resident
    // socket and observed a transport failure. Plain CLI/UI status never carries it,
    // so normal non-MCP use cannot false-alarm on an owner it never needed.
    structured.insert(
        "mcp_owner".to_string(),
        json!({
            "state": "unreachable",
            "source": "mcp_transport",
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

/// Parse and dispatch one JSON-RPC frame. Returns the serialized response, or
/// `None` for notifications (which never get a reply). A malformed frame yields
/// a JSON-RPC parse error rather than killing the loop.
fn handle_line(path: &std::path::Path, line: &str) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(error_frame(
                Value::Null,
                PARSE_ERROR,
                &format!("Parse error: {error}"),
            ));
        }
    };

    // Presence of an `id` member distinguishes a request (must be answered) from
    // a notification (must not be answered), per JSON-RPC 2.0.
    let is_request = message.get("id").is_some();
    let id = message.get("id").cloned().unwrap_or(Value::Null);

    let method = match message.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => {
            return is_request
                .then(|| error_frame(id, INVALID_REQUEST, "Invalid Request: missing method"));
        }
    };

    match method {
        "initialize" => Some(result_frame(id, initialize_result(&message))),
        "tools/list" => Some(result_frame(id, tools_list_result())),
        "tools/call" => Some(tools_call(path, id, &message)),
        "ping" => Some(result_frame(id, json!({}))),
        // `notifications/initialized` and any other client notification: no reply.
        _ if method.starts_with("notifications/") => None,
        _ => is_request
            .then(|| error_frame(id, METHOD_NOT_FOUND, &format!("Method not found: {method}"))),
    }
}

/// Build the `initialize` result, echoing the client's requested protocol
/// version when it sends a string, else falling back to the default.
fn initialize_result(message: &Value) -> Value {
    let protocol_version = message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "ishoo",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Auto-orientation injected into the agent's context on connect (MCP-32),
        // so the protocol arrives without prompting "run ishoo brief". Concise on
        // purpose; the full protocol is the ishoo_brief tool.
        "instructions": SERVER_INSTRUCTIONS,
    })
}

/// Concise server instructions a compliant host loads into the model's context at
/// connect time. Points at the orientation tool and the full protocol; the brief
/// stays the single source of truth (via `ishoo_brief`), so this can't drift into
/// a second protocol doc.
const SERVER_INSTRUCTIONS: &str = "\
This repository is managed by Ishoo, the issue control plane for AI agents. Drive all \
issue, plan, and decision work through the ishoo_* MCP tools — not the CLI. Begin by \
calling ishoo_status to orient: it reports your current focus, the recommended next \
step, and governing_decisions (the ACCEPTED ADRs you must not contradict). Call \
ishoo_brief for the full agent protocol (SEMMAP-first workflow, Scope/Resolution \
Contracts, the land gates) before non-trivial work. Never edit .ishoo/ storage files \
directly.";

/// The advertised tool set, rendered from the registry (MCP-03).
fn tools_list_result() -> Value {
    let tools: Vec<Value> = registry::registry()
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": (tool.input_schema)(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Dispatch a `tools/call` through the registry. Unknown tools and missing names
/// are invalid-params errors; a registered tool with no handler yet (owned by a
/// later slice) returns a structured "not implemented" naming the owning issue;
/// a handled tool returns its typed result as both text and structured content.
fn tools_call(path: &std::path::Path, id: Value, message: &Value) -> String {
    let params = message.get("params");
    let name = match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
        Some(name) => name,
        None => return error_frame(id, INVALID_PARAMS, "Missing tool name in params"),
    };
    let tools = registry::registry();
    let tool = match tools.iter().find(|tool| tool.name == name) {
        Some(tool) => tool,
        None => return error_frame(id, INVALID_PARAMS, &format!("Unknown tool: {name}")),
    };
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    match tool.handler {
        Some(handler) => execute_tool_handler(path, id, name, tool, handler, &arguments),
        None => error_frame(
            id,
            registry::NOT_IMPLEMENTED,
            &format!(
                "Tool '{name}' is registered (capability '{}', core fn {}); its handler is \
                 delivered by {}.",
                tool.capability, tool.core_fn, tool.owner_issue
            ),
        ),
    }
}

fn execute_tool_handler(
    path: &std::path::Path,
    id: Value,
    name: &str,
    tool: &registry::ToolSpec,
    handler: registry::Handler,
    arguments: &Value,
) -> String {
    match handler(path, arguments) {
        Ok(mut value) => {
            // FIX-76 / DEC-51/52/54: a mutating tool must persist the store
            // the same way the CLI autocommit wrapper does — snapshot to the
            // refs/ishoo/store ref and best-effort push. The MCP server is
            // dispatched before that wrapper, so it does it here.
            if (tool.mutates_store)(arguments) {
                // FIX-76 / DEC-51/52/54: snapshot the just-written record to the
                // refs/ishoo/store ref and best-effort push.
                let commit_message = format!("chore(ishoo): mcp {name} mutation");
                // FIX-89: the MCP server is long-running, so it records the store
                // locally and hands the push to a detached thread, returning at once.
                // This keeps the mutation off the network and — crucially — releases
                // the global mutation lock immediately, so a slow/hung push can never
                // freeze every other mutation behind it (the Windows failure mode).
                let result =
                    crate::model::git_remote::commit_store_mutation_deferred(path, &commit_message);
                // MCP-40: the handler already wrote the record to the local store, so a
                // snapshot/publish failure does NOT mean the mutation failed. Returning a
                // JSON-RPC error here would hide the created id and tempt a retry that
                // duplicates the record. Instead, fold a truthful durability state into the
                // single success result — the agent learns the record exists and how to
                // recover, and never retries blindly.
                attach_store_sync_result(&mut value, result);
            }
            result_frame(id, tool_ok(value))
        }
        Err(error) => error_frame(id, error.code, &error.message),
    }
}

/// MCP-40: fold the store snapshot/publish outcome into a mutating tool's result as a
/// `publish` field. On success it reports the sync state (pushed/deferred/…); on a
/// snapshot/publish failure it reports `state: "failed"` with the reason and a
/// non-duplicating recovery, since the record is already written locally.
fn attach_store_sync_result(
    value: &mut Value,
    result: Result<crate::model::git_remote::StoreSyncOutcome, String>,
) {
    let publish = match result {
        Ok(outcome) => publish_json_for(outcome),
        Err(reason) => json!({
            "store_ref": crate::model::git_remote::STORE_REF,
            "state": "failed",
            "reason": reason,
            "recovery": "the record is written to the local store but not yet snapshotted to \
                         refs/ishoo/store; it will be included in the next store mutation or by \
                         `ishoo doctor` / the background publisher. Do NOT re-run this mutation — \
                         the record already exists.",
        }),
    };
    match value {
        Value::Object(map) => {
            map.insert("publish".to_string(), publish);
        }
        other => {
            *other = json!({ "value": other.take(), "publish": publish });
        }
    }
}

fn publish_json_for(outcome: crate::model::git_remote::StoreSyncOutcome) -> Value {
    match outcome {
        crate::model::git_remote::StoreSyncOutcome::NoRemote => {
            json!({ "store_ref": crate::model::git_remote::STORE_REF, "state": "no_remote" })
        }
        crate::model::git_remote::StoreSyncOutcome::UpToDate => {
            json!({ "store_ref": crate::model::git_remote::STORE_REF, "state": "up_to_date" })
        }
        crate::model::git_remote::StoreSyncOutcome::Pushed => {
            json!({ "store_ref": crate::model::git_remote::STORE_REF, "state": "pushed" })
        }
        crate::model::git_remote::StoreSyncOutcome::Queued => json!({
            "store_ref": crate::model::git_remote::STORE_REF,
            "state": "queued",
            "detail": "recorded locally; publishing to the remote in the background"
        }),
        crate::model::git_remote::StoreSyncOutcome::Pulled => {
            json!({ "store_ref": crate::model::git_remote::STORE_REF, "state": "pulled" })
        }
        crate::model::git_remote::StoreSyncOutcome::Deferred(reason) => json!({
            "store_ref": crate::model::git_remote::STORE_REF,
            "state": "deferred",
            "reason": reason
        }),
        crate::model::git_remote::StoreSyncOutcome::Conflict(paths) => json!({
            "store_ref": crate::model::git_remote::STORE_REF,
            "state": "conflict",
            "paths": paths
        }),
    }
}

/// Wrap a typed tool result as an MCP `tools/call` success payload: the JSON as
/// a text content block (the universal path) plus `structuredContent` for hosts
/// that consume typed output directly.
fn tool_ok(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": value,
        "isError": false
    })
}

/// Serialize a JSON-RPC success response.
fn result_frame(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// Serialize a JSON-RPC error response.
fn error_frame(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

#[cfg(test)]
mod tests;
