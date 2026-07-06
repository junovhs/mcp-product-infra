use super::*;
use clap::CommandFactory;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use tempfile::tempdir;

const PRODUCT_DOMAIN_TOOLS: &[&str] = &[
    "ishoo_milestone_new",
    "ishoo_milestone_list",
    "ishoo_milestone_show",
    "ishoo_milestone_close",
    "ishoo_milestone_link",
    "ishoo_milestone_check",
    "ishoo_epic_new",
    "ishoo_epic_list",
    "ishoo_epic_show",
    "ishoo_roadmap_show",
    "ishoo_roadmap_set",
    "ishoo_version_get",
    "ishoo_version_bump",
    "ishoo_version_set_source",
    "ishoo_people_add",
    "ishoo_people_list",
];

/// Parse a handler response line back into JSON for assertions.
fn respond(path: &std::path::Path, request: &str) -> Value {
    let line = handle_line(path, request).expect("request must produce a response");
    serde_json::from_str(&line).expect("response must be valid JSON")
}

#[test]
fn initialize_echoes_protocol_version_and_reports_server_info() {
    let dir = tempdir().unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(response["result"]["serverInfo"]["name"], "ishoo");
    // Tools capability must be advertised so the host enables tools/list.
    assert!(response["result"]["capabilities"]["tools"].is_object());
    // MCP-32: the protocol auto-orients via server instructions, so the host can
    // inject it without the user prompting "run ishoo brief".
    let instructions = response["result"]["instructions"]
        .as_str()
        .expect("initialize must carry server instructions");
    assert!(instructions.contains("ishoo_status"));
    assert!(instructions.contains("ishoo_brief"));
    assert!(instructions.contains("ishoo_* MCP tools"));
}

#[test]
fn brief_tool_returns_the_full_protocol() {
    let dir = tempdir().unwrap();
    let response = call_tool(dir.path(), "ishoo_brief", json!({}));
    let brief = response["result"]["structuredContent"]["brief"]
        .as_str()
        .expect("ishoo_brief returns the protocol text");
    // It's the real protocol, not a stub.
    assert!(brief.contains("Agent Protocol"));
    assert!(brief.contains("Scope Contract"));
    assert!(brief.contains("Default completion is `ishoo_done <id>`"));
    assert!(brief.contains("synthesizes one commit from the Resolution Contract"));
    assert!(!brief.contains("`ishoo land` does **not** commit your work"));
    assert!(!brief.contains("Commit your change with a real message first"));
}

#[test]
fn initialize_falls_back_to_default_protocol_version() {
    let dir = tempdir().unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(
        response["result"]["protocolVersion"],
        DEFAULT_PROTOCOL_VERSION
    );
}

#[test]
fn tools_list_returns_full_registry_with_schemas() {
    let dir = tempdir().unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools = response["result"]["tools"].as_array().unwrap();
    // The full in-scope agent surface is advertised (MCP-03).
    assert_eq!(tools.len(), registry::IN_SCOPE_CAPABILITIES.len());
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"ishoo_status"));
    assert!(names.contains(&"ishoo_new"));
    assert!(names.contains(&"ishoo_hero_signal"));
    assert!(names.contains(&"ishoo_done"));
    // DEC-86: the land/finish aliases are dropped from the MCP surface.
    assert!(!names.contains(&"ishoo_land"));
    assert!(!names.contains(&"ishoo_finish"));
    for gone in PRODUCT_DOMAIN_TOOLS {
        assert!(
            !names.contains(gone),
            "{gone} should be deferred off the MCP surface"
        );
    }
    // Every advertised tool carries an object input schema.
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
}

#[test]
fn every_registered_tool_now_has_a_handler() {
    // After MCP-06 the whole in-scope surface is implemented — no pending tools.
    for tool in registry::registry() {
        assert!(
            tool.handler.is_some(),
            "tool {} still has no handler",
            tool.name
        );
    }
}

#[test]
fn every_tool_name_follows_the_naming_convention() {
    // DEC-86 (supersedes DEC-56): issue is the implicit default domain — issue
    // CRUD/lifecycle are bare verbs; workspace reads are nouns; a secondary entity
    // is one op-dispatched bare token (ishoo_plan {op}, …). The remaining
    // ishoo_<entity>_<verb> prefixes are still valid while their consolidation
    // children (MCP-58..61) are pending. Adding a tool must add a bare token or an
    // entity prefix here — a conscious naming decision — so the surface can't drift.
    const ENTITY_PREFIXES: &[&str] = &[
        "milestone_",
        "epic_",
        "roadmap_",
        "version_",
        "people_",
        "hero_",
    ];
    const BARE_OK: &[&str] = &[
        // op-dispatched entity tools (DEC-86)
        "plan",
        "decision",
        "comment",
        "admin",
        // workspace-level reads (nouns)
        "status",
        "brief",
        "candidates",
        // issue CRUD
        "show",
        "list",
        "new",
        "decompose",
        "edit",
        "resolve",
        "delete",
        "decline",
        "supersede",
        "rename_id",
        "shelve",
        // issue lifecycle
        "set_active",
        "start",
        "done",
    ];
    for tool in registry::registry() {
        let rest = tool
            .name
            .strip_prefix("ishoo_")
            .unwrap_or_else(|| panic!("tool {} must start with ishoo_", tool.name));
        let conforms =
            BARE_OK.contains(&rest) || ENTITY_PREFIXES.iter().any(|p| rest.starts_with(p));
        assert!(
            conforms,
            "tool {} violates the DEC-56 naming convention (add a bare token or an entity prefix)",
            tool.name
        );
    }
}

#[test]
fn cli_capability_inventory_covers_every_root_cli_command() {
    let cli_commands: BTreeSet<String> = crate::main_cli::Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_string())
        .collect();

    let mut inventory_commands = BTreeSet::new();
    for entry in registry::cli_capability_inventory() {
        assert!(
            inventory_commands.insert(entry.command.to_string()),
            "duplicate CLI capability classification for {}",
            entry.command
        );
    }

    assert_eq!(
        inventory_commands, cli_commands,
        "every root CLI command must have exactly one MCP parity classification"
    );
}

#[test]
fn agent_required_cli_capabilities_are_covered_or_tracked() {
    let registered_tools: HashSet<&str> =
        registry::registry().iter().map(|tool| tool.name).collect();
    let mut untracked_agent_gaps = Vec::new();

    for entry in registry::cli_capability_inventory() {
        assert!(
            !entry.rationale.trim().is_empty(),
            "{} must explain its classification",
            entry.command
        );
        for tool in entry.mcp_tools {
            assert!(
                registered_tools.contains(tool),
                "{} references unregistered MCP tool {}",
                entry.command,
                tool
            );
        }
        for issue in entry.follow_up_issues {
            assert!(
                issue.contains('-'),
                "{} follow-up issue {} should be an Ishoo id",
                entry.command,
                issue
            );
        }
        if entry.class == registry::CliCapabilityClass::AgentRequired
            && entry.mcp_tools.is_empty()
            && entry.follow_up_issues.is_empty()
        {
            untracked_agent_gaps.push(entry.command);
        }
    }

    assert!(
        untracked_agent_gaps.is_empty(),
        "agent-required CLI commands need MCP coverage or follow-up issues: {:?}",
        untracked_agent_gaps
    );
}

#[test]
fn tools_call_status_returns_structured_facts() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ishoo_status","arguments":{}}}"#,
    );
    let result = &response["result"];
    assert_eq!(result["isError"], false);
    // The text content block must carry the serialized report.
    assert_eq!(result["content"][0]["type"], "text");
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("active_plan"));
    // Structured content exposes the typed fields the agent branches on.
    let structured = &result["structuredContent"];
    assert!(structured["root"].is_string());
    assert!(structured["store_ok"].is_boolean());
    assert!(structured["active_plan"].is_string());
    assert!(structured["recommended_next"].is_string());
    assert!(
        structured.get("mcp_owner").is_none(),
        "plain in-process status must not report an MCP owner failure it did not observe"
    );
}

#[test]
fn tools_call_unknown_tool_is_invalid_params() {
    let dir = tempdir().unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ishoo_nope"}}"#,
    );
    assert_eq!(response["error"]["code"], INVALID_PARAMS);
}

#[test]
fn malformed_frame_returns_parse_error_without_panicking() {
    let dir = tempdir().unwrap();
    let response = respond(dir.path(), "{ this is not json");
    assert_eq!(response["error"]["code"], PARSE_ERROR);
    assert!(response["id"].is_null());
}

#[test]
fn notification_without_id_gets_no_response() {
    let dir = tempdir().unwrap();
    assert!(handle_line(
        dir.path(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    )
    .is_none());
}

#[test]
fn unknown_request_method_is_method_not_found() {
    let dir = tempdir().unwrap();
    let response = respond(
        dir.path(),
        r#"{"jsonrpc":"2.0","id":5,"method":"frobnicate"}"#,
    );
    assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
}

#[test]
fn ping_returns_empty_result() {
    let dir = tempdir().unwrap();
    let response = respond(dir.path(), r#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#);
    assert!(response["result"].is_object());
    assert!(response.get("error").is_none());
}

fn recv_completed(
    rx: &std::sync::mpsc::Receiver<ServerEvent>,
    timeout: std::time::Duration,
) -> Value {
    match rx.recv_timeout(timeout).expect("expected MCP response") {
        ServerEvent::Completed(Some(line)) => {
            serde_json::from_str(&line).expect("response must be valid JSON")
        }
        ServerEvent::Completed(None) => panic!("expected request response, got notification"),
        ServerEvent::Line(_) | ServerEvent::InputClosed | ServerEvent::ParentGone => {
            panic!("response channel should only receive completed requests in this test")
        }
    }
}

struct EnvGuard(&'static str);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

static TIMING_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

fn timing_test_guard() -> std::sync::MutexGuard<'static, ()> {
    TIMING_TEST_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn shutdown_drain_is_configurable_and_not_one_second_by_default() {
    let _timing_guard = timing_test_guard();
    std::env::remove_var("ISHOO_MCP_SHUTDOWN_DRAIN_MS");
    assert!(shutdown_drain() > std::time::Duration::from_secs(1));

    std::env::set_var("ISHOO_MCP_SHUTDOWN_DRAIN_MS", "75");
    let _drain_guard = EnvGuard("ISHOO_MCP_SHUTDOWN_DRAIN_MS");
    assert_eq!(shutdown_drain(), std::time::Duration::from_millis(75));
}

#[cfg(unix)]
#[test]
fn parent_disappeared_when_ppid_changes_or_reparents_to_init() {
    assert!(!parent_disappeared(42, 42));
    assert!(parent_disappeared(42, 43));
    assert!(parent_disappeared(42, 1));
}

#[cfg(unix)]
#[test]
fn parent_watchdog_reports_parent_disappearance_without_stdin_eof() {
    let _timing_guard = timing_test_guard();
    let (tx, rx) = std::sync::mpsc::channel();
    let current_parent = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(42));
    let probe = current_parent.clone();
    spawn_parent_watchdog_with(tx, std::time::Duration::from_millis(10), 42, move || {
        probe.load(std::sync::atomic::Ordering::SeqCst) as libc::pid_t
    });

    assert!(
        matches!(
            rx.recv_timeout(std::time::Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "watchdog must not fire while the original parent is still current"
    );

    current_parent.store(43, std::sync::atomic::Ordering::SeqCst);
    match rx.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(ServerEvent::ParentGone) => {}
        Ok(other) => panic!("expected ParentGone, got {other:?}"),
        Err(error) => panic!("watchdog did not report parent disappearance: {error}"),
    }
}

#[test]
fn async_dispatch_keeps_reads_moving_while_mutation_is_wedged() {
    let _timing_guard = timing_test_guard();
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let path = std::sync::Arc::new(dir.path().to_path_buf());
    let (tx, rx) = std::sync::mpsc::channel();
    let dispatch = Dispatch::new(path, tx, None);
    let slow_new = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "ishoo_new",
            "arguments": {
                "title": "Slow mutation",
                "category": "fix",
                "plan": "new:\"Work\"",
                "concrete_change": "a",
                "main_surface": "b",
                "proof_of_done": "c",
                "out_of_scope": "d",
                "decisions": [],
                "depends_on": []
            }
        }
    });

    std::env::set_var("ISHOO_MCP_NEW_RESPONSE_DELAY_MS", "700");
    let _delay_guard = EnvGuard("ISHOO_MCP_NEW_RESPONSE_DELAY_MS");
    dispatch.dispatch(slow_new.to_string());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let loaded = crate::model::Workspace::load(dir.path()).unwrap();
        if loaded.issues.iter().any(|issue| issue.id == "FIX-01") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "slow mutation never wrote its local record"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    dispatch.dispatch(
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "ishoo_status", "arguments": {} }
        })
        .to_string(),
    );
    dispatch.dispatch(
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "ishoo_plan", "arguments": { "op": "list" } }
        })
        .to_string(),
    );

    let first_read = recv_completed(&rx, std::time::Duration::from_millis(500));
    let second_read = recv_completed(&rx, std::time::Duration::from_millis(500));
    let read_ids = [
        first_read["id"].as_i64().unwrap(),
        second_read["id"].as_i64().unwrap(),
    ];
    assert!(
        read_ids.contains(&2) && read_ids.contains(&3),
        "read-only tools should answer before the delayed mutation; got ids {read_ids:?}"
    );
    for response in [&first_read, &second_read] {
        assert_eq!(
            response["result"]["isError"], false,
            "response: {response:#?}"
        );
    }

    let slow = recv_completed(&rx, std::time::Duration::from_secs(2));
    assert_eq!(slow["id"], 1);
    assert_eq!(
        slow["result"]["structuredContent"]["id"], "FIX-01",
        "slow mutation should still complete normally"
    );
}

// FIX-124 (DEC-77): pipelined dependent mutations must apply in strict arrival
// order. The old design spawned each mutation on its own thread racing for a
// global lock — exclusion without ordering — so a later mutation could win and
// run first (the reported ishoo_done-before-ishoo_resolve block). The single
// FIFO worker makes that reordering unrepresentable; this proves it across
// enough iterations that a race regression is caught with near-certainty.
#[test]
fn pipelined_dependent_mutations_apply_in_arrival_order() {
    let _timing_guard = timing_test_guard();
    for iteration in 0..20 {
        let dir = tempdir().unwrap();
        crate::model::init_workspace(dir.path()).unwrap();
        let path = std::sync::Arc::new(dir.path().to_path_buf());
        let (tx, rx) = std::sync::mpsc::channel();
        let dispatch = Dispatch::new(path, tx, None);

        // First mutation creates FIX-01; the second edits FIX-01. The edit can
        // only succeed if the create already applied — the same dependency as
        // resolve->done. Both are dispatched back-to-back with no awaiting.
        let create = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "ishoo_new", "arguments": {
                "title": "Created first", "category": "fix", "plan": "new:\"Work\"",
                "concrete_change": "a", "main_surface": "b", "proof_of_done": "c",
                "out_of_scope": "d", "decisions": [], "depends_on": []
            }}
        });
        let edit = json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "ishoo_edit", "arguments": {
                "id": "FIX-01", "title": "Edited second"
            }}
        });
        dispatch.dispatch(create.to_string());
        dispatch.dispatch(edit.to_string());

        // The single worker emits responses in execution order.
        let first = recv_completed(&rx, std::time::Duration::from_secs(2));
        let second = recv_completed(&rx, std::time::Duration::from_secs(2));
        assert_eq!(
            first["id"], 1,
            "iteration {iteration}: create must complete before edit"
        );
        assert_eq!(
            second["id"], 2,
            "iteration {iteration}: edit completes second"
        );
        assert_eq!(
            second["result"]["isError"], false,
            "iteration {iteration}: edit must succeed because the create applied first; got {second:#?}"
        );
    }
}

