//! OWN-02 / DEC-06: an owner its app reports unhealthy is retired automatically
//! — drained, bounded, and never silent.
//!
//! Every owner here is a REAL child process, because the whole failure being
//! fixed is a *resident process* that answers its socket while its application
//! is wedged, and because retirement exits that process. The child is this test
//! binary re-executed with a filter that selects an owner-mode test function.
//!
//! Child mode is selected by the working directory, not an environment
//! variable: `ensure_owner_process` pins a spawned owner's cwd to the workspace
//! root, so an owner elected by infrastructure lands in the same temp workspace
//! as one the test spawned by hand, and both find the same markers. A normal
//! `cargo test` run has the crate root as its cwd, where no marker exists, so
//! the child-mode functions are inert passes.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use mcp_product_infra::sidecar::{
    enforce_owner_health, query_owner_health, run_owner_server, send_line, OwnerHealthAction,
};
use mcp_product_infra::{
    McpServer, OwnerEndpoint, OwnerHealth, OwnerHealthState, ServerConfig, SidecarConfig, ToolSpec,
};

/// Present only inside a test workspace: its absence is what keeps the
/// child-mode functions inert during an ordinary suite run.
const MODE_MARKER: &str = "own02-mode";

/// The app is "broken" exactly while this file exists — the stand-in for the
/// real thing (a warm handle that started returning `None`).
fn unhealthy_marker(dir: &Path) -> PathBuf {
    dir.join("app-is-broken")
}

/// Written by the slow mutation when it finishes. Its existence is the proof
/// that retirement drained the work instead of cutting it.
fn mutation_done_marker(dir: &Path) -> PathBuf {
    dir.join("slow-mutation-finished")
}

fn child_dir() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?;
    dir.join(MODE_MARKER).exists().then_some(dir)
}

/// The sidecar config every participant shares. `owner_args` re-executes this
/// test binary in owner mode, so infrastructure electing a replacement produces
/// a real owner process without the test being involved.
fn sidecar_config(dir: &Path) -> SidecarConfig {
    SidecarConfig::new("own02", dir, dir.join("cache"))
        .app_version("0.0.1")
        .owner_args(["own02_child_owner", "--exact"])
        // A short idle timeout self-reaps any owner a failing test leaks.
        .idle_timeout(Duration::from_secs(20))
}

/// The same config plus the app health hook — the only thing that may report
/// this owner unhealthy (DEC-03).
fn sidecar_config_with_health(dir: &Path) -> SidecarConfig {
    let marker = unhealthy_marker(dir);
    sidecar_config(dir).health(move || {
        if marker.exists() {
            OwnerHealth::unhealthy("project database handle returned None")
        } else {
            OwnerHealth::ready()
        }
    })
}

fn owner_server(dir: &Path) -> McpServer {
    let done = mutation_done_marker(dir);
    McpServer::new(
        ServerConfig::new("own02", "0.0.1", dir)
            .tool(ToolSpec::read(
                "ok",
                "Succeeds immediately.",
                serde_json::json!({ "type": "object" }),
                |_ctx, _args| Ok(serde_json::json!({ "served": true })),
            ))
            .tool(ToolSpec::write(
                "slow_write",
                "A mutation that legitimately takes a while.",
                serde_json::json!({ "type": "object" }),
                move |_ctx, _args| {
                    thread::sleep(Duration::from_millis(1500));
                    std::fs::write(&done, "committed").expect("record the committed mutation");
                    Ok(serde_json::json!({ "committed": true }))
                },
            )),
    )
}

/// Child mode: the resident owner process. Inert during a normal suite run.
#[test]
fn own02_child_owner() {
    let Some(dir) = child_dir() else {
        return;
    };
    let server = owner_server(&dir);
    run_owner_server(
        sidecar_config_with_health(&dir),
        || Ok(()),
        move |line| server.handle_line(line),
    )
    .expect("child owner runs");
    // Reached only on the lock-contention early return; retirement exits the
    // process. Either way the child must not fall through into other tests.
    std::process::exit(0);
}

/// Child mode: an owner whose app registered NO health hook, for the test that
/// existing behavior is untouched.
#[test]
fn own02_child_owner_without_health_hook() {
    let Some(dir) = child_dir() else {
        return;
    };
    let server = owner_server(&dir);
    run_owner_server(
        sidecar_config(&dir),
        || Ok(()),
        move |line| server.handle_line(line),
    )
    .expect("child owner runs");
    std::process::exit(0);
}

