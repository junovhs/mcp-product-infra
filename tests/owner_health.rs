//! OWN-01: an owner whose application state is broken must be nameable.
//!
//! The owner runs `run_owner_server` in a REAL child process — this test binary
//! re-executed with `TURNKEY_OWNER_HEALTH_CHILD_DIR` set — because the point of
//! the issue is a *resident process* whose transport answers while its app is
//! wedged, and because the owner loop exits the process when it retires. The
//! child's app health is driven by a marker file so the parent can decide,
//! before spawn, which condition it is testing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use mcp_product_infra::sidecar::{query_owner_health, run_owner_server, send_line};
use mcp_product_infra::types::{kinds, ToolError};
use mcp_product_infra::{
    McpServer, OwnerEndpoint, OwnerHealth, OwnerHealthState, ServerConfig, SidecarConfig, ToolSpec,
};

const CHILD_DIR_ENV: &str = "TURNKEY_OWNER_HEALTH_CHILD_DIR";

/// The child's app is "broken" exactly while this file exists — the stand-in
/// for the real thing (a warm handle that started returning `None`).
fn unhealthy_marker(dir: &Path) -> PathBuf {
    dir.join("app-is-broken")
}

fn child_config(dir: &Path) -> SidecarConfig {
    let marker = unhealthy_marker(dir);
    // A short idle timeout self-reaps the child if a failing parent leaks it.
    SidecarConfig::new("owner-health-test", dir, dir.join("cache"))
        .app_version("0.0.1")
        .idle_timeout(Duration::from_secs(10))
        .health(move || {
            if marker.exists() {
                OwnerHealth::unhealthy("project database handle returned None")
            } else {
                OwnerHealth::ready()
            }
        })
}

/// Child mode: with `TURNKEY_OWNER_HEALTH_CHILD_DIR` set, this "test" is the
/// owner process. Without it (the normal suite run) it is an inert pass.
#[test]
fn child_owner_serves_until_shutdown() {
    let dir = match std::env::var(CHILD_DIR_ENV) {
        Ok(dir) => dir,
        Err(_) => return,
    };
    let dir = PathBuf::from(dir);
    // A real MCP server behind the owner, with one tool that fails the way an
    // ordinary app tool fails: a typed `ToolError`, not a panic or a hang.
    let server = McpServer::new(ServerConfig::new("owner-health-test", "0.0.1", &dir).tool(
        ToolSpec::read(
            "boom",
            "Always fails with an ordinary tool error.",
            serde_json::json!({ "type": "object" }),
            |_ctx, _args| Err(ToolError::server("tool handler failed").with_kind(kinds::INTERNAL)),
        ),
    ));
    run_owner_server(
        child_config(&dir),
        || Ok(()),
        move |line| server.handle_line(line),
    )
    .expect("child owner runs");
    // Reached only on the lock-contention early return; the retirement path
    // exits the process. Either way the child must not fall through to run
    // other tests.
    std::process::exit(0);
}

/// The state that had no name: alive, reachable, current build — and broken.
#[test]
fn a_wedged_owner_answers_ping_and_reports_itself_unhealthy() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(unhealthy_marker(dir.path()), "wedged").expect("arm the unhealthy marker");
    let mut owner = ChildOwner::spawn(dir.path());

    // Every check that existed before OWN-01 says the owner is fine.
    assert!(
        send_line(
            &owner.endpoint,
            r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#
        )
        .is_ok(),
        "ping must still succeed — a wedged owner's transport is healthy"
    );

    let report = query_owner_health(&owner.endpoint).expect("health query answers");
    assert_eq!(
        report.state,
        OwnerHealthState::Unhealthy,
        "the app's hook must be able to say the owner is broken"
    );
    assert_eq!(
        report.detail.as_deref(),
        Some("project database handle returned None"),
        "the app's diagnosis must reach the client verbatim"
    );
    assert_eq!(report.pid, owner.child.id(), "the report names the process");
    assert_eq!(report.fingerprint, owner.endpoint.fingerprint);
    assert_eq!(
        report.generation, owner.endpoint.generation,
        "the answering runtime must be the one we registered against"
    );

    owner.retire();
}

/// A tool that fails is the app refusing a call. Infrastructure must not read
/// that as the owner being sick, and must not retire it (DEC-03).
#[test]
fn an_ordinary_tool_error_leaves_the_owner_healthy_and_alive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut owner = ChildOwner::spawn(dir.path());

    let before = query_owner_health(&owner.endpoint).expect("health before");
    assert_eq!(before.state, OwnerHealthState::Ready);

    let failure = send_line(
        &owner.endpoint,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"boom","arguments":{}}}"#,
    )
    .expect("the owner answers")
    .expect("an error frame is still a response");
    assert!(
        failure.contains("tool handler failed") && failure.contains(kinds::INTERNAL),
        "expected an ordinary typed tool error, got: {failure}"
    );

    let after = query_owner_health(&owner.endpoint).expect("health after");
    assert_eq!(
        after, before,
        "a handler error must not move the owner's reported health"
    );
    assert!(
        owner.child.try_wait().expect("poll child").is_none(),
        "a handler error must not retire the owner"
    );

    owner.retire();
}

/// "The endpoint answered" must never be mistaken for "the new runtime is
/// serving me" — the reload trap, where the old listener keeps answering.
#[test]
fn the_reported_generation_changes_across_an_owner_replacement() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut first = ChildOwner::spawn(dir.path());
    let before = query_owner_health(&first.endpoint).expect("first health");
    first.retire();

    let mut second = ChildOwner::spawn(dir.path());
    let after = query_owner_health(&second.endpoint).expect("second health");

    assert_eq!(
        before.state, after.state,
        "both runtimes report the same app state — only identity changed"
    );
    assert_ne!(
        before.generation, after.generation,
        "a replacement owner must report a fresh generation"
    );
    assert_ne!(
        first.endpoint.generation, second.endpoint.generation,
        "and must register that fresh generation for clients to compare"
    );
    assert_eq!(
        after.generation, second.endpoint.generation,
        "the live registration must match the runtime actually answering"
    );

    second.retire();
}

/// A real resident owner in a child process, plus its registration.
struct ChildOwner {
    child: std::process::Child,
    endpoint: OwnerEndpoint,
}

impl ChildOwner {
    fn spawn(dir: &Path) -> Self {
        let config = child_config(dir);
        let exe = std::env::current_exe().expect("test binary path");
        let mut child = Command::new(exe)
            .arg("child_owner_serves_until_shutdown")
            .arg("--exact")
            .env(CHILD_DIR_ENV, dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child owner");
        let endpoint = wait_for_endpoint(&config, child.id()).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("child owner never published its endpoint");
        });
        Self { child, endpoint }
    }

    /// Retire this owner and wait for the process to go, so a replacement can
    /// win the singleton lock.
    fn retire(&mut self) {
        let _ = send_line(
            &self.endpoint,
            r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for the registration published by THIS child. A replacement test reuses
/// the workspace, so a registration left by the previous owner would otherwise
/// be read as the new one's.
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