/// Issue a `tools/call` and return the parsed response.
fn call_tool(path: &std::path::Path, name: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0", "id": 99, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    respond(path, &request.to_string())
}

/// FIX-122 (DEC-77): probing a tool against a repo with no initialized `.ishoo`
/// store must return a structured tool error — naming the missing-store condition
/// and the `ishoo init` recovery — and must NOT exit the process or close the
/// stdio transport. Before the fix, `ishoo_status` reached the CLI's
/// `load_workspace`, which calls `process::exit(1)` on a missing store and would
/// take the whole MCP server down (read as a dead transport). Reaching the
/// assertions at all proves no exit occurred; the follow-up call proves the
/// transport is still alive and the error path is non-destructive (idempotent).
#[test]
fn status_on_uninitialized_store_errors_without_killing_the_transport() {
    let dir = tempdir().unwrap();
    // Deliberately NOT init_workspace: the canonical store does not exist.
    let response = call_tool(dir.path(), "ishoo_status", json!({}));
    let message = response["error"]["message"]
        .as_str()
        .expect("a missing store yields a structured tool error, not a panic/exit");
    assert!(
        message.contains("No canonical store found"),
        "error must name the missing-store path/condition: {message}"
    );
    assert!(
        message.contains("ishoo init"),
        "error must carry the `ishoo init` recovery: {message}"
    );

    // The transport stays alive: a following call on the same path still works,
    // and the same probe is repeatable (no corruption, no half-dead server).
    let again = call_tool(dir.path(), "ishoo_status", json!({}));
    assert!(
        again["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("ishoo init")),
        "a repeated probe must still return the structured error: {again}"
    );
}

/// DX-01: a repo can be explicitly Ishoo-enabled for agents (`.mcp.json` plus
/// managed agent instructions) before the canonical store exists. In that case
/// the MCP error must identify the bootstrap gap and name the nested init path,
/// not leave the agent to infer it from the generic missing-store string.
#[test]
fn configured_repo_without_store_names_nested_init_recovery() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    fs::write(
        dir.path().join(".mcp.json"),
        r#"{"mcpServers":{"ishoo":{"command":"ishoo","args":["mcp"]}}}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("AGENTS.md"),
        "Before handling the first user request, call the ishoo_brief MCP tool.\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("docs/issues")).unwrap();
    let discovered = crate::model::discover_root(dir.path());

    let response = call_tool(&discovered, "ishoo_status", json!({}));
    let message = response["error"]["message"]
        .as_str()
        .expect("configured repo without store yields a structured tool error");
    assert!(
        message.contains("already Ishoo-configured"),
        "error must classify the repo-bootstrap trap: {message}"
    );
    assert!(
        message.contains(&discovered.join(".ishoo").display().to_string()),
        "error must name the expected nested store path: {message}"
    );
    assert!(
        message.contains("ishoo init --path"),
        "error must carry the nested init recovery: {message}"
    );
}

#[test]
fn status_structured_content_carries_host_readiness_facts() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    crate::model::init_workspace(dir.path()).unwrap();

    let response = call_tool(dir.path(), "ishoo_status", json!({}));
    let hosts = response["result"]["structuredContent"]["host_readiness"]
        .as_array()
        .expect("status structured content includes host readiness facts");
    assert_eq!(hosts.len(), 2);
    assert!(hosts.iter().any(|h| h["host"] == "Claude Code"));
    assert!(hosts.iter().any(|h| h["host"] == "Codex"));
    for host in hosts {
        assert!(host["user_registration"]["state"].is_string());
        assert!(host["repository_adapter"]["state"].is_string());
        assert!(host["effective_source"].is_string());
        assert_eq!(host["connectivity"], "unchecked");
    }
}

#[test]
fn new_assembles_the_scope_contract_and_creates_the_issue() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let response = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Sample task",
            "category": "fix",
            "plan": "new:\"Work\"",
            "concrete_change": "do x",
            "main_surface": "src/y.rs",
            "proof_of_done": "tests pass",
            "out_of_scope": "z",
            "decisions": [],
            "depends_on": []
        }),
    );
    let id = response["result"]["structuredContent"]["id"]
        .as_str()
        .expect("created id");
    assert_eq!(id, "FIX-01");
    let assessment = &response["result"]["structuredContent"]["urgency_assessment"];
    assert_eq!(assessment["status"], "not_set");
    let guidance = assessment["guidance"].as_str().expect("guidance");
    assert!(guidance.contains("urgent interrupts across plans"));
    assert!(guidance.contains("important is high-value active-plan work"));
    assert!(guidance.contains("mid is normal work"));
    assert!(guidance.contains("later is deferred"));
    assert!(guidance.contains("shelved is retained knowledge"));

    // `ishoo show`'s core fn renders the assembled contract from the stored issue.
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert!(issue.description.contains("**Concrete change:** do x"));
    assert!(issue.description.contains("**Out of scope:** z"));
}

#[test]
fn new_accepts_the_none_sentinel_for_decisions_and_depends_on() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    // The brief documents `none` as the way to assert "no ADR / no blocker", and the
    // CLI accepts `--decisions none` / `--depends-on none`. The array sentinel
    // `["none"]` (any case) must work over MCP too instead of being read as a literal
    // ref and rejected with "Invalid depends_on ref 'none'".
    let response = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Sentinel task",
            "category": "fix",
            "plan": "new:\"Work\"",
            "concrete_change": "do x",
            "main_surface": "src/y.rs",
            "proof_of_done": "tests pass",
            "out_of_scope": "z",
            "decisions": ["none"],
            "depends_on": ["None"]
        }),
    );
    assert!(
        response.get("error").is_none(),
        "the documented none sentinel must not error: {response}"
    );
    let id = response["result"]["structuredContent"]["id"]
        .as_str()
        .expect("created id");
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == id).unwrap();
    assert!(
        issue.depends_on.is_empty(),
        "['None'] depends_on resolves to no blockers, got {:?}",
        issue.depends_on
    );
    assert!(
        issue.decision_refs.is_empty(),
        "['none'] decisions resolves to no ADRs, got {:?}",
        issue.decision_refs
    );
}

#[test]
fn new_accepts_labels_and_files_at_creation_and_strips_unknown_labels() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let response = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Labelled task",
            "category": "fix",
            "plan": "new:\"Work\"",
            "concrete_change": "do x",
            "main_surface": "src/y.rs",
            "proof_of_done": "tests pass",
            "out_of_scope": "z",
            "decisions": [],
            "depends_on": [],
            "labels": ["ux", "feat", "totallybogus"],
            "files": ["src/a.rs", "src/b.rs"]
        }),
    );
    let id = response["result"]["structuredContent"]["id"]
        .as_str()
        .expect("created id");
    // The unknown label is stripped and reported in the result (DEC-25).
    let stripped = response["result"]["structuredContent"]["stripped_labels"]
        .as_array()
        .expect("stripped_labels reported");
    assert_eq!(stripped, &[json!("totallybogus")]);

    // The issue is born complete — labels (canonical only) and files, no second edit.
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == id).unwrap();
    assert_eq!(issue.labels, vec!["ux".to_string(), "feat".to_string()]);
    assert_eq!(
        issue.files,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );
}

#[test]
fn preflight_returns_typed_readiness_and_rejects_unknown_id() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let created = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Preflight task", "category": "fix", "plan": "new:\"Work\"",
            "concrete_change": "x", "main_surface": "y", "proof_of_done": "z",
            "out_of_scope": "w", "decisions": [], "depends_on": []
        }),
    );
    let id = created["result"]["structuredContent"]["id"]
        .as_str()
        .expect("created id")
        .to_string();

    // A backlog/unclaimed issue is not ready to land; the typed report names the
    // worst blocker and a recommended next command.
    let report = call_tool(
        dir.path(),
        "ishoo_admin",
        json!({ "op": "preflight", "id": id }),
    );
    let result = &report["result"]["structuredContent"];
    assert_eq!(result["id"].as_str().unwrap(), id);
    assert_eq!(result["readiness"], json!("not_claimed_here"));
    assert!(result["recommended_next"]
        .as_str()
        .unwrap()
        .contains(&format!("ishoo start {id}")));

    // Unknown id is an error.
    let unknown = call_tool(
        dir.path(),
        "ishoo_admin",
        json!({ "op": "preflight", "id": "NOPE-99" }),
    );
    assert_eq!(unknown["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn lint_returns_structured_findings_for_clean_and_broken_stores() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    // Seed one valid issue.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Valid", "category": "fix", "plan": "new:\"Work\"",
            "concrete_change": "x", "main_surface": "y", "proof_of_done": "z",
            "out_of_scope": "w", "decisions": [], "depends_on": []
        }),
    );

    // Clean store → ok, no findings.
    let clean = call_tool(dir.path(), "ishoo_admin", json!({ "op": "lint",}));
    let result = &clean["result"]["structuredContent"];
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["count"], json!(0));

    // Introduce a broken decision ref directly in the store (ishoo_new would reject it).
    let mut workspace = crate::model::Workspace::load(dir.path()).unwrap();
    workspace.issues[0].decision_refs = vec!["DEC-999".to_string()];
    workspace.save().unwrap();

    let broken = call_tool(dir.path(), "ishoo_admin", json!({ "op": "lint",}));
    let result = &broken["result"]["structuredContent"];
    assert_eq!(result["ok"], json!(false));
    assert!(result["count"].as_u64().unwrap() >= 1);
    let findings = result["findings"].as_array().expect("findings array");
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap_or("").contains("DEC-999")),
        "the broken decision ref is reported: {findings:?}"
    );
}

#[test]
fn new_rejects_a_missing_contract_field() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    // Omit out_of_scope.
    let response = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Sample",
            "plan": "new:\"Work\"",
            "concrete_change": "x",
            "main_surface": "y",
            "proof_of_done": "z",
            "decisions": [],
            "depends_on": []
        }),
    );
    assert_eq!(response["error"]["code"], registry::INVALID_PARAMS);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("out_of_scope"));
}

#[test]
fn new_preserves_multiline_text_with_shell_metacharacters_byte_exact() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let tricky = "line one\nwith `backticks` and $(echo hi)\n- a bullet";
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Tricky",
            "category": "fix",
            "plan": "new:\"Work\"",
            "concrete_change": tricky,
            "main_surface": "y",
            "proof_of_done": "z",
            "out_of_scope": "w",
            "decisions": [],
            "depends_on": []
        }),
    );
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    // The exact bytes survive — no shell, no escaping mangling.
    assert!(issue.description.contains(tricky));
}

#[test]
fn resolve_assembles_and_writes_the_resolution_contract() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Sample", "category": "fix", "plan": "new:\"Work\"",
            "concrete_change": "x", "main_surface": "y", "proof_of_done": "z",
            "out_of_scope": "w", "decisions": [], "depends_on": []
        }),
    );
    let response = call_tool(
        dir.path(),
        "ishoo_resolve",
        json!({
            "id": "FIX-01",
            "what_changed": "did the thing",
            "why": "because",
            "verification": "ran `cargo test`",
            "handoff": "none"
        }),
    );
    assert_eq!(response["result"]["structuredContent"]["id"], "FIX-01");
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert!(issue.resolution.contains("**What changed:** did the thing"));
    assert!(issue
        .resolution
        .contains("**Verification:** ran `cargo test`"));
    // A complete contract was written.
    assert!(crate::model::validate_resolution_contract(&issue.resolution).complete);
}

/// Create one issue in a fresh **git-backed** workspace and return its dir. The
/// git init keeps `find_repo_root` inside the tempdir (the test harness's TMPDIR
/// can otherwise sit under the real repo), so the land gates measure this
/// workspace, not the host repo.
fn workspace_with_issue() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Sample", "category": "fix", "plan": "new:\"Work\"",
            "concrete_change": "x", "main_surface": "y", "proof_of_done": "z",
            "out_of_scope": "w", "decisions": [], "depends_on": []
        }),
    );
    dir
}

#[test]
fn show_returns_the_typed_issue_view() {
    let dir = workspace_with_issue();
    let response = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    let view = &response["result"]["structuredContent"];
    assert_eq!(view["issue"]["id"], "FIX-01");
    assert_eq!(view["scope_complete"], true);
    assert_eq!(view["resolution_complete"], false);
    assert!(view["issue"].get("source_file").is_none());
    assert_eq!(
        view["issue"]["export_metadata"]["document"],
        "issues-active.md"
    );
    assert_eq!(
        view["issue"]["export_metadata"]["export_path"],
        "docs/issues/issues-active.md"
    );
    assert_eq!(view["issue"]["export_metadata"]["freshness"], "unknown");
    assert_eq!(view["issue"]["export_metadata"]["authoritative"], false);
}