/// Child mode: the stdio MCP server — the real client of the resident owner,
/// and the surface an agent actually calls. Inert during a normal suite run.
#[test]
fn own02_child_stdio() {
    let Some(dir) = child_dir() else {
        return;
    };
    let code = McpServer::new(
        ServerConfig::new("own02", "0.0.1", &dir)
            .tool(ToolSpec::read(
                "ok",
                "Succeeds immediately.",
                serde_json::json!({ "type": "object" }),
                |_ctx, _args| Ok(serde_json::json!({ "served": "in-process" })),
            ))
            .sidecar(sidecar_config_with_health(&dir)),
    )
    .run_stdio();
    std::process::exit(code);
}

/// The incident, end to end: an agent calls a tool, the resident owner is
/// wedged, and the call is served anyway by a replacement.
#[test]
fn a_tool_call_against_a_wedged_owner_is_served_by_a_replacement() {
    let workspace = TestWorkspace::new();
    workspace.wedge_the_app();

    // The owner an agent's session would attach to: alive, reachable, current
    // build, and broken.
    let mut wedged = ChildOwner::spawn(&workspace, "own02_child_owner");
    assert_eq!(
        query_owner_health(&wedged.endpoint)
            .expect("health query answers")
            .state,
        OwnerHealthState::Unhealthy,
        "precondition: the owner reports itself unusable"
    );

    let mut stdio = StdioClient::spawn(&workspace);
    let response = stdio.call_tool(1, "ok");

    assert!(
        response.contains("\"result\""),
        "the call must be served, not refused: {response}"
    );
    assert!(
        !response.contains("in-process"),
        "it must be served by a resident owner, not by the degraded in-process \
         fallback: {response}"
    );

    let replacement = workspace.wait_for_new_registration(&wedged.endpoint);
    assert_ne!(
        replacement.generation, wedged.endpoint.generation,
        "a replacement runtime must be serving — a fresh generation is the only \
         proof of that, since the retired listener can briefly still answer"
    );
    assert!(
        wedged.wait_for_exit(),
        "the wedged owner must actually be gone, not merely bypassed"
    );

    stdio.shutdown();
    workspace.retire_registered_owner();
}

/// DEC-05: retirement drains in-flight work. A mutation running when the app
/// goes unhealthy must finish and commit — never be cut at an arbitrary point.
#[test]
fn retirement_drains_an_in_flight_mutation_instead_of_cutting_it() {
    let workspace = TestWorkspace::new();
    let mut owner = ChildOwner::spawn(&workspace, "own02_child_owner");

    // Start a legitimately slow mutation against the healthy owner.
    let endpoint = owner.endpoint.clone();
    let mutation = thread::spawn(move || {
        send_line(
            &endpoint,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"slow_write","arguments":{}}}"#,
        )
    });

    // Mid-flight, the application goes bad. The next call boundary retires the
    // owner — with the mutation still running inside it.
    thread::sleep(Duration::from_millis(300));
    workspace.wedge_the_app();
    let action = enforce_owner_health(&workspace.config, &owner.endpoint);
    assert!(
        matches!(action, OwnerHealthAction::Retired { .. }),
        "an unhealthy owner must be retired at the call boundary"
    );

    let outcome = mutation
        .join()
        .expect("the mutation thread must not panic")
        .expect("the in-flight mutation must receive its response, not a severed socket");
    let frame = outcome.expect("a mutation response frame");
    assert!(
        frame.contains("\"committed\":true"),
        "the mutation must run to completion: {frame}"
    );
    assert!(
        mutation_done_marker(workspace.dir()).exists(),
        "the mutation's write must have landed before the process exited"
    );
    assert!(
        owner.wait_for_exit(),
        "and only then does the retired owner exit"
    );

    workspace.retire_registered_owner();
}

/// The bound is the safety property: an app that is unhealthy at every startup
/// must degrade to a legible terminal error, never to a restart loop.
#[test]
fn repeated_unhealthy_startups_stop_at_the_bound_instead_of_looping() {
    let workspace = TestWorkspace::new();
    // Broken now and broken for every replacement: each new owner is born sick.
    workspace.wedge_the_app();
    let mut first = ChildOwner::spawn(&workspace, "own02_child_owner");

    let mut retirements = 0;
    let mut exhausted = None;
    let mut endpoint = first.endpoint.clone();
    // Generously more attempts than the bound: the assertion is that this stops
    // on its own, so the loop must be able to run past the bound if it does not.
    for _ in 0..8 {
        match enforce_owner_health(&workspace.config, &endpoint) {
            OwnerHealthAction::Retired { replacement, .. } => {
                retirements += 1;
                endpoint = replacement;
            }
            OwnerHealthAction::Exhausted {
                attempts,
                unhealthy,
                ..
            } => {
                exhausted = Some((attempts, unhealthy));
                break;
            }
            other => panic!("unexpected action for a confirmed-unhealthy owner: {other:?}"),
        }
    }

    let (attempts, unhealthy) = exhausted.expect(
        "an app that is unhealthy on every startup must exhaust the bound, not \
         retire owners forever",
    );
    assert_eq!(
        retirements, attempts,
        "the reported attempt count must be the number of retirements that happened"
    );
    assert!(
        (1..=4).contains(&retirements),
        "retirement must stop after a small bounded number of attempts, got {retirements}"
    );
    assert_eq!(
        unhealthy.detail.as_deref(),
        Some("project database handle returned None"),
        "the terminal report must carry the app's own diagnosis, not a generic string"
    );
    assert!(
        !unhealthy.generation.is_empty(),
        "the terminal report must name the runtime it gave up on"
    );

    // And it stays stopped: asking again does not resume the loop.
    assert!(
        matches!(
            enforce_owner_health(&workspace.config, &endpoint),
            OwnerHealthAction::Exhausted { .. }
        ),
        "the bound must hold across calls, not reset on the next one"
    );

    let _ = first.wait_for_exit();
    workspace.retire_registered_owner();
}

/// An app that never said how to tell ready from wedged does not get
/// infrastructure guessing on its behalf — and gets exactly today's behavior.
#[test]
fn an_owner_without_a_health_hook_is_never_retired() {
    let workspace = TestWorkspace::new();
    // Armed, and irrelevant: this owner registers no hook, so nothing reads it.
    workspace.wedge_the_app();
    let mut owner = ChildOwner::spawn(&workspace, "own02_child_owner_without_health_hook");

    assert_eq!(
        query_owner_health(&owner.endpoint)
            .expect("health query answers")
            .state,
        OwnerHealthState::Unknown,
        "no hook means no opinion — never a verdict of unhealthy"
    );

    for _ in 0..3 {
        assert!(
            matches!(
                enforce_owner_health(&workspace.config, &owner.endpoint),
                OwnerHealthAction::Proceed
            ),
            "an owner nobody has called unhealthy must be left alone"
        );
        // Past the probe throttle, so each pass is a real query rather than a
        // cached verdict.
        thread::sleep(Duration::from_millis(5100));
    }

    assert!(
        owner.is_running(),
        "the owner process must still be serving"
    );
    let registered = workspace.read_registration().expect("a live registration");
    assert_eq!(
        registered.generation, owner.endpoint.generation,
        "and it must be the same runtime — nothing was replaced"
    );

    owner.retire();
}

/// A tool that fails is the app refusing a call, not the owner being sick.
/// Infrastructure must not read a handler error as a reason to retire (DEC-03).
#[test]
fn an_ordinary_tool_failure_never_triggers_retirement() {
    let workspace = TestWorkspace::new();
    let mut owner = ChildOwner::spawn(&workspace, "own02_child_owner");

    // A malformed tool call: the handler refuses it. The app stays healthy.
    let refusal = send_line(
        &owner.endpoint,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
    )
    .expect("the owner answers")
    .expect("a refusal is still a response");
    assert!(
        refusal.contains("\"error\""),
        "expected a refusal frame, got: {refusal}"
    );

    assert!(
        matches!(
            enforce_owner_health(&workspace.config, &owner.endpoint),
            OwnerHealthAction::Proceed
        ),
        "a refused call must not move the owner's fate"
    );
    assert!(owner.is_running(), "and must not retire the owner");

    owner.retire();
}

/// A temp workspace plus the sidecar config every participant shares.
struct TestWorkspace {
    dir: tempfile::TempDir,
    config: SidecarConfig,
}