#[test]
fn show_unknown_issue_errors() {
    let dir = workspace_with_issue();
    let response = call_tool(dir.path(), "ishoo_show", json!({ "id": "NOPE-9" }));
    assert_eq!(response["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn list_returns_stats_and_issues_matching_cli_facts() {
    let dir = workspace_with_issue();
    let response = call_tool(dir.path(), "ishoo_list", json!({}));
    let list = &response["result"]["structuredContent"];
    // Stats mirror the CLI's `ishoo list` header for the same store state.
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let stats = workspace.stats();
    assert_eq!(list["stats"]["total"], stats.total);
    assert_eq!(list["stats"]["backlog"], stats.backlog);
    assert_eq!(list["issues"][0]["id"], "FIX-01");
    assert!(list["issues"][0].get("source_file").is_none());
    assert_eq!(list["issues"][0]["export_metadata"]["freshness"], "unknown");
    assert_eq!(list["issues"][0]["export_metadata"]["authoritative"], false);
}

#[test]
fn list_filters_by_status() {
    let dir = workspace_with_issue();
    // The only issue is backlog; filtering for active yields none.
    let response = call_tool(dir.path(), "ishoo_list", json!({ "status": "active" }));
    let issues = response["result"]["structuredContent"]["issues"]
        .as_array()
        .unwrap();
    assert!(issues.is_empty());
}

// MCP-39: the compact projection returns bounded rows (no full records), paginates,
// and honors the richer filters — so an agent can audit a large store without truncation.
#[test]
fn list_compact_paginates_filters_and_projects_bounded_rows() {
    use crate::model::{Issue, Status, Workspace};
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let mut ws = Workspace::load(dir.path()).unwrap();
    for (id, status) in [
        ("FIX-01", Status::Active),
        ("FIX-02", Status::Backlog),
        ("MCP-01", Status::Backlog),
    ] {
        ws.issues.push(Issue {
            id: id.to_string(),
            title: format!("title {id}"),
            status,
            ..Issue::default()
        });
    }
    ws.save().unwrap();

    // Page 1 of 2: bounded rows, correct totals, and NO full-record fields.
    let page1 = call_tool(
        dir.path(),
        "ishoo_list",
        json!({ "compact": true, "limit": 2 }),
    );
    let c = &page1["result"]["structuredContent"];
    assert_eq!(c["total"], 3);
    assert_eq!(c["returned"], 2);
    assert_eq!(c["issues"].as_array().unwrap().len(), 2);
    assert!(c["issues"][0]["id"].is_string());
    assert!(c["issues"][0]["plan"].is_string());
    assert!(
        c["issues"][0].get("description").is_none(),
        "compact rows must not carry full-record fields"
    );

    // Pagination: offset past the first page returns the remainder.
    let page2 = call_tool(
        dir.path(),
        "ishoo_list",
        json!({ "compact": true, "limit": 2, "offset": 2 }),
    );
    assert_eq!(page2["result"]["structuredContent"]["returned"], 1);

    // Prefix filter narrows to one category.
    let mcp_only = call_tool(
        dir.path(),
        "ishoo_list",
        json!({ "compact": true, "prefix": "MCP" }),
    );
    let m = &mcp_only["result"]["structuredContent"];
    assert_eq!(m["total"], 1);
    assert_eq!(m["issues"][0]["id"], "MCP-01");
}

#[test]
fn plan_next_returns_the_ready_item_then_null() {
    let dir = workspace_with_issue();
    // The created issue is the active plan's ready front.
    let response = call_tool(dir.path(), "ishoo_plan", json!({ "op": "next",}));
    let next = &response["result"]["structuredContent"];
    assert_eq!(next["issue_id"], "FIX-01");

    // A fresh, empty workspace has no ready item -> null.
    let empty = tempdir().unwrap();
    crate::model::init_workspace(empty.path()).unwrap();
    let response = call_tool(empty.path(), "ishoo_plan", json!({ "op": "next",}));
    assert!(response["result"]["structuredContent"].is_null());
}

#[test]
fn home_hero_signal_record_persists_typed_expiring_activity() {
    let dir = workspace_with_issue();
    let response = call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-1",
            "kind": "working",
            "source": "codex",
            "ttl_secs": 60
        }),
    );
    let signal = &response["result"]["structuredContent"];

    assert_eq!(signal["recorded"]["operation_id"], "op-1");
    assert_eq!(signal["recorded"]["kind"], "working");
    assert_eq!(signal["recorded"]["source"], "codex");
    assert!(signal["recorded"]["recorded_at"]
        .as_str()
        .unwrap()
        .contains('T'));
    assert!(signal["recorded"]["expires_at"]
        .as_str()
        .unwrap()
        .contains('T'));
    assert_eq!(signal["contributes_state"], "active");
    assert_eq!(signal["active_signals"].as_array().unwrap().len(), 1);

    let store = crate::model::project_store::ProjectStore::load(dir.path()).unwrap();
    assert_eq!(store.home_hero_activity.entries.len(), 1);
    assert_eq!(
        store.home_hero_activity.entries[0].kind,
        crate::model::project_store::HomeHeroActivitySignalKind::Working
    );
}

#[test]
fn home_hero_signal_record_replaces_repeated_operation_id() {
    let dir = workspace_with_issue();
    call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-1",
            "kind": "working",
            "source": "codex"
        }),
    );
    let response = call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-1",
            "kind": "running_checks",
            "source": "codex"
        }),
    );
    let signal = &response["result"]["structuredContent"];

    assert_eq!(signal["replaced"], true);
    assert_eq!(signal["active_signals"].as_array().unwrap().len(), 1);
    assert_eq!(signal["active_signals"][0]["kind"], "running_checks");

    let store = crate::model::project_store::ProjectStore::load(dir.path()).unwrap();
    assert_eq!(store.home_hero_activity.entries.len(), 1);
    assert_eq!(
        store.home_hero_activity.entries[0].kind,
        crate::model::project_store::HomeHeroActivitySignalKind::RunningChecks
    );
}

#[test]
fn home_hero_signal_record_prunes_expired_signals() {
    let dir = workspace_with_issue();
    crate::model::project_store::ProjectStore::update(dir.path(), |store| {
        store.home_hero_activity.entries.push(
            crate::model::project_store::HomeHeroActivitySignalRecord {
                operation_id: "old-op".to_string(),
                kind: crate::model::project_store::HomeHeroActivitySignalKind::WaitingOnTool,
                source: "codex".to_string(),
                recorded_at: "2026-01-01T00:00:00Z".to_string(),
                expires_at: "2026-01-01T00:00:01Z".to_string(),
            },
        );
        Ok(())
    })
    .unwrap();

    let response = call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-2",
            "kind": "handoff_ready",
            "source": "codex"
        }),
    );
    let signal = &response["result"]["structuredContent"];

    assert_eq!(signal["pruned_expired"], 1);
    assert_eq!(signal["active_signals"].as_array().unwrap().len(), 1);
    assert_eq!(signal["active_signals"][0]["operation_id"], "op-2");
}

#[test]
fn home_hero_signal_record_rejects_display_text_and_attention() {
    let dir = workspace_with_issue();
    let prose = call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-1",
            "kind": "working",
            "source": "codex",
            "display_text": "Running the tests"
        }),
    );
    assert!(prose["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unsupported field(s): display_text"));

    let attention = call_tool(
        dir.path(),
        "ishoo_hero_signal",
        json!({
            "operation_id": "op-1",
            "kind": "attention",
            "source": "codex"
        }),
    );
    assert!(attention["error"]["message"]
        .as_str()
        .unwrap()
        .contains("invalid signal kind"));
}

#[test]
fn resolve_rejects_a_missing_contract_field() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let response = call_tool(
        dir.path(),
        "ishoo_resolve",
        json!({ "id": "FIX-01", "what_changed": "x", "why": "y", "verification": "z" }),
    );
    assert_eq!(response["error"]["code"], registry::INVALID_PARAMS);
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("handoff"),
        "must name the missing field: {message}"
    );
    // The error must also report the keys it *did* receive, so a field dropped
    // in client serialization/transport (absent here despite being sent) is
    // self-evident rather than looking like a forgotten argument.
    assert!(
        message.contains("received keys:")
            && message.contains("id")
            && message.contains("what_changed")
            && message.contains("why")
            && message.contains("verification"),
        "must list the received keys: {message}"
    );
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(unix)]
fn install_slow_receive_hook(bare: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let script = bare.join("hooks/pre-receive");
    std::fs::write(&script, "#!/bin/sh\nsleep 1\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
}

fn store_ref(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "refs/ishoo/store"])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn startup_store_sync_fetches_remote_store_before_status_reads() {
    let outer = tempdir().unwrap();
    let origin = outer.path().join("origin.git");
    let a = outer.path().join("a");
    let b = outer.path().join("b");

    let init_origin = std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&origin)
        .output()
        .unwrap();
    assert!(init_origin.status.success());

    std::fs::create_dir_all(&a).unwrap();
    git(&a, &["init", "-q"]);
    git(&a, &["branch", "-M", "main"]);
    git(&a, &["config", "user.email", "t@t"]);
    git(&a, &["config", "user.name", "t"]);
    crate::model::init_workspace(&a).unwrap();
    git(&a, &["add", "-A"]);
    git(&a, &["commit", "-qm", "init", "--allow-empty"]);
    git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&a, &["push", "-u", "origin", "main"]);

    call_tool(
        &a,
        "ishoo_new",
        json!({
            "title": "Remote store task", "category": "fix", "plan": "new:\"W\"",
            "concrete_change": "a", "main_surface": "b", "proof_of_done": "c",
            "out_of_scope": "d", "decisions": [], "depends_on": []
        }),
    );
    assert!(
        matches!(
            crate::model::git_remote::sync_store_ref(&a).unwrap(),
            crate::model::git_remote::StoreSyncOutcome::Pushed
                | crate::model::git_remote::StoreSyncOutcome::UpToDate
        ),
        "clone A should publish its store ref"
    );

    let clone = std::process::Command::new("git")
        .arg("clone")
        .arg(&origin)
        .arg(&b)
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    git(&b, &["config", "user.email", "t@t"]);
    git(&b, &["config", "user.name", "t"]);

    super::run_startup_store_sync(&b);
    let status = call_tool(&b, "ishoo_status", json!({}));
    let startup = &status["result"]["structuredContent"]["mcp_startup_store_sync"];
    assert_eq!(startup["store_ref"], crate::model::git_remote::STORE_REF);
    assert_eq!(startup["state"], "pulled");

    let read = call_tool(&b, "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(read["result"]["structuredContent"]["issue"]["id"], "FIX-01");
}

#[test]
fn a_mutating_tool_snapshots_the_store_ref() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);

    // No store ref before any mutation (FIX-76 regression target).
    assert!(store_ref(dir.path()).is_none());

    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "T", "category": "fix", "plan": "new:\"W\"",
            "concrete_change": "a", "main_surface": "b", "proof_of_done": "c",
            "out_of_scope": "d", "decisions": [], "depends_on": []
        }),
    );

    // The mutating MCP call must have snapshotted the store to refs/ishoo/store.
    assert!(
        store_ref(dir.path()).is_some(),
        "ishoo_new via MCP did not advance refs/ishoo/store"
    );
}

#[test]
fn socket_transport_forwards_tool_calls_to_the_resident_owner() {
    let _timing_guard = timing_test_guard();
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);

    let endpoint = super::transport::start_owner_thread_for_tests(dir.path().to_path_buf())
        .expect("test owner socket");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "ishoo_new",
            "arguments": {
                "title": "Socket mutation",
                "category": "fix",
                "plan": "new:\"W\"",
                "concrete_change": "a",
                "main_surface": "b",
                "proof_of_done": "c",
                "out_of_scope": "d",
                "decisions": [],
                "depends_on": []
            }
        }
    });

    let line = super::transport::send_line(&endpoint, &request.to_string())
        .expect("socket call succeeds")
        .expect("tools/call returns a response");
    let response: Value = serde_json::from_str(&line).expect("response is JSON");
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["structuredContent"]["id"], "FIX-01");
    assert_eq!(
        response["result"]["structuredContent"]["publish"]["store_ref"],
        crate::model::git_remote::STORE_REF
    );
    assert!(
        store_ref(dir.path()).is_some(),
        "resident owner socket mutation must snapshot refs/ishoo/store"
    );

    let status_request = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "ishoo_status",
            "arguments": {}
        }
    });
    let line = super::transport::send_line(&endpoint, &status_request.to_string())
        .expect("status over owner socket succeeds")
        .expect("status returns a response");
    let status: Value = serde_json::from_str(&line).expect("status response is JSON");
    let owner = &status["result"]["structuredContent"]["owner"];
    assert_eq!(owner["state"], "running");
    assert!(
        owner["completed_mutations"].as_u64().unwrap_or(0) >= 1,
        "owner health should include mutation progress after the socket write: {owner:?}"
    );
}

// STOR-22 (DEC-83): when the resident store owner (the single writer) is unreachable,
// a store MUTATION must fail closed — a clear typed error, and nothing written behind
// the owner's back. It must never fall back to an in-process same-uid write.
#[test]
fn a_mutation_is_refused_and_writes_nothing_when_the_owner_is_unreachable() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();

    let dead = super::transport::unreachable_endpoint_for_tests();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "ishoo_new",
            "arguments": {
                "title": "should not be written",
                "category": "fix",
                "plan": "new:\"W\"",
                "concrete_change": "a",
                "main_surface": "b",
                "proof_of_done": "c",
                "out_of_scope": "d",
                "decisions": [],
                "depends_on": []
            }
        }
    });

    let line = super::handle_line_maybe_remote(dir.path(), &request.to_string(), Some(&dead))
        .expect("a request must produce a response");
    let response: Value = serde_json::from_str(&line).expect("response is JSON");

    // Fail closed: a typed service-unavailable error, correlated to the call — never a
    // success carrying a fabricated id.
    assert_eq!(response["id"], 11);
    assert_eq!(response["error"]["code"], -32010);
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("write refused"),
        "the refusal must say the write was refused: {response}"
    );
    // FIX-150 (DEC-36): the refusal must never promise a recovery that does not happen.
    assert!(
        !message.contains("restarts automatically"),
        "the refusal must not falsely claim automatic restart: {response}"
    );
    assert!(
        response.get("result").is_none(),
        "a refused write must not return a success result: {response}"
    );

    // The decisive property: nothing was written behind the owner's back.
    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    assert!(
        ws.issues.is_empty(),
        "a refused mutation must create no issue: {:?}",
        ws.issues.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
}

// STOR-22: a READ may degrade gracefully when the owner is unreachable — it serves an
// in-process read (which cannot corrupt the store) instead of failing closed, so the
// agent can still orient while writes stay strictly refused.
#[test]
fn a_read_degrades_gracefully_when_the_owner_is_unreachable() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();

    let dead = super::transport::unreachable_endpoint_for_tests();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": { "name": "ishoo_status", "arguments": {} }
    });

    let line = super::handle_line_maybe_remote(dir.path(), &request.to_string(), Some(&dead))
        .expect("a read must produce a response");
    let response: Value = serde_json::from_str(&line).expect("response is JSON");

    assert_eq!(response["id"], 12);
    assert!(
        response.get("error").is_none(),
        "a read must degrade gracefully, not fail closed: {response}"
    );
    assert!(
        response["result"].is_object(),
        "the read returns an in-process result: {response}"
    );
    let structured = &response["result"]["structuredContent"];
    assert_eq!(
        structured["mcp_owner"]["state"], "unreachable",
        "MCP status fallback must surface the owner transport failure: {response}"
    );
    assert_eq!(
        structured["mcp_owner"]["source"], "mcp_transport",
        "the degraded state is scoped to the MCP socket owner"
    );
    assert_eq!(
        structured["mcp_owner"]["write_behavior"],
        "fail_closed_or_reattach"
    );
    assert!(
        structured["mcp_owner"]["error"].is_string(),
        "the measured transport error should be included: {response}"
    );
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mcp_owner"),
        "text content must match the structured fallback status: {response}"
    );
}

// FIX-150 (DEC-53 "crashes heal"): when the owner we were using is unreachable but a
// live owner is registered (the app was restarted / a fresh writer elected), a store
// mutation must NOT wedge — it re-elects to the live owner and the write succeeds,
// rather than refusing forever.
#[test]
fn a_mutation_re_elects_to_a_live_owner_and_succeeds_when_the_prior_owner_is_gone() {
    let _timing_guard = timing_test_guard();
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);

    // A fresh, live resident owner is registered (as after an app restart).
    let live = super::transport::start_owner_thread_for_tests(dir.path().to_path_buf())
        .expect("live owner socket");
    super::transport::write_endpoint_for_tests(dir.path(), &live);

    // The endpoint this client was holding is now unreachable.
    let stale = super::transport::unreachable_endpoint_for_tests();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "tools/call",
        "params": {
            "name": "ishoo_new",
            "arguments": {
                "title": "Written after re-election",
                "category": "fix",
                "plan": "new:\"W\"",
                "concrete_change": "a",
                "main_surface": "b",
                "proof_of_done": "c",
                "out_of_scope": "d",
                "decisions": [],
                "depends_on": []
            }
        }
    });

    let line = super::handle_line_maybe_remote(dir.path(), &request.to_string(), Some(&stale))
        .expect("a request must produce a response");
    let response: Value = serde_json::from_str(&line).expect("response is JSON");

    assert_eq!(response["id"], 21);
    assert!(
        response.get("error").is_none(),
        "re-election to a live owner must let the write succeed: {response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["id"], "FIX-01",
        "the mutation must land through the re-elected owner: {response}"
    );
    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    assert_eq!(
        ws.issues.len(),
        1,
        "the issue must be written after re-election"
    );
}

// MCP-40: a store snapshot/publish failure must NOT turn into a JSON-RPC error that
// hides the created id and tempts a duplicate retry. The mutation result stays a
// success carrying the id, with a truthful `failed` durability state + recovery.
#[test]
fn mutating_result_reports_failed_durability_without_losing_the_id() {
    use crate::model::git_remote::StoreSyncOutcome;

    let mut failed = json!({ "id": "FIX-01", "status": "BACKLOG" });
    super::attach_store_sync_result(&mut failed, Err("ref update rejected".to_string()));
    assert_eq!(
        failed["id"], "FIX-01",
        "the created id must survive a durability failure"
    );
    assert_eq!(failed["status"], "BACKLOG");
    assert_eq!(failed["publish"]["state"], "failed");
    assert_eq!(failed["publish"]["reason"], "ref update rejected");
    assert!(
        failed["publish"]["recovery"]
            .as_str()
            .unwrap_or_default()
            .contains("already exists"),
        "recovery must tell the operator not to re-run (no duplicate)"
    );

    // The success path still reports the concrete sync state alongside the result.
    let mut pushed = json!({ "id": "FIX-02" });
    super::attach_store_sync_result(&mut pushed, Ok(StoreSyncOutcome::Pushed));
    assert_eq!(pushed["id"], "FIX-02");
    assert_eq!(pushed["publish"]["state"], "pushed");
}

#[cfg(unix)]
#[test]
fn mutating_tool_returns_queued_without_waiting_for_a_hung_push() {
    let _timing_guard = timing_test_guard();
    let origin = tempdir().unwrap();
    git(origin.path(), &["init", "--bare", "-q"]);
    install_slow_receive_hook(origin.path());

    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            &origin.path().display().to_string(),
        ],
    );

    // A slow/hung push must not be on the mutation's critical path at all: the MCP
    // server records locally and hands the push to a detached thread (FIX-89). Even
    // with a receive hook that sleeps, the mutation returns near-instantly as `queued`.
    let started = std::time::Instant::now();
    let response = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "T", "category": "fix", "plan": "new:\"W\"",
            "concrete_change": "a", "main_surface": "b", "proof_of_done": "c",
            "out_of_scope": "d", "decisions": [], "depends_on": []
        }),
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "MCP mutation must not wait for the (detached) push"
    );
    let structured = &response["result"]["structuredContent"];
    assert_eq!(structured["id"], "FIX-01", "response: {response:#?}");
    assert_eq!(structured["publish"]["state"], "queued");
    assert!(
        store_ref(dir.path()).is_some(),
        "local store ref must be recorded before the mutation returns"
    );

    let read = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(read["result"]["structuredContent"]["issue"]["id"], "FIX-01");
}

#[test]
fn new_retry_with_same_mutation_id_returns_existing_issue() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let args = json!({
        "mutation_id": "client-123",
        "title": "Retryable",
        "category": "fix",
        "plan": "new:\"Work\"",
        "concrete_change": "a",
        "main_surface": "b",
        "proof_of_done": "c",
        "out_of_scope": "d",
        "decisions": [],
        "depends_on": []
    });

    let first = call_tool(dir.path(), "ishoo_new", args.clone());
    let second = call_tool(dir.path(), "ishoo_new", args);

    let created = &first["result"]["structuredContent"];
    let retried = &second["result"]["structuredContent"];
    assert_eq!(created["id"], "FIX-01");
    assert_eq!(created["mutation"]["status"], "created");
    assert_eq!(created["mutation"]["id"], "client-123");
    assert_eq!(retried["id"], "FIX-01");
    assert_eq!(retried["mutation"]["status"], "already_created");

    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    assert_eq!(workspace.issues.len(), 1, "retry must not duplicate issue");
    let plans = crate::model::AllPlans::load(dir.path());
    let entries: Vec<&str> = plans
        .named
        .iter()
        .flat_map(|plan| {
            plan.plan
                .entries
                .iter()
                .map(|entry| entry.issue_id.as_str())
        })
        .collect();
    assert_eq!(entries, vec!["FIX-01"]);
}

#[test]
fn new_retry_after_response_timeout_reports_existing_issue() {
    let _timing_guard = timing_test_guard();
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let path = dir.path().to_path_buf();
    let args = json!({
        "title": "Timeout recoverable",
        "category": "fix",
        "plan": "new:\"Work\"",
        "concrete_change": "a",
        "main_surface": "b",
        "proof_of_done": "c",
        "out_of_scope": "d",
        "decisions": [],
        "depends_on": []
    });
    let first_args = args.clone();
    let (tx, rx) = std::sync::mpsc::channel();

    std::env::set_var("ISHOO_MCP_NEW_RESPONSE_DELAY_MS", "300");
    let _delay_guard = EnvGuard("ISHOO_MCP_NEW_RESPONSE_DELAY_MS");
    let worker = std::thread::spawn(move || {
        let response = call_tool(&path, "ishoo_new", first_args);
        let _ = tx.send(response);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let loaded = crate::model::Workspace::load(dir.path()).unwrap();
        if loaded.issues.iter().any(|issue| issue.id == "FIX-01") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first call did not write the issue before the timeout window"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "first call should still be delayed like a client-side timeout"
    );

    std::env::remove_var("ISHOO_MCP_NEW_RESPONSE_DELAY_MS");
    let retry = call_tool(dir.path(), "ishoo_new", args);
    let retried = &retry["result"]["structuredContent"];
    assert_eq!(retried["id"], "FIX-01");
    assert_eq!(retried["mutation"]["status"], "already_created");

    let first = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
    worker.join().unwrap();
    assert_eq!(first["result"]["structuredContent"]["id"], "FIX-01");
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    assert_eq!(
        workspace.issues.len(),
        1,
        "timeout retry must not duplicate issue"
    );
}

#[test]
fn a_read_tool_does_not_create_a_store_commit() {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);

    call_tool(dir.path(), "ishoo_list", json!({}));
    // A pure read must not snapshot the store.
    assert!(store_ref(dir.path()).is_none());
}

#[test]
fn land_blocks_on_a_missing_resolution_contract() {
    let dir = workspace_with_issue();
    // No resolution written yet -> land is BLOCKED, naming the missing contract.
    let response = call_tool(dir.path(), "ishoo_done", json!({ "id": "FIX-01" }));
    let verdict = &response["result"]["structuredContent"];
    assert_eq!(verdict["status"], "BLOCKED");
    assert_eq!(verdict["blocked"], true);
    assert_eq!(verdict["resolution_contract"]["complete"], false);
    let missing = verdict["resolution_contract"]["missing"]
        .as_array()
        .unwrap();
    assert!(missing.iter().any(|m| m == "What changed"));
}

#[test]
fn land_succeeds_when_every_gate_is_clear() {
    let dir = workspace_with_issue();
    call_tool(
        dir.path(),
        "ishoo_resolve",
        json!({
            "id": "FIX-01", "what_changed": "did it", "why": "needed",
            "verification": "tested", "handoff": "none"
        }),
    );
    // DEC-70 (KILL-02/03/04): the control surface never runs a build/test gate, so
    // completion reports the fixed advisory label.
    let response = call_tool(dir.path(), "ishoo_done", json!({ "id": "FIX-01" }));
    let verdict = &response["result"]["structuredContent"];
    assert_eq!(verdict["status"], "LANDED");
    assert_eq!(verdict["blocked"], false);
    assert!(verdict["correctness_gate"]
        .as_str()
        .unwrap()
        .contains("not run by Ishoo"));
    // The issue is now done.
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert_eq!(issue.status, crate::model::Status::Done);
}

fn completion_tool_integrates_the_worktree_commit_and_advances_main(tool_name: &str) {
    // MCP-42/MCP-43: an issue started via ishoo_start commits its fix on the
    // execution branch inside a worktree, not on main. The completion tool must
    // measure that worktree's base..HEAD (file_count > 0), fast-forward main to
    // the verified commit, and tear the worktree down.
    fn git_head(dir: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    let dir = workspace_with_issue(); // FIX-01, git repo with an init commit
    call_tool(
        dir.path(),
        "ishoo_resolve",
        json!({
            "id": "FIX-01", "what_changed": "did it", "why": "needed",
            "verification": "tested", "handoff": "none"
        }),
    );

    // Start: claim + worktree + execution branch ishoo/FIX-01, base_commit = HEAD.
    let started = call_tool(dir.path(), "ishoo_start", json!({ "id": "FIX-01" }));
    assert_eq!(started["result"]["structuredContent"]["status"], "STARTED");
    let worktree = dir.path().join(".ishoo/worktrees/FIX-01");
    assert!(
        worktree.exists(),
        "start must create the execution worktree"
    );

    let main_before = git_head(dir.path());

    // Commit a source change on the execution branch, inside the worktree.
    std::fs::write(worktree.join("change.rs"), "pub fn added() {}\n").unwrap();
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "-qm", "work on the branch"]);
    let branch_tip = git_head(&worktree);
    assert_ne!(
        branch_tip, main_before,
        "the commit is on the branch, not main"
    );

    // Land via the agent surface (DEC-70: structural gates only, no heavy gate).
    let response = call_tool(dir.path(), tool_name, json!({ "id": "FIX-01" }));
    let verdict = &response["result"]["structuredContent"];
    assert_eq!(verdict["status"], "LANDED", "verdict: {verdict:#?}");
    // The scope was measured against the worktree, so the real changed file is seen.
    assert!(
        verdict["scope"]["file_count"].as_u64().unwrap() >= 1,
        "land must measure the worktree diff, not main's empty range: {verdict:#?}"
    );
    // main was fast-forwarded to the verified commit (push deferred — no remote).
    assert!(
        verdict["integration"]
            .as_str()
            .unwrap()
            .contains("fast-forwarded"),
        "integration: {:?}",
        verdict["integration"]
    );
    assert_eq!(
        git_head(dir.path()),
        branch_tip,
        "main must advance to the branch tip"
    );

    // DEC-35: the execution substrate is torn down once the commit is on main.
    assert!(
        !worktree.exists(),
        "the worktree must be removed after landing"
    );

    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert_eq!(issue.status, crate::model::Status::Done);
}

#[test]
fn done_integrates_the_worktree_commit_and_advances_main() {
    completion_tool_integrates_the_worktree_commit_and_advances_main("ishoo_done");
}

// FIX-79 / DEC-39 / DEC-53, via the MCP completion path (model::gates::land — the
// core_fn behind ishoo_done/ishoo_land). The CLI run_completion path has its own
// divergence test (main_dispatch_workflow_tests::done_blocks_and_preserves_when_
// default_branch_diverged); gates::land had none, so its never-destroy-on-
// divergence behavior could regress (finalize a diverged issue, stranding the
// commit) with the whole suite still green. This pins it on the agent surface.
#[test]
fn done_via_mcp_blocks_and_preserves_when_default_branch_diverged() {
    fn git_head(dir: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    let dir = workspace_with_issue(); // FIX-01, git repo with an init commit
    call_tool(
        dir.path(),
        "ishoo_resolve",
        json!({
            "id": "FIX-01", "what_changed": "did it", "why": "needed",
            "verification": "tested", "handoff": "none"
        }),
    );
    let started = call_tool(dir.path(), "ishoo_start", json!({ "id": "FIX-01" }));
    assert_eq!(started["result"]["structuredContent"]["status"], "STARTED");
    let worktree = dir.path().join(".ishoo/worktrees/FIX-01");
    assert!(
        worktree.exists(),
        "start must create the execution worktree"
    );

    // The verified done-commit lands on the execution branch, inside the worktree.
    std::fs::write(worktree.join("change.rs"), "pub fn added() {}\n").unwrap();
    git(&worktree, &["add", "-A"]);
    git(&worktree, &["commit", "-qm", "work on the branch"]);
    let branch_tip = git_head(&worktree);

    // Advance the default branch past base AFTER start, in the main tree, so the
    // execution branch and the default branch genuinely diverge (neither is an
    // ancestor of the other) — a clean fast-forward is impossible.
    std::fs::write(dir.path().join("other.rs"), "pub fn other() {}\n").unwrap();
    git(dir.path(), &["add", "other.rs"]);
    git(dir.path(), &["commit", "-qm", "concurrent main work"]);
    let main_after = git_head(dir.path());
    assert_ne!(main_after, branch_tip, "main and branch must diverge");

    // Complete via the agent surface: must BLOCK, never finalize.
    let response = call_tool(dir.path(), "ishoo_done", json!({ "id": "FIX-01" }));
    let verdict = &response["result"]["structuredContent"];
    assert_eq!(
        verdict["status"], "BLOCKED",
        "done must block on a diverged default branch, not finalize: {verdict:#?}"
    );
    assert_eq!(verdict["blocked"], true);
    let reasons = verdict["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| {
            let s = r.as_str().unwrap_or("");
            s.contains("fast-forward is impossible") || s.contains("rebase")
        }),
        "the block must name the divergence/rebase recovery: {reasons:#?}"
    );

    // DEC-39: the divergent default branch is left untouched (no FF, no merge).
    assert_eq!(
        git_head(dir.path()),
        main_after,
        "the divergent default branch must not move when the land is blocked"
    );
    // FIX-79 never-destroy: the worktree is preserved and the issue stays ACTIVE so
    // the work is recoverable (rebase + done again), not silently finalized.
    assert!(
        worktree.exists(),
        "the execution worktree must be preserved when integration is blocked"
    );
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = workspace.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert_ne!(
        issue.status,
        crate::model::Status::Done,
        "the issue must not be finalized when its commit did not reach the default branch"
    );
}