impl TestWorkspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MODE_MARKER), "own02").expect("arm child mode");
        let config = sidecar_config_with_health(dir.path());
        Self { dir, config }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn wedge_the_app(&self) {
        std::fs::write(unhealthy_marker(self.dir()), "wedged").expect("arm the unhealthy marker");
    }

    fn read_registration(&self) -> Option<OwnerEndpoint> {
        let raw = std::fs::read_to_string(self.config.endpoint_path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Wait for a registration that is not `previous` — the replacement.
    fn wait_for_new_registration(&self, previous: &OwnerEndpoint) -> OwnerEndpoint {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(endpoint) = self.read_registration() {
                if endpoint.generation != previous.generation {
                    return endpoint;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("no replacement owner was ever registered");
    }

    /// Best-effort teardown: retire whatever owner is registered so no child
    /// outlives the test holding the temp dir open.
    fn retire_registered_owner(&self) {
        if let Some(endpoint) = self.read_registration() {
            let _ = send_line(
                &endpoint,
                r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && process_is_alive_for_test(&endpoint) {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// A resident owner running in a real child process, plus its registration.
struct ChildOwner {
    child: Child,
    endpoint: OwnerEndpoint,
}

impl ChildOwner {
    fn spawn(workspace: &TestWorkspace, test_fn: &str) -> Self {
        let exe = std::env::current_exe().expect("test binary path");
        let mut child = Command::new(exe)
            .arg(test_fn)
            .arg("--exact")
            .current_dir(workspace.dir())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child owner");
        let endpoint = wait_for_endpoint(&workspace.config, child.id()).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("child owner never published its endpoint");
        });
        Self { child, endpoint }
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn wait_for_exit(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => return false,
            }
        }
        false
    }

    fn retire(&mut self) {
        let _ = send_line(
            &self.endpoint,
            r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#,
        );
        if !self.wait_for_exit() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The stdio MCP server in a child process, driven the way a host drives it.
struct StdioClient {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl StdioClient {
    fn spawn(workspace: &TestWorkspace) -> Self {
        let exe = std::env::current_exe().expect("test binary path");
        let mut child = Command::new(exe)
            .arg("own02_child_stdio")
            .arg("--exact")
            .current_dir(workspace.dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stdio server");
        let stdin = child.stdin.take().expect("stdin pipe");
        let stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
        let mut client = Self {
            child,
            stdin,
            stdout,
        };
        client.request(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"own02","version":"1"}}}"#,
        );
        client
    }

    /// Send one JSON-RPC line and read the next JSON-RPC line back.
    ///
    /// The child is a test binary, so libtest writes its own progress lines
    /// ("running 1 test", blank lines) onto the same stdout that carries the
    /// MCP transport. Skip anything that is not a JSON frame rather than
    /// mistaking a harness banner for a response.
    fn request(&mut self, line: &str) -> String {
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush request");
        loop {
            let mut response = String::new();
            let read = self.stdout.read_line(&mut response).expect("read response");
            assert!(read > 0, "the stdio server closed without answering");
            if response.trim_start().starts_with('{') {
                return response;
            }
        }
    }

    fn call_tool(&mut self, id: u32, name: &str) -> String {
        self.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{{}}}}}}"#
        ))
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for the registration published by THIS child. Tests reuse a workspace
/// across owner generations, so a registration left by a previous owner would
/// otherwise be read as the new one's.
fn wait_for_endpoint(config: &SidecarConfig, pid: u32) -> Option<OwnerEndpoint> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(config.endpoint_path()) {
            if let Ok(endpoint) = serde_json::from_str::<OwnerEndpoint>(&raw) {
                if endpoint.pid == pid {
                    return Some(endpoint);
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Liveness by the only means a test has for a process it did not spawn: ask
/// the endpoint. A retired owner stops answering.
fn process_is_alive_for_test(endpoint: &OwnerEndpoint) -> bool {
    send_line(endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok()
}

/// Guard against a silently-inert suite: if the mode marker ever stops
/// selecting child mode, every child-mode test would pass by doing nothing.
#[test]
fn child_mode_is_inert_only_outside_a_test_workspace() {
    assert!(
        child_dir().is_none(),
        "the ordinary suite run must not be in a child-mode workspace"
    );
    let workspace = TestWorkspace::new();
    assert!(
        workspace.dir().join(MODE_MARKER).exists(),
        "a test workspace must arm child mode, or every child-mode test is a no-op"
    );
}