#[test]
fn inventory_aggregates_labels_files_and_refs() {
    let dir = workspace_with_issue(); // FIX-01
                                      // Give FIX-01 a label, a file, and a blocker via ishoo_edit.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Blocker", "category": "fix", "plan": "Work",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    );
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({
            "id": "FIX-01", "labels": ["cleanup"],
            "files": ["src/a.rs"], "depends_on": ["FIX-02"]
        }),
    );
    let inv = call_tool(dir.path(), "ishoo_admin", json!({ "op": "inventory",}));
    let cat = &inv["result"]["structuredContent"];
    assert!(cat["labels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["value"] == "cleanup"));
    assert!(cat["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["value"] == "src/a.rs"));
    assert!(cat["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["value"] == "FIX-02" && e["count"] == 1));
}

#[test]
fn comment_add_then_list_round_trips() {
    let dir = workspace_with_issue(); // FIX-01
                                      // Empty list to start.
    let empty = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "list", "id": "FIX-01" }),
    );
    assert!(empty["result"]["structuredContent"]["comments"]
        .as_array()
        .unwrap()
        .is_empty());
    // Add a comment with multi-line text incl. backticks (survives via JSON).
    let body = "needs review of `foo()`\nand the $(bar) path";
    let add = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "add", "id": "FIX-01", "text": body, "author": "spencer" }),
    );
    assert_eq!(add["result"]["structuredContent"]["author"], "spencer");
    assert_eq!(add["result"]["structuredContent"]["text"], body);
    // List now returns it byte-exact.
    let list = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "list", "id": "FIX-01" }),
    );
    let comments = list["result"]["structuredContent"]["comments"]
        .as_array()
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["text"], body);
    // Empty text is rejected.
    let bad = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "add", "id": "FIX-01", "text": "  " }),
    );
    assert_eq!(bad["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn comment_edit_and_remove_round_trip_by_index() {
    let dir = workspace_with_issue(); // FIX-01
    call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "add", "id": "FIX-01", "text": "first", "author": "spencer" }),
    );
    call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "add", "id": "FIX-01", "text": "second", "author": "spencer" }),
    );
    // Edit index 0 (oldest): text changes, author preserved.
    let edited = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "edit", "id": "FIX-01", "index": 0, "text": "first (edited)" }),
    );
    assert_eq!(
        edited["result"]["structuredContent"]["text"],
        "first (edited)"
    );
    assert_eq!(edited["result"]["structuredContent"]["author"], "spencer");
    // Remove index 0: returns the removed comment, list shrinks to the second.
    let removed = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "remove", "id": "FIX-01", "index": 0 }),
    );
    assert_eq!(
        removed["result"]["structuredContent"]["text"],
        "first (edited)"
    );
    let list = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "list", "id": "FIX-01" }),
    );
    let comments = list["result"]["structuredContent"]["comments"]
        .as_array()
        .unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["text"], "second");
    // Out-of-range index is an invalid-params error.
    let bad = call_tool(
        dir.path(),
        "ishoo_comment",
        json!({ "op": "remove", "id": "FIX-01", "index": 9 }),
    );
    assert_eq!(bad["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn decision_adr_renders_markdown_for_a_decision() {
    let dir = workspace_with_issue(); // FIX-01
    let created = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "new", "title": "Use a shared core", "decision": "one core, three front-ends", "problem": "p" }),
    );
    let id = created["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let adr = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "adr", "id": id }),
    );
    let markdown = adr["result"]["structuredContent"]["markdown"]
        .as_str()
        .expect("decision_adr returns markdown");
    assert!(markdown.starts_with("## "));
    assert!(markdown.contains("Use a shared core"));
    // Unknown id is an invalid-params error.
    let bad = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "adr", "id": "DEC-999" }),
    );
    assert_eq!(bad["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn plan_entry_and_lifecycle_tools_round_trip() {
    let dir = workspace_with_issue(); // FIX-01 in active plan "Work"
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "second", "category": "fix", "plan": "Work",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    ); // FIX-02

    // Create + switch to a fresh empty named plan.
    let created = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "new", "name": "Sprint" }),
    );
    assert_eq!(created["result"]["structuredContent"]["name"], "Sprint");
    assert_eq!(created["result"]["structuredContent"]["active"], true);

    // Add appends; anchored add positions; passing both anchors is rejected.
    let added = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "add", "ref": "FIX-01" }),
    );
    assert_eq!(added["result"]["structuredContent"]["changed"], true);
    assert_eq!(
        added["result"]["structuredContent"]["active_plan"],
        "Sprint"
    );
    call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "add", "ref": "FIX-02", "after": "FIX-01" }),
    );
    let both = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "add", "ref": "FIX-01", "after": "FIX-02", "before": "FIX-01" }),
    );
    assert_eq!(both["error"]["code"], registry::INVALID_PARAMS);

    // Move requires exactly one anchor.
    let no_anchor = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "move", "ref": "FIX-02" }),
    );
    assert_eq!(no_anchor["error"]["code"], registry::INVALID_PARAMS);
    let moved = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "move", "ref": "FIX-02", "before": "FIX-01" }),
    );
    assert_eq!(moved["result"]["structuredContent"]["changed"], true);

    // Remove present (changed) then absent (no-op).
    let removed = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "remove", "ref": "FIX-01" }),
    );
    assert_eq!(removed["result"]["structuredContent"]["changed"], true);
    let absent = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "remove", "ref": "FIX-01" }),
    );
    assert_eq!(absent["result"]["structuredContent"]["changed"], false);

    // Clear empties the plan; rename relabels it.
    let cleared = call_tool(dir.path(), "ishoo_plan", json!({ "op": "clear",}));
    assert_eq!(cleared["result"]["structuredContent"]["cleared"], true);
    let renamed = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "rename", "name": "Sprint-2" }),
    );
    assert_eq!(
        renamed["result"]["structuredContent"]["active_plan"],
        "Sprint-2"
    );

    // Populate prepends the live issues.
    let populated = call_tool(dir.path(), "ishoo_plan", json!({ "op": "populate",}));
    assert!(
        populated["result"]["structuredContent"]["prepended"]
            .as_u64()
            .unwrap()
            >= 1
    );

    // delete (empty-only) refuses a non-empty plan; drop force-deletes it.
    let del_nonempty = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "delete", "name": "Sprint-2" }),
    );
    assert_eq!(del_nonempty["error"]["code"], registry::INVALID_PARAMS);
    let dropped = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "drop", "name": "Sprint-2" }),
    );
    assert_eq!(
        dropped["result"]["structuredContent"]["dropped"],
        "Sprint-2"
    );

    // deactivate returns to Backlog; a fresh empty plan deletes cleanly; archive keeps it.
    call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "new", "name": "Temp" }),
    );
    let deact = call_tool(dir.path(), "ishoo_plan", json!({ "op": "deactivate",}));
    assert_eq!(
        deact["result"]["structuredContent"]["active_plan"],
        crate::model::BACKLOG_NAME
    );
    let del_empty = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "delete", "name": "Temp" }),
    );
    assert_eq!(del_empty["result"]["structuredContent"]["deleted"], "Temp");
    call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "new", "name": "Arch" }),
    );
    call_tool(dir.path(), "ishoo_plan", json!({ "op": "deactivate",}));
    let archived = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "archive", "name": "Arch" }),
    );
    assert_eq!(archived["result"]["structuredContent"]["archived"], "Arch");
}

// MCP-57 / DEC-86: ishoo_plan is one op-dispatched tool. A known op runs; a
// missing or unknown op is a structured error (not a panic, not a silent no-op).
#[test]
fn ishoo_plan_dispatches_ops_and_rejects_unknown_or_missing() {
    let dir = workspace_with_issue(); // FIX-01

    let list = call_tool(dir.path(), "ishoo_plan", json!({ "op": "list" }));
    assert!(list.get("error").is_none(), "op:list must succeed: {list}");
    assert!(list["result"].get("structuredContent").is_some());

    let missing = call_tool(dir.path(), "ishoo_plan", json!({}));
    assert_eq!(missing["error"]["code"], registry::INVALID_PARAMS);
    assert!(missing["error"]["message"].as_str().unwrap().contains("op"));

    let bogus = call_tool(dir.path(), "ishoo_plan", json!({ "op": "frobnicate" }));
    assert_eq!(bogus["error"]["code"], registry::INVALID_PARAMS);
    assert!(bogus["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown op"));
}

// MCP-57: the op decides mutation, so a read op skips the post-call snapshot and
// takes the concurrent (non-serial) dispatch path.
#[test]
fn ishoo_plan_classifies_reads_as_non_mutating_and_writes_as_mutating() {
    let reg = registry::registry();
    let plan = reg
        .iter()
        .find(|t| t.name == "ishoo_plan")
        .expect("ishoo_plan tool registered");
    for op in ["next", "show", "list"] {
        assert!(
            !(plan.mutates_store)(&json!({ "op": op })),
            "{op} must be classified as a read"
        );
    }
    for op in [
        "add",
        "move",
        "remove",
        "set",
        "use",
        "new",
        "delete",
        "drop",
        "clear",
        "populate",
        "milestone",
        "rename",
        "deactivate",
        "archive",
    ] {
        assert!(
            (plan.mutates_store)(&json!({ "op": op })),
            "{op} must be classified as a mutation"
        );
    }
}

// MCP-57 / DEC-86: the 17 per-verb plan tools/capabilities are gone, replaced by
// the single ishoo_plan tool and "plan" capability.
#[test]
fn plan_surface_collapsed_to_one_op_tool() {
    let names: Vec<&str> = registry::registry().iter().map(|t| t.name).collect();
    assert!(names.contains(&"ishoo_plan"));
    assert!(
        !names.iter().any(|n| n.starts_with("ishoo_plan_")),
        "no per-verb plan tools should remain: {names:?}"
    );
    assert!(registry::IN_SCOPE_CAPABILITIES.contains(&"plan"));
    assert!(!registry::IN_SCOPE_CAPABILITIES
        .iter()
        .any(|c| c.starts_with("plan_")));
}

// MCP-58 / DEC-86: the 8 per-verb decision tools/capabilities collapse into the
// single ishoo_decision tool, with reads (show/list/adr) classified non-mutating.
#[test]
fn decision_surface_collapsed_to_one_op_tool() {
    let reg = registry::registry();
    let names: Vec<&str> = reg.iter().map(|t| t.name).collect();
    assert!(names.contains(&"ishoo_decision"));
    assert!(
        !names.iter().any(|n| n.starts_with("ishoo_decision_")),
        "no per-verb decision tools should remain: {names:?}"
    );
    assert!(registry::IN_SCOPE_CAPABILITIES.contains(&"decision"));
    assert!(!registry::IN_SCOPE_CAPABILITIES
        .iter()
        .any(|c| c.starts_with("decision_")));

    let decision = reg.iter().find(|t| t.name == "ishoo_decision").unwrap();
    for op in ["show", "list", "adr"] {
        assert!(
            !(decision.mutates_store)(&json!({ "op": op })),
            "{op} is a read"
        );
    }
    for op in ["new", "accept", "edit", "supersede", "delete"] {
        assert!(
            (decision.mutates_store)(&json!({ "op": op })),
            "{op} mutates"
        );
    }
}

// MCP-59 / DEC-86: the 4 per-verb comment tools/capabilities collapse into the
// single ishoo_comment tool, with the read (list) classified non-mutating.
#[test]
fn comment_surface_collapsed_to_one_op_tool() {
    let reg = registry::registry();
    let names: Vec<&str> = reg.iter().map(|t| t.name).collect();
    assert!(names.contains(&"ishoo_comment"));
    assert!(
        !names.iter().any(|n| n.starts_with("ishoo_comment_")),
        "no per-verb comment tools should remain: {names:?}"
    );
    assert!(registry::IN_SCOPE_CAPABILITIES.contains(&"comment"));
    assert!(!registry::IN_SCOPE_CAPABILITIES
        .iter()
        .any(|c| c.starts_with("comment_")));

    let comment = reg.iter().find(|t| t.name == "ishoo_comment").unwrap();
    assert!(
        !(comment.mutates_store)(&json!({ "op": "list" })),
        "list is a read"
    );
    for op in ["add", "edit", "remove"] {
        assert!(
            (comment.mutates_store)(&json!({ "op": op })),
            "{op} mutates"
        );
    }
}

// MCP-60 / DEC-86: the 6 bare maintenance tools fold into ishoo_admin {op}, and the
// land/finish aliases are dropped from the agent surface (done is canonical).
#[test]
fn admin_surface_folded_and_aliases_dropped() {
    let names: Vec<&str> = registry::registry().iter().map(|t| t.name).collect();
    assert!(names.contains(&"ishoo_admin"));
    for gone in [
        "ishoo_inventory",
        "ishoo_lint",
        "ishoo_preflight",
        "ishoo_doctor",
        "ishoo_reclaim",
        "ishoo_gc",
        "ishoo_land",
        "ishoo_finish",
    ] {
        assert!(
            !names.contains(&gone),
            "{gone} should no longer be registered"
        );
    }
    assert!(registry::IN_SCOPE_CAPABILITIES.contains(&"admin"));
    // ishoo_admin never snapshots the store (all ops touch git refs/worktrees or read).
    let admin = registry::registry()
        .into_iter()
        .find(|t| t.name == "ishoo_admin")
        .unwrap();
    for op in ["inventory", "lint", "preflight", "doctor", "reclaim", "gc"] {
        assert!(
            !(admin.mutates_store)(&json!({ "op": op })),
            "{op} must not snapshot"
        );
    }
}

#[test]
fn product_domain_tools_deferred_from_agent_surface() {
    let names: Vec<&str> = registry::registry().iter().map(|t| t.name).collect();
    for gone in PRODUCT_DOMAIN_TOOLS {
        assert!(
            !names.contains(gone),
            "{gone} should be CLI/UI-only in the v1 agent surface"
        );
    }
    assert!(
        names.contains(&"ishoo_hero_signal"),
        "hero_signal remains until DEC-80 is superseded or reconciled with DEC-86"
    );

    for command in ["milestone", "epic", "roadmap", "version", "people"] {
        let entry = registry::cli_capability_inventory()
            .into_iter()
            .find(|entry| entry.command == command)
            .unwrap_or_else(|| panic!("{command} must be inventoried"));
        assert_eq!(entry.class, registry::CliCapabilityClass::ScriptOnly);
        assert!(
            entry.mcp_tools.is_empty(),
            "{command} should not advertise MCP coverage"
        );
    }
}

#[test]
fn plan_show_next_is_scoped_to_active_plan_not_backlog() {
    let dir = workspace_with_issue(); // Work active, FIX-01 ready
                                      // A ready item in the default Backlog — the item the OLD fallback would surface.
                                      // (DEC-55: `backlog` isn't a valid --plan target, so seed via the model API.)
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "in backlog", "category": "fix", "plan": "new:\"Other\"",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    );
    let mut all = crate::model::AllPlans::load(dir.path());
    all.default_plan
        .add("local".to_string(), "FIX-02".to_string());
    all.save(dir.path()).unwrap();
    // Make Work active and exhausted (its only item done).
    call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "use", "name": "Work" }),
    );
    let mut ws = crate::model::Workspace::load(dir.path()).unwrap();
    ws.issues
        .iter_mut()
        .find(|i| i.id == "FIX-01")
        .unwrap()
        .status = crate::model::Status::Done;
    ws.save().unwrap();

    // plan_show.next is the active plan's own front (null here), not a Backlog item.
    let show = call_tool(dir.path(), "ishoo_plan", json!({ "op": "show",}));
    let v = &show["result"]["structuredContent"];
    assert_eq!(v["active_plan"], "Work");
    assert!(
        v["next"].is_null(),
        "plan_show.next must not fall back to Backlog: {}",
        v["next"]
    );
    // plan_next (the cross-plan tool) still applies the DEC-44 fallback.
    let pn = call_tool(dir.path(), "ishoo_plan", json!({ "op": "next",}));
    assert_eq!(pn["result"]["structuredContent"]["issue_id"], "FIX-02");
}

#[test]
fn plan_list_enumerates_plans_and_plan_use_disambiguates_default() {
    let dir = workspace_with_issue(); // active plan "Work" with FIX-01
                                      // plan_list shows the default Backlog plus the named Work, with Work active.
    let list = call_tool(dir.path(), "ishoo_plan", json!({ "op": "list",}));
    let plans = list["result"]["structuredContent"]["plans"]
        .as_array()
        .unwrap();
    let work = plans.iter().find(|p| p["name"] == "Work").unwrap();
    assert_eq!(work["active"], true);
    assert_eq!(work["entries"], 1);
    let backlog = plans
        .iter()
        .find(|p| p["default"] == true)
        .expect("default plan listed");
    assert_eq!(backlog["name"], "Backlog");
    assert_eq!(backlog["active"], false);

    // Disambiguation: a named plan literally "Backlog" (seeded via the model API,
    // since `new` reserves that name) must win over the default in plan_use —
    // proving the structural switch_to-first logic, not a magic string.
    let mut all = crate::model::AllPlans::load(dir.path());
    all.add_named_plan("Backlog").unwrap();
    all.active_plan_id = None; // leave the default active before the switch
    all.save(dir.path()).unwrap();
    let used = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "use", "name": "Backlog" }),
    );
    assert_eq!(
        used["result"]["structuredContent"]["active_plan"],
        "Backlog"
    );
    let after = crate::model::AllPlans::load(dir.path());
    assert!(
        after.active_plan_id.is_some(),
        "plan_use must activate the named Backlog, not deactivate to the default"
    );
}

#[test]
fn plan_show_by_name_inspects_any_plan_without_changing_active() {
    let dir = workspace_with_issue(); // "Work" active, FIX-01 in it
                                      // A second named plan with its own issue.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "in other", "category": "fix", "plan": "new:\"Other\"",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    );
    // Creating "Other" made it active; switch back so the test's premise holds.
    call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "use", "name": "Work" }),
    );
    let before = crate::model::AllPlans::load(dir.path()).active_label();
    assert_eq!(before, "Work");

    // Inspect "Other" by name — the view is of Other, with its issue.
    let show = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "show", "name": "Other" }),
    );
    let v = &show["result"]["structuredContent"];
    assert_eq!(v["active_plan"], "Other");
    assert_eq!(v["items"][0]["issue_id"], "FIX-02");

    // The active plan is unchanged by the read-only inspection.
    let after = crate::model::AllPlans::load(dir.path()).active_label();
    assert_eq!(
        after, "Work",
        "plan_show by name must not change the active plan"
    );

    // Backlog is addressable by name; an unknown name is a clean error.
    let backlog = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "show", "name": "Backlog" }),
    );
    assert_eq!(
        backlog["result"]["structuredContent"]["active_plan"],
        "Backlog"
    );
    let missing = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "show", "name": "Nope" }),
    );
    assert_eq!(missing["error"]["code"], registry::INVALID_PARAMS);
    assert!(missing["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Nope"));
}

#[test]
fn plan_list_reports_open_and_done_counts() {
    let dir = workspace_with_issue(); // "Work" active with FIX-01 (open)
    let list = call_tool(dir.path(), "ishoo_plan", json!({ "op": "list",}));
    let plans = list["result"]["structuredContent"]["plans"]
        .as_array()
        .unwrap();
    let work = plans.iter().find(|p| p["name"] == "Work").unwrap();
    assert_eq!(work["entries"], 1);
    assert_eq!(work["open"], 1);
    assert_eq!(work["done"], 0);

    // Mark FIX-01 done; the counts flip.
    let mut ws = crate::model::Workspace::load(dir.path()).unwrap();
    ws.issues
        .iter_mut()
        .find(|i| i.id == "FIX-01")
        .unwrap()
        .status = crate::model::Status::Done;
    ws.save().unwrap();
    let list = call_tool(dir.path(), "ishoo_plan", json!({ "op": "list",}));
    let plans = list["result"]["structuredContent"]["plans"]
        .as_array()
        .unwrap();
    let work = plans.iter().find(|p| p["name"] == "Work").unwrap();
    assert_eq!(work["open"], 0);
    assert_eq!(work["done"], 1);
}

#[test]
fn plan_show_use_and_set_drive_the_active_plan() {
    let dir = workspace_with_issue(); // FIX-01 in active plan "Work"
                                      // plan_show surfaces the whole active plan, not just the front.
    let show = call_tool(dir.path(), "ishoo_plan", json!({ "op": "show",}));
    let view = &show["result"]["structuredContent"];
    assert_eq!(view["active_plan"], "Work");
    assert_eq!(view["items"][0]["issue_id"], "FIX-01");

    // A second issue in the same plan, to exercise reconstruction.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Second", "category": "fix", "plan": "Work",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    );

    // plan_set reconstructs the active plan from an ordered id list.
    let set = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "set", "ids": ["FIX-02", "FIX-01"] }),
    );
    assert_eq!(set["result"]["structuredContent"]["entry_count"], 2);
    // A bad id leaves the plan unchanged (invalid-params error).
    let bad = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "set", "ids": ["NOPE-9"] }),
    );
    assert_eq!(bad["error"]["code"], registry::INVALID_PARAMS);

    // plan_use activates a different plan; plan_show then reflects it.
    let used = call_tool(
        dir.path(),
        "ishoo_plan",
        json!({ "op": "use", "name": "Backlog" }),
    );
    assert_eq!(
        used["result"]["structuredContent"]["active_plan"],
        "Backlog"
    );
    let show2 = call_tool(dir.path(), "ishoo_plan", json!({ "op": "show",}));
    assert_eq!(
        show2["result"]["structuredContent"]["active_plan"],
        "Backlog"
    );
}

#[test]
fn rename_id_updates_issue_refs_and_plan_entries() {
    let dir = workspace_with_issue(); // FIX-01 in plan "Work"
                                      // A second issue that depends on FIX-01.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "dependent", "category": "fix", "plan": "Work",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": ["FIX-01"]
        }),
    );
    // Re-categorize FIX-01 → CLI-09.
    let r = call_tool(
        dir.path(),
        "ishoo_rename_id",
        json!({ "id": "FIX-01", "new_id": "CLI-09" }),
    );
    assert_eq!(r["result"]["structuredContent"]["new_id"], "CLI-09");

    // Old id gone, new id present.
    let old = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(old["error"]["code"], registry::INVALID_PARAMS);
    let new = call_tool(dir.path(), "ishoo_show", json!({ "id": "CLI-09" }));
    assert_eq!(new["result"]["structuredContent"]["issue"]["id"], "CLI-09");

    // The dependent's depends_on was rewritten to the new id.
    let dep = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-02" }));
    assert!(dep["result"]["structuredContent"]["issue"]["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d == "CLI-09"));

    // The plan entry was rewritten across the plan.
    let plan = call_tool(dir.path(), "ishoo_plan", json!({ "op": "show",}));
    let items = plan["result"]["structuredContent"]["items"]
        .as_array()
        .unwrap();
    assert!(items.iter().any(|i| i["issue_id"] == "CLI-09"));
    assert!(!items.iter().any(|i| i["issue_id"] == "FIX-01"));
}

#[test]
fn delete_guards_done_issues_without_force() {
    let dir = workspace_with_issue(); // FIX-01
                                      // Mark FIX-01 done directly in the store.
    let mut ws = crate::model::Workspace::load(dir.path()).unwrap();
    ws.issues
        .iter_mut()
        .find(|i| i.id == "FIX-01")
        .unwrap()
        .status = crate::model::Status::Done;
    ws.save().unwrap();

    // Delete without force is refused, and the issue survives.
    let refused = call_tool(dir.path(), "ishoo_delete", json!({ "id": "FIX-01" }));
    assert_eq!(refused["error"]["code"], registry::INVALID_PARAMS);
    let still = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(
        still["result"]["structuredContent"]["issue"]["id"],
        "FIX-01"
    );

    // With force:true it deletes.
    let forced = call_tool(
        dir.path(),
        "ishoo_delete",
        json!({ "id": "FIX-01", "force": true }),
    );
    assert_eq!(forced["result"]["structuredContent"]["deleted"], true);
}

#[test]
fn delete_removes_issue_and_prunes_plan_entry() {
    let dir = workspace_with_issue(); // FIX-01, filed into plan "Work"
    let resp = call_tool(dir.path(), "ishoo_delete", json!({ "id": "FIX-01" }));
    let out = &resp["result"]["structuredContent"];
    assert_eq!(out["id"], "FIX-01");
    assert_eq!(out["deleted"], true);
    // It was the sole entry of the "Work" plan, so one entry is pruned.
    assert_eq!(out["pruned_plan_entries"], 1);
    // The issue is gone from the store.
    let gone = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(gone["error"]["code"], registry::INVALID_PARAMS);
    // Deleting a missing id is an invalid-params error, not a panic.
    let missing = call_tool(dir.path(), "ishoo_delete", json!({ "id": "NOPE-9" }));
    assert_eq!(missing["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn shelve_retires_a_shelved_labeled_issue_without_gates() {
    let dir = workspace_with_issue(); // FIX-01, in plan "Work"

    // Without the `shelved` label, shelve is refused (URGE-05 / DEC-90).
    let refused = call_tool(
        dir.path(),
        "ishoo_shelve",
        json!({ "id": "FIX-01", "reason": "defer" }),
    );
    assert_eq!(refused["error"]["code"], registry::INVALID_PARAMS);

    // Label it shelved via the shared edit path.
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "labels": ["shelved"] }),
    );

    // A missing rationale is refused at the tool boundary.
    let no_reason = call_tool(dir.path(), "ishoo_shelve", json!({ "id": "FIX-01" }));
    assert_eq!(no_reason["error"]["code"], registry::INVALID_PARAMS);

    // With the label and a reason it retires — no completion gates, no worktree.
    let ok = call_tool(
        dir.path(),
        "ishoo_shelve",
        json!({ "id": "FIX-01", "reason": "keeping the research notes" }),
    );
    assert_eq!(ok["result"]["structuredContent"]["shelved"], true);

    // It stays queryable, now Declined with the recorded rationale.
    let show = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    let issue = &show["result"]["structuredContent"]["issue"];
    assert_eq!(issue["status"], "Declined");
    assert_eq!(issue["retire_reason"], "keeping the research notes");

    // And it has left the live queue: the plan has no ready next item.
    let next = call_tool(dir.path(), "ishoo_plan", json!({ "op": "next" }));
    assert!(
        next["result"]["structuredContent"].is_null(),
        "shelved issue must not be recommended as next work"
    );
}

#[test]
fn recovery_tools_are_advertised_and_cli_inventory_marks_them_covered() {
    let response = respond(
        tempdir().unwrap().path(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools = response["result"]["tools"].as_array().unwrap();
    let names: HashSet<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    // MCP-60 / DEC-86: reclaim/gc/doctor are folded into ishoo_admin {op}.
    assert!(names.contains("ishoo_admin"));
    assert!(!names.contains("ishoo_reclaim"));
    assert!(!names.contains("ishoo_gc"));
    assert!(!names.contains("ishoo_doctor"));

    for command in ["reclaim", "gc", "doctor"] {
        let entry = registry::cli_capability_inventory()
            .iter()
            .find(|entry| entry.command == command)
            .unwrap();
        assert_eq!(entry.follow_up_issues, &[] as &[&str]);
        assert!(
            !entry.mcp_tools.is_empty(),
            "{command} must be covered by an MCP tool"
        );
    }
}

#[test]
fn reclaim_refuses_fresh_claim_and_takes_stale_claim() {
    let dir = workspace_with_issue();
    let root = dir.path();

    crate::model::git_remote::create_claim(root, "FIX-01").unwrap();
    let fresh = call_tool(
        root,
        "ishoo_admin",
        json!({ "op": "reclaim", "id": "FIX-01" }),
    );
    let fresh_out = &fresh["result"]["structuredContent"];
    assert_eq!(fresh_out["status"], "BLOCKED");
    assert_eq!(fresh_out["reason"], "fresh_claim");
    assert_eq!(fresh_out["reclaimed"], false);

    crate::model::git_remote::release_claim(root, "FIX-01").unwrap();
    let claim_file = root.join("old-claim.txt");
    std::fs::write(
        &claim_file,
        "issue_id=FIX-01\ntimestamp_secs=0\nhostname=old-host\n",
    )
    .unwrap();
    let blob = git_out(root, &["hash-object", "-w", claim_file.to_str().unwrap()]);
    git(
        root,
        &[
            "update-ref",
            &crate::model::git_remote::claim_ref_name("FIX-01"),
            &blob,
        ],
    );

    let reclaimed = call_tool(
        root,
        "ishoo_admin",
        json!({ "op": "reclaim", "id": "FIX-01" }),
    );
    let out = &reclaimed["result"]["structuredContent"];
    assert_eq!(out["status"], "RECLAIMED");
    assert_eq!(out["reclaimed"], true);
    assert_eq!(out["previous_claim"]["hostname"], "old-host");
    assert_eq!(out["claim_push"]["state"], "no_remote");
}

#[test]
fn gc_sweeps_orphan_execution_substrate_through_mcp() {
    let dir = workspace_with_issue();
    let root = dir.path();

    crate::model::git_remote::create_claim(root, "ISS-A").unwrap();
    crate::model::git_remote::create_worktree(root, "ISS-A").unwrap();
    crate::model::git_remote::release_claim(root, "ISS-A").unwrap();

    crate::model::git_remote::create_claim(root, "ISS-B").unwrap();

    let response = call_tool(root, "ishoo_admin", json!({ "op": "gc",}));
    let out = &response["result"]["structuredContent"];
    assert_eq!(out["status"], "CLEANED");
    let report = &out["report"];
    assert!(report["removed_worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == "ISS-A"));
    assert!(report["removed_claims"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == "ISS-B"));
}

#[test]
fn doctor_diagnoses_and_fixes_store_drift_through_mcp() {
    let dir = workspace_with_issue();
    let root = dir.path();

    let mut workspace = crate::model::Workspace::load(root).unwrap();
    workspace
        .issues
        .iter_mut()
        .find(|issue| issue.id == "FIX-01")
        .unwrap()
        .title = "unsnapshotted title".to_string();
    workspace.save().unwrap();

    let diagnosed = call_tool(root, "ishoo_admin", json!({ "op": "doctor",}));
    let diagnosis = &diagnosed["result"]["structuredContent"];
    assert_eq!(diagnosis["status"], "FAULTS");
    assert_eq!(diagnosis["report"]["store_drift"], "ahead");

    let fixed = call_tool(root, "ishoo_admin", json!({ "op": "doctor", "fix": true }));
    let outcome = &fixed["result"]["structuredContent"];
    assert_eq!(outcome["status"], "HEALED");
    assert_eq!(outcome["fixed"], true);
    assert_eq!(outcome["heal"]["resnapshotted"], true);
    assert_eq!(outcome["after"]["healthy"], true);
}

#[test]
fn edit_scalar_clear_is_symmetric_and_absent_is_unchanged() {
    let dir = workspace_with_issue(); // FIX-01
                                      // Set a scalar (description).
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "description": "hello world" }),
    );
    let v1 = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(
        v1["result"]["structuredContent"]["issue"]["description"],
        "hello world"
    );
    // An edit that omits description leaves it unchanged (present-vs-absent).
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "labels": ["fix"] }),
    );
    let v2 = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(
        v2["result"]["structuredContent"]["issue"]["description"],
        "hello world"
    );
    // A present empty string clears it — symmetric with [] clearing a list field.
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "description": "" }),
    );
    let v3 = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert_eq!(
        v3["result"]["structuredContent"]["issue"]["description"],
        ""
    );
}

#[test]
fn edit_updates_non_resolution_fields_clears_and_rejects_noop() {
    let dir = workspace_with_issue(); // FIX-01
                                      // A second issue to reference as a real blocker (validated by the core).
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Blocker", "category": "fix", "plan": "Work",
            "concrete_change": "c", "main_surface": "src/b.rs", "proof_of_done": "p",
            "out_of_scope": "o", "decisions": [], "depends_on": []
        }),
    );
    // Edit FIX-01: add a label and convert a later-discovered blocker (MCP-13).
    let resp = call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "labels": ["cleanup"], "depends_on": ["FIX-02"] }),
    );
    assert_eq!(resp["result"]["structuredContent"]["id"], "FIX-01");

    // Read the change back through ishoo_show.
    let view = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    let issue = &view["result"]["structuredContent"]["issue"];
    assert!(issue["labels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l == "cleanup"));
    assert!(issue["depends_on"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d == "FIX-02"));

    // A present empty array clears the field.
    call_tool(
        dir.path(),
        "ishoo_edit",
        json!({ "id": "FIX-01", "labels": [] }),
    );
    let cleared = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert!(cleared["result"]["structuredContent"]["issue"]["labels"]
        .as_array()
        .unwrap()
        .is_empty());

    // An id-only call (nothing to change) is an invalid-params error.
    let noop = call_tool(dir.path(), "ishoo_edit", json!({ "id": "FIX-01" }));
    assert_eq!(noop["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn governing_adrs_surface_in_status_begin_and_show() {
    let dir = workspace_with_issue(); // FIX-01, plan front
                                      // Relevant ACCEPTED decisions govern; unrelated ACCEPTED and PROPOSED ones must not surface.
    let mut ws = crate::model::Workspace::load(dir.path()).unwrap();
    ws.issues[0].labels = vec!["git".to_string()];
    ws.save().unwrap();

    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    let accepted = crate::model::build_decision(
        &ws,
        &crate::model::NewDecisionInput {
            title: "Git-native store".to_string(),
            decision: "The store is git-native.".to_string(),
            problem: "p".to_string(),
            status: crate::model::DecisionStatus::Accepted,
            tags: vec!["git".to_string()],
            ..Default::default()
        },
    )
    .unwrap();
    let acc_id = accepted.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![accepted]).unwrap();

    let ws2 = crate::model::Workspace::load(dir.path()).unwrap();
    let explicit = crate::model::build_decision(
        &ws2,
        &crate::model::NewDecisionInput {
            title: "Safety rule".to_string(),
            decision: "Never destroy work.".to_string(),
            problem: "p".to_string(),
            status: crate::model::DecisionStatus::Accepted,
            tags: vec!["safety".to_string()],
            ..Default::default()
        },
    )
    .unwrap();
    let explicit_id = explicit.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![explicit]).unwrap();

    let ws3 = crate::model::Workspace::load(dir.path()).unwrap();
    let unrelated = crate::model::build_decision(
        &ws3,
        &crate::model::NewDecisionInput {
            title: "UI rule".to_string(),
            decision: "Keep layout stable.".to_string(),
            problem: "p".to_string(),
            status: crate::model::DecisionStatus::Accepted,
            tags: vec!["ui".to_string()],
            ..Default::default()
        },
    )
    .unwrap();
    let unrelated_id = unrelated.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![unrelated]).unwrap();

    let mut ws4 = crate::model::Workspace::load(dir.path()).unwrap();
    ws4.issues[0].decision_refs = vec![explicit_id.clone()];
    ws4.save().unwrap();

    let ws5 = crate::model::Workspace::load(dir.path()).unwrap();
    let proposed = crate::model::build_decision(
        &ws5,
        &crate::model::NewDecisionInput {
            title: "Draft idea".to_string(),
            decision: "Not yet binding.".to_string(),
            problem: "p".to_string(),
            status: crate::model::DecisionStatus::Proposed,
            ..Default::default()
        },
    )
    .unwrap();
    let prop_id = proposed.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![proposed]).unwrap();

    let has_accepted = |resp: &Value| -> bool {
        resp["result"]["structuredContent"]["governing_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| {
                g["decision_id"] == acc_id && g["headline"].as_str().unwrap().contains("git-native")
            })
    };
    let has_proposed = |resp: &Value| -> bool {
        resp["result"]["structuredContent"]["governing_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["decision_id"] == prop_id)
    };
    let has_explicit = |resp: &Value| -> bool {
        resp["result"]["structuredContent"]["governing_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["decision_id"] == explicit_id)
    };
    let has_unrelated = |resp: &Value| -> bool {
        resp["result"]["structuredContent"]["governing_decisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["decision_id"] == unrelated_id)
    };

    // All three orientation/transition surfaces carry only relevant accepted ADRs.
    let show = call_tool(dir.path(), "ishoo_show", json!({ "id": "FIX-01" }));
    assert!(has_accepted(&show) && has_explicit(&show));
    assert!(!has_unrelated(&show) && !has_proposed(&show));
    let status = call_tool(dir.path(), "ishoo_status", json!({}));
    assert!(has_accepted(&status) && has_explicit(&status));
    assert!(!has_unrelated(&status) && !has_proposed(&status));
    let begin = call_tool(dir.path(), "ishoo_set_active", json!({ "id": "FIX-01" }));
    assert!(has_accepted(&begin) && has_explicit(&begin));
    assert!(!has_unrelated(&begin) && !has_proposed(&begin));
}

#[test]
fn decision_authoring_validates_refs_and_rejects_noop_edit() {
    let dir = workspace_with_issue(); // FIX-01 exists
                                      // decision_new with a dangling related_issue is rejected.
    let bad = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "new", "title": "t", "decision": "d", "problem": "p", "related_issues": ["NOPE-9"] }),
    );
    assert_eq!(bad["error"]["code"], registry::INVALID_PARAMS);
    // With a real related_issue it succeeds.
    let ok = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "new", "title": "t", "decision": "d", "problem": "p", "related_issues": ["FIX-01"] }),
    );
    let id = ok["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // decision_edit with only an id is rejected (no-op guard).
    let noop = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "edit", "id": id }),
    );
    assert_eq!(noop["error"]["code"], registry::INVALID_PARAMS);
    // decision_edit with a dangling related_issue is rejected (confirm:true so it
    // passes the DEC-12 confirm-gate and reaches ref validation).
    let bade = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "edit", "id": id, "related_issues": ["NOPE-9"], "confirm": true }),
    );
    assert_eq!(bade["error"]["code"], registry::INVALID_PARAMS);
}

#[test]
fn decision_new_accept_edit_round_trip() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    // Author an ADR through MCP.
    let created = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "new",
            "title": "Sidecar ref store",
            "decision": "The store rides refs/ishoo/store.",
            "problem": "Store churn polluted main."
        }),
    );
    let id = created["result"]["structuredContent"]["id"]
        .as_str()
        .expect("new decision id")
        .to_string();
    assert_eq!(created["result"]["structuredContent"]["status"], "PROPOSED");

    // Accept it.
    let accepted = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "accept", "id": id }),
    );
    assert_eq!(
        accepted["result"]["structuredContent"]["status"],
        "ACCEPTED"
    );

    // Amend the rule (confirm:true clears the DEC-12 confirm-gate for a clarification).
    call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "edit", "id": id, "rule": "Never track .ishoo on main.", "confirm": true }),
    );

    // Read it back through ishoo_decision (op:show): accepted + amended rule persisted.
    let show = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": id }),
    );
    let view = &show["result"]["structuredContent"];
    assert_eq!(view["status"], "ACCEPTED");
    assert_eq!(view["rule"], "Never track .ishoo on main.");
    assert_eq!(view["problem"], "Store churn polluted main.");
}

// MCP-49 / DEC-12: decision_edit is confirm-gated (edit = clarification only), and
// supersede/delete complete the agent's ADR-curation surface.
#[test]
fn decision_edit_confirm_gate_and_supersede_delete() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();

    let mk = |title: &str| {
        let created = call_tool(
            dir.path(),
            "ishoo_decision",
            json!({ "op": "new", "title": title, "decision": "d", "problem": "p" }),
        );
        created["result"]["structuredContent"]["id"]
            .as_str()
            .expect("new decision id")
            .to_string()
    };

    // Confirm-gate: editing WITHOUT confirm makes no change and asks to confirm.
    let old_id = mk("Old position");
    let gated = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "edit", "id": old_id, "rule": "changed wording" }),
    );
    assert_eq!(
        gated["result"]["structuredContent"]["status"],
        "confirmation_required"
    );
    let show = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": old_id }),
    );
    assert_eq!(
        show["result"]["structuredContent"]["rule"], "None.",
        "a gated (unconfirmed) edit must not mutate the record"
    );

    // Supersede: author the replacement, then link OLD -> NEW.
    let new_id = mk("New position");

    // DECI-01: a supersession without a reason is rejected.
    let no_reason = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "supersede", "superseded_id": old_id, "new_id": new_id }),
    );
    assert_eq!(no_reason["error"]["code"], registry::INVALID_PARAMS);
    // The rejected attempt must not have mutated the old record.
    let untouched = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": old_id }),
    );
    assert_ne!(
        untouched["result"]["structuredContent"]["status"],
        "SUPERSEDED"
    );

    let referencing_issue = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Review old decision reference",
            "category": "adrrev",
            "plan": "new:\"Decision review\"",
            "concrete_change": "review the old ADR reference",
            "main_surface": "issue tracker",
            "proof_of_done": "decision refs are reconciled",
            "out_of_scope": "auto-repointing the issue",
            "decisions": [old_id.clone()],
            "depends_on": []
        }),
    );
    let referencing_issue_id = referencing_issue["result"]["structuredContent"]["id"]
        .as_str()
        .expect("created issue id")
        .to_string();

    let superseded = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "supersede",
            "superseded_id": old_id,
            "new_id": new_id,
            "reason": "the old position was replaced by a sharper rule"
        }),
    );
    assert_eq!(
        superseded["result"]["structuredContent"]["status"],
        "SUPERSEDED"
    );
    assert_eq!(
        superseded["result"]["structuredContent"]["affected_live_issue_count"],
        1
    );
    assert_eq!(
        superseded["result"]["structuredContent"]["affected_live_issues"][0],
        referencing_issue_id
    );
    let lint = call_tool(dir.path(), "ishoo_admin", json!({ "op": "lint" }));
    let findings = lint["result"]["structuredContent"]["findings"]
        .as_array()
        .expect("lint findings array");
    assert!(
        findings.iter().any(|finding| {
            let message = finding["message"].as_str().unwrap_or_default();
            message.contains("references superseded decision")
                && message.contains(&referencing_issue_id)
                && message.contains(old_id.as_str())
                && message.contains(new_id.as_str())
        }),
        "lint must report affected live issue for review: {lint}"
    );
    // DECI-01: the why is recorded on BOTH records' supporting note.
    let old_after = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": old_id }),
    );
    assert_eq!(
        old_after["result"]["structuredContent"]["status"],
        "SUPERSEDED"
    );
    assert_eq!(
        old_after["result"]["structuredContent"]["superseded_by"],
        new_id
    );
    assert!(
        old_after["result"]["structuredContent"]["supporting_note"]
            .as_str()
            .unwrap_or_default()
            .contains("the old position was replaced by a sharper rule"),
        "the superseded record must show the why"
    );
    let new_after = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": new_id }),
    );
    assert_eq!(
        new_after["result"]["structuredContent"]["supersedes"],
        old_id
    );
    assert!(
        new_after["result"]["structuredContent"]["supporting_note"]
            .as_str()
            .unwrap_or_default()
            .contains("the old position was replaced by a sharper rule"),
        "the superseding record must show the why"
    );

    // Delete: gated without confirm, removes the record with confirm:true.
    let noise_id = mk("Genuine noise");
    let delete_gated = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "delete", "id": noise_id }),
    );
    assert_eq!(
        delete_gated["result"]["structuredContent"]["status"],
        "confirmation_required"
    );
    let still_there = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": noise_id }),
    );
    assert!(
        still_there["result"]["structuredContent"]["id"].is_string(),
        "a gated (unconfirmed) delete must not remove the record"
    );
    let deleted = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "delete", "id": noise_id, "confirm": true }),
    );
    assert_eq!(deleted["result"]["structuredContent"]["deleted"], true);
    let gone = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": noise_id }),
    );
    assert_eq!(
        gone["error"]["code"],
        registry::INVALID_PARAMS,
        "the deleted ADR must no longer be found"
    );
}

#[test]
fn decision_show_and_list_return_typed_adrs() {
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    // Seed one ADR through the model, then read it back through MCP (MCP-12).
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let decision = crate::model::build_decision(
        &workspace,
        &crate::model::NewDecisionInput {
            title: "Use the sidecar ref".to_string(),
            decision: "Store rides refs/ishoo/store.".to_string(),
            problem: "Store churn polluted main.".to_string(),
            scope: "The store ref.".to_string(),
            rule: "Never track .ishoo on main.".to_string(),
            consequences: "Clean history.".to_string(),
            alternatives_rejected: "Tracking the dir.".to_string(),
            operational_impact: "None.".to_string(),
            status: crate::model::DecisionStatus::Accepted,
            ..Default::default()
        },
    )
    .unwrap();
    let id = decision.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![decision]).unwrap();

    // decision_list surfaces it with id/title/status.
    let list = call_tool(dir.path(), "ishoo_decision", json!({ "op": "list",}));
    let decisions = list["result"]["structuredContent"]["decisions"]
        .as_array()
        .unwrap();
    assert!(decisions
        .iter()
        .any(|d| d["id"] == id && d["status"] == "ACCEPTED"));

    // decision_show returns the full structured record byte-exact.
    let show = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": id }),
    );
    let view = &show["result"]["structuredContent"];
    assert_eq!(view["rule"], "Never track .ishoo on main.");
    assert_eq!(view["status"], "ACCEPTED");
    assert_eq!(view["problem"], "Store churn polluted main.");

    // A missing id is an invalid-params error, not a panic.
    let missing = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "show", "id": "DEC-999" }),
    );
    assert!(missing["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

#[test]
fn decompose_files_children_records_lineage_and_inherits_parent_plan() {
    // FEAT-15 / DEC-61: ishoo_decompose splits a parent into children, recording
    // parent.decomposed_into <-> child.decomposed_from durably, and the children
    // inherit the parent's named plan — all through the real MCP surface.
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();

    // Parent lives in the named plan "Work".
    let parent = call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Umbrella", "category": "feat", "plan": "new:\"Work\"",
            "concrete_change": "a", "main_surface": "b", "proof_of_done": "c",
            "out_of_scope": "d", "decisions": [], "depends_on": []
        }),
    );
    let parent_id = parent["result"]["structuredContent"]["id"]
        .as_str()
        .expect("parent id")
        .to_string();

    let response = call_tool(
        dir.path(),
        "ishoo_decompose",
        json!({
            "parent": parent_id,
            "children": [
                {
                    "title": "First child", "category": "feat",
                    "concrete_change": "c1", "main_surface": "s1",
                    "proof_of_done": "p1", "out_of_scope": "o1",
                    "decisions": [], "depends_on": []
                },
                {
                    "title": "Second child", "category": "fix",
                    "concrete_change": "c2", "main_surface": "s2",
                    "proof_of_done": "p2", "out_of_scope": "o2",
                    "decisions": [], "depends_on": []
                }
            ]
        }),
    );

    let structured = &response["result"]["structuredContent"];
    assert_eq!(
        structured["parent_id"], parent_id,
        "response: {response:#?}"
    );
    let children = structured["children"].as_array().expect("children array");
    assert_eq!(children.len(), 2);
    let child_ids: Vec<String> = children
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();

    // Lineage persisted on both sides of the relation.
    let workspace = crate::model::Workspace::load(dir.path()).unwrap();
    let parent_issue = workspace.issues.iter().find(|i| i.id == parent_id).unwrap();
    assert_eq!(parent_issue.decomposed_into, child_ids);
    assert!(parent_issue.decomposed_from.is_none());
    for cid in &child_ids {
        let child = workspace.issues.iter().find(|i| &i.id == cid).unwrap();
        assert_eq!(child.decomposed_from.as_deref(), Some(parent_id.as_str()));
        // The decomposition relation never leaks into links/depends_on.
        assert!(child.links.is_empty());
        assert!(child.depends_on.is_empty());
    }

    // Children inherited the parent's plan: all three ids sit in plan "Work".
    let plans = crate::model::AllPlans::load(dir.path());
    let work = plans
        .named
        .iter()
        .find(|p| p.name == "Work")
        .expect("plan Work exists");
    let members: Vec<&str> = work
        .plan
        .entries
        .iter()
        .map(|e| e.issue_id.as_str())
        .collect();
    assert!(members.contains(&parent_id.as_str()));
    for cid in &child_ids {
        assert!(
            members.contains(&cid.as_str()),
            "child {cid} not in plan Work"
        );
    }
}

#[test]
fn decompose_rejects_unknown_parent() {
    // A missing parent is an invalid-params error, not a panic or a silent no-op.
    let dir = tempdir().unwrap();
    crate::model::init_workspace(dir.path()).unwrap();
    let response = call_tool(
        dir.path(),
        "ishoo_decompose",
        json!({
            "parent": "FEAT-99",
            "children": [{
                "title": "Orphan", "concrete_change": "a", "main_surface": "b",
                "proof_of_done": "c", "out_of_scope": "d",
                "decisions": [], "depends_on": []
            }]
        }),
    );
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

#[test]
fn decision_list_filters_by_label_and_text() {
    let dir = workspace_with_issue();
    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    let a = crate::model::build_decision(
        &ws,
        &crate::model::NewDecisionInput {
            title: "Atomic push".to_string(),
            decision: "push main and store".to_string(),
            problem: "p".to_string(),
            tags: vec!["durability".to_string(), "git".to_string()],
            status: crate::model::DecisionStatus::Accepted,
            ..Default::default()
        },
    )
    .unwrap();
    let a_id = a.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![a]).unwrap();
    let ws2 = crate::model::Workspace::load(dir.path()).unwrap();
    let b = crate::model::build_decision(
        &ws2,
        &crate::model::NewDecisionInput {
            title: "Sharded store".to_string(),
            decision: "per-record files".to_string(),
            problem: "p".to_string(),
            tags: vec!["store".to_string()],
            status: crate::model::DecisionStatus::Accepted,
            ..Default::default()
        },
    )
    .unwrap();
    let b_id = b.decision_id.clone();
    crate::model::Workspace::persist_decisions(dir.path(), vec![b]).unwrap();

    let ids = |resp: &Value| -> Vec<String> {
        resp["result"]["structuredContent"]["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap().to_string())
            .collect()
    };

    // label filter returns only the carrying ADR, and the row carries its labels.
    let by_label = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "list", "label": "durability" }),
    );
    assert_eq!(ids(&by_label), vec![a_id.clone()]);
    assert_eq!(
        by_label["result"]["structuredContent"]["decisions"][0]["labels"],
        json!(["durability", "git"])
    );

    // text filter matches a title substring (case-insensitive).
    let by_text = call_tool(
        dir.path(),
        "ishoo_decision",
        json!({ "op": "list", "text": "SHARDED" }),
    );
    assert_eq!(ids(&by_text), vec![b_id.clone()]);

    // no args returns all.
    let all = call_tool(dir.path(), "ishoo_decision", json!({ "op": "list",}));
    assert_eq!(ids(&all).len(), 2);
}

// ============================================================================
// URGE-06 (DEC-90): end-to-end urgency-tier regression matrix.
//
// Drives the real MCP surface (ishoo_new / ishoo_plan / ishoo_status /
// ishoo_edit / ishoo_shelve / ishoo_delete) to lock the whole urgency system
// together: the five tier labels, active-plan vs cross-plan next selection,
// urgent-derived blockers, the >15 urgent status guidance, and shelved
// exclusion + closure. Unit-level coverage lives beside each behavior
// (urge02_tier_tests / urge03_blocker_tests / handlers_status urgent_review /
// the shelve tests); this ties them into one integration lock.
// ============================================================================

/// A fresh git-backed workspace (the store rides refs/ishoo/store, so a repo is
/// required) with no issues.
fn urge06_workspace() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    crate::model::init_workspace(dir.path()).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init", "--allow-empty"]);
    dir
}

/// Create an issue through `ishoo_new`. `category` fixes the id prefix (each
/// prefix has its own counter, so the first `foo` issue is `FOO-01`). Asserts the
/// creation succeeded.
fn urge06_new(
    dir: &std::path::Path,
    category: &str,
    plan: &str,
    labels: &[&str],
    depends_on: &[&str],
) {
    let mut args = json!({
        "title": category,
        "category": category,
        "plan": plan,
        "concrete_change": "c",
        "main_surface": "s",
        "proof_of_done": "p",
        "out_of_scope": "o",
        "decisions": [],
        "depends_on": depends_on,
    });
    if !labels.is_empty() {
        args["labels"] = json!(labels);
    }
    let resp = call_tool(dir, "ishoo_new", args);
    assert!(
        resp.get("error").map(Value::is_null).unwrap_or(true),
        "ishoo_new failed: {resp}"
    );
}

/// The `issue_id` recommended by `ishoo_plan {op:next}`, or `None` when nothing is
/// ready.
fn urge06_next(dir: &std::path::Path) -> Option<String> {
    let resp = call_tool(dir, "ishoo_plan", json!({ "op": "next" }));
    resp["result"]["structuredContent"]["issue_id"]
        .as_str()
        .map(str::to_string)
}

#[test]
fn urge06_matrix_cross_plan_urgent_then_within_plan_tier_ladder_then_shelved() {
    let dir = urge06_workspace();
    let p = dir.path();

    // Active plan "work" carries a full non-urgent tier ladder + a shelved item.
    urge06_new(p, "imp", "new:\"work\"", &["important"], &[]);
    urge06_new(p, "mid", "work", &["mid"], &[]);
    urge06_new(p, "unl", "work", &[], &[]);
    urge06_new(p, "lat", "work", &["later"], &[]);
    urge06_new(p, "shv", "work", &["shelved"], &[]);
    // An urgent issue sits in a DIFFERENT plan, "fires".
    urge06_new(p, "urg", "new:\"fires\"", &["urgent"], &[]);
    // Re-activate "work" (creating "fires" made it active).
    call_tool(p, "ishoo_plan", json!({ "op": "use", "name": "work" }));

    // 1) Cross-plan urgent interrupt: urgent in "fires" outranks all active work.
    assert_eq!(urge06_next(p).as_deref(), Some("URG-01"));
    let status = call_tool(p, "ishoo_status", json!({}));
    assert_eq!(
        status["result"]["structuredContent"]["recommended_next"], "ishoo start URG-01",
        "status must recommend the same urgent scheduler front as plan next"
    );

    // Peel the urgent off; the within-plan tier ladder then applies strictly:
    // important > mid > unlabeled > later.
    call_tool(p, "ishoo_delete", json!({ "id": "URG-01" }));
    assert_eq!(urge06_next(p).as_deref(), Some("IMP-01"), "important first");
    call_tool(p, "ishoo_delete", json!({ "id": "IMP-01" }));
    assert_eq!(urge06_next(p).as_deref(), Some("MID-01"), "then mid");
    call_tool(p, "ishoo_delete", json!({ "id": "MID-01" }));
    assert_eq!(
        urge06_next(p).as_deref(),
        Some("UNL-01"),
        "unlabeled beats later"
    );
    call_tool(p, "ishoo_delete", json!({ "id": "UNL-01" }));
    assert_eq!(urge06_next(p).as_deref(), Some("LAT-01"), "later is last");
    call_tool(p, "ishoo_delete", json!({ "id": "LAT-01" }));

    // Only the shelved issue remains ready -> excluded, so there is no next work.
    assert_eq!(urge06_next(p), None, "shelved issues are never recommended");
}

#[test]
fn urge06_matrix_urgent_blocker_propagation_and_status_and_shelved_closure() {
    let dir = urge06_workspace();
    let p = dir.path();

    urge06_new(p, "blk", "new:\"work\"", &[], &[]); // BLK-01 ready blocker (non-urgent)
    urge06_new(p, "urg", "work", &["urgent"], &["BLK-01"]); // URG-01 urgent, blocked by BLK-01
    urge06_new(p, "ord", "work", &[], &[]); // ORD-01 ordinary work

    // 2) Urgent pressure propagates through the blocker: the ready blocker BLK-01
    // outranks ordinary ORD-01 and is tagged with the source urgent id.
    let resp = call_tool(p, "ishoo_plan", json!({ "op": "next" }));
    let next = &resp["result"]["structuredContent"];
    assert_eq!(next["issue_id"], "BLK-01");
    assert_eq!(next["urgent_via"], "URG-01");

    // 3) Status urgent-review facts: URG-01 is urgent-but-blocked.
    let status = call_tool(p, "ishoo_status", json!({}));
    let review = &status["result"]["structuredContent"]["urgent_review"];
    assert_eq!(review["ready_count"], 0);
    assert_eq!(review["blocked_count"], 1);
    assert!(
        review["guidance"].as_str().unwrap().contains("blocked"),
        "{review}"
    );

    // 4) Shelved closure: label ORD-01 shelved, then shelve it without done gates.
    call_tool(
        p,
        "ishoo_edit",
        json!({ "id": "ORD-01", "labels": ["shelved"] }),
    );
    // Shelve refuses without a rationale.
    let no_reason = call_tool(p, "ishoo_shelve", json!({ "id": "ORD-01" }));
    assert_eq!(no_reason["error"]["code"], registry::INVALID_PARAMS);
    // With a rationale it retires, staying queryable as Declined.
    let ok = call_tool(
        p,
        "ishoo_shelve",
        json!({ "id": "ORD-01", "reason": "retained knowledge" }),
    );
    assert_eq!(ok["result"]["structuredContent"]["shelved"], true);
    let show = call_tool(p, "ishoo_show", json!({ "id": "ORD-01" }));
    assert_eq!(
        show["result"]["structuredContent"]["issue"]["status"],
        "Declined"
    );
}

#[test]
fn urge06_matrix_over_fifteen_ready_urgent_routes_to_candidate_workflow() {
    let dir = urge06_workspace();
    let p = dir.path();

    // 16 distinct ready urgent issues. Titles must differ so each call has a
    // distinct mutation id (identical args dedupe as "already_created").
    for i in 0..16 {
        let plan = if i == 0 { "new:\"fires\"" } else { "fires" };
        let resp = call_tool(
            p,
            "ishoo_new",
            json!({
                "title": format!("urgent fire {i}"),
                "category": "u",
                "plan": plan,
                "concrete_change": "c", "main_surface": "s",
                "proof_of_done": "p", "out_of_scope": "o",
                "decisions": [], "depends_on": [],
                "labels": ["urgent"],
            }),
        );
        assert!(
            resp.get("error").map(Value::is_null).unwrap_or(true),
            "ishoo_new failed: {resp}"
        );
    }

    let status = call_tool(p, "ishoo_status", json!({}));
    let review = &status["result"]["structuredContent"]["urgent_review"];
    assert_eq!(review["ready_count"], 16);
    assert_eq!(review["top_ready"].as_array().unwrap().len(), 5);
    let guidance = review["guidance"].as_str().unwrap();
    assert!(guidance.contains("ishoo_candidates"), "{guidance}");
    assert!(guidance.contains("charter"), "{guidance}");
}

#[test]
fn urge06_matrix_all_five_tier_labels_survive_creation() {
    let dir = urge06_workspace();
    let p = dir.path();

    // All five tier labels are canonical (URGE-01): none are stripped at creation.
    let resp = call_tool(
        p,
        "ishoo_new",
        json!({
            "title": "tiers",
            "category": "tier",
            "plan": "new:\"work\"",
            "concrete_change": "c", "main_surface": "s",
            "proof_of_done": "p", "out_of_scope": "o",
            "decisions": [], "depends_on": [],
            "labels": ["urgent", "important", "mid", "later", "shelved"],
        }),
    );
    assert!(
        resp["result"]["structuredContent"]["stripped_labels"].is_null(),
        "no tier label should be stripped: {resp}"
    );
    assert!(
        resp["result"]["structuredContent"]["urgency_assessment"].is_null(),
        "tiered create should not ask for another urgency assessment: {resp}"
    );
    let show = call_tool(p, "ishoo_show", json!({ "id": "TIER-01" }));
    let labels = show["result"]["structuredContent"]["issue"]["labels"]
        .as_array()
        .unwrap();
    for tier in ["urgent", "important", "mid", "later", "shelved"] {
        assert!(
            labels.iter().any(|l| l == tier),
            "{tier} must be persisted: {labels:?}"
        );
    }
}

// CORE-07 (DEC-47/DEC-49): the ishoo_decline and ishoo_supersede MCP tools mirror
// the CLI retirement verbs, enforcing the same rationale/replacement rules.
#[test]
fn decline_and_supersede_are_registered() {
    let reg = registry::registry();
    for name in ["ishoo_decline", "ishoo_supersede"] {
        assert!(
            reg.iter().any(|t| t.name == name),
            "{name} must be a registered MCP tool"
        );
    }
}

#[test]
fn decline_retires_the_issue_and_requires_a_nonempty_reason() {
    let dir = workspace_with_issue();

    // No reason at all -> rejected (schema/param).
    let no_reason = call_tool(dir.path(), "ishoo_decline", json!({ "id": "FIX-01" }));
    assert!(no_reason.get("error").is_some(), "reason is required");

    // Whitespace-only reason -> rejected at the core, not just the schema.
    let empty = call_tool(
        dir.path(),
        "ishoo_decline",
        json!({ "id": "FIX-01", "reason": "   " }),
    );
    assert_eq!(empty["error"]["code"], registry::INVALID_PARAMS);

    // A valid decline retires the issue and records the reason.
    let ok = call_tool(
        dir.path(),
        "ishoo_decline",
        json!({ "id": "FIX-01", "reason": "duplicate of an earlier idea" }),
    );
    assert_eq!(ok["result"]["structuredContent"]["status"], "declined");
    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = ws.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert_eq!(issue.status, crate::model::Status::Declined);
    assert!(issue.retire_reason.contains("duplicate"), "reason recorded");
}

#[test]
fn supersede_records_the_replacement_and_validates_it() {
    let dir = workspace_with_issue();
    // A second issue to serve as the replacement.
    call_tool(
        dir.path(),
        "ishoo_new",
        json!({
            "title": "Replacement", "category": "fix", "plan": "Work",
            "concrete_change": "x", "main_surface": "y", "proof_of_done": "z",
            "out_of_scope": "w", "decisions": [], "depends_on": []
        }),
    );

    // Missing replacement -> rejected.
    let no_repl = call_tool(
        dir.path(),
        "ishoo_supersede",
        json!({ "id": "FIX-01", "reason": "r" }),
    );
    assert!(no_repl.get("error").is_some(), "replacement is required");

    // An issue cannot supersede itself -> core validation error.
    let self_ref = call_tool(
        dir.path(),
        "ishoo_supersede",
        json!({ "id": "FIX-01", "replacement": "FIX-01", "reason": "r" }),
    );
    assert_eq!(self_ref["error"]["code"], registry::INVALID_PARAMS);

    // A valid supersede retires FIX-01 and records the replacement.
    let ok = call_tool(
        dir.path(),
        "ishoo_supersede",
        json!({ "id": "FIX-01", "replacement": "FIX-02", "reason": "replaced by FIX-02" }),
    );
    assert_eq!(
        ok["result"]["structuredContent"]["superseded_by"],
        "FIX-02"
    );
    let ws = crate::model::Workspace::load(dir.path()).unwrap();
    let issue = ws.issues.iter().find(|i| i.id == "FIX-01").unwrap();
    assert_eq!(issue.status, crate::model::Status::Declined);
    assert_eq!(issue.superseded_by.as_deref(), Some("FIX-02"));
}
