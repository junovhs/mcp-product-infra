//! ADPT-01 (DEC-88 / DEC-57): `ishoo enable` — materialize the repo's host-adapter
//! files so Claude Code and Codex pick up the Ishoo + SEMMAP MCP servers and know to
//! drive work through them, with no hand-copied config.
//!
//! Per DEC-88 these files materialize **only on explicit enable**, never on a tool
//! call / startup / UI open, and every merge is idempotent and non-clobbering:
//! - `.mcp.json` — add the `ishoo`/`semmap` server keys, never touching others.
//! - `.codex/config.toml` — append the `[mcp_servers.*]` tables if absent (text-append,
//!   so user formatting/comments and other tables are preserved exactly).
//! - `.claude/settings.local.json` / `.claude/.gitignore` — merge the local Claude
//!   host-adapter defaults users currently copy by hand.
//! - `CLAUDE.md` / `AGENTS.md` — upsert a marker-delimited managed block, leaving all
//!   user prose intact.
//!
//! Running twice is a no-op (nothing rewritten when already current).

use std::path::{Path, PathBuf};
use std::{env, fs};

/// Marker delimiters for the managed block in CLAUDE.md / AGENTS.md. Everything
/// between them is Ishoo-owned; everything else is the user's and never touched.
const MANAGED_BEGIN: &str = "<!-- ishoo:begin -->";
const MANAGED_END: &str = "<!-- ishoo:end -->";

/// The bootstrap instruction Ishoo owns inside the managed block. Mirrors what users
/// hand-copy today: call the briefs, drive work through the MCP tools, stop if a server
/// is unavailable.
const MANAGED_BODY: &str =
    "This repository is managed by Ishoo and mapped by SEMMAP. Before handling the first \
user request, call the `ishoo_brief` and `semmap_brief` MCP tools. Drive all issue, \
plan, and decision work through the `ishoo_*` MCP tools and code navigation through the \
`semmap_*` tools — do not substitute the Ishoo or SEMMAP command-line interfaces. If \
either MCP server or its brief tool is unavailable, stop and tell the user which server \
must be enabled before continuing.";

const CLAUDE_ALLOWED_COMMANDS: &[&str] = &[
    "Bash(*)",
    "Bash(cargo test:*)",
    "PowerShell(neti check *)",
    "PowerShell(ishoo set *)",
];

/// What `enable` did to one adapter file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterAction {
    /// The file did not exist and was created.
    Created,
    /// The file existed and its managed part was updated (user content preserved).
    Updated,
    /// The file was already current — nothing was rewritten (idempotent).
    Unchanged,
    /// The file was left untouched with the given reason (e.g. unparseable).
    Skipped(String),
}

impl AdapterAction {
    pub fn tag(&self) -> &'static str {
        match self {
            AdapterAction::Created => "created",
            AdapterAction::Updated => "updated",
            AdapterAction::Unchanged => "unchanged",
            AdapterAction::Skipped(_) => "skipped",
        }
    }
}

/// The typed result of an enable run: what happened to each adapter file, plus the
/// repo root they were written under.
#[derive(Debug, Clone)]
pub struct EnableReport {
    pub repo_root: PathBuf,
    pub files: Vec<(String, AdapterAction)>,
}

/// The typed result of a user/global host registration run.
#[derive(Debug, Clone)]
pub struct UserAdapterReport {
    pub files: Vec<(String, AdapterAction)>,
}

/// ADPT-02: one host's effective readiness, combining user/global registration,
/// repository adapter state, and the effective precedence between them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostReadinessReport {
    pub host: String,
    pub user_registration: HostConfigFact,
    pub repository_adapter: HostConfigFact,
    /// `user` | `repository` | `both` | `none`.
    pub effective_source: String,
    /// `reachable` | `unreachable` | `unchecked`.
    pub connectivity: String,
    pub ready: bool,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary_action: Option<String>,
}

/// ADPT-02: a typed config fact. `state` is intentionally a string in the wire
/// payload so future states can be added without reshaping `ishoo_status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostConfigFact {
    /// `current` | `absent` | `drifted` | `unreadable` | `shadowed`.
    pub state: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
struct UserAdapterPaths {
    codex_config: PathBuf,
    claude_json: PathBuf,
    ishoo_command: String,
    semmap_command: String,
}

/// Materialize every repo host-adapter file under the git repo containing `ws_root`.
/// Idempotent and non-clobbering (DEC-88). Errors only on an unresolvable repo root;
/// an individual unparseable file is reported as `Skipped`, never overwritten.
pub fn enable_repo_adapters(ws_root: &Path) -> Result<EnableReport, String> {
    let repo_root = find_git_root(ws_root)
        .ok_or_else(|| "not inside a git repository; run `ishoo enable` from a repo".to_string())?;

    let mut files = Vec::new();
    files.push((
        ".mcp.json".to_string(),
        with_action(&repo_root.join(".mcp.json"), || ensure_mcp_json(&repo_root))?,
    ));
    files.push((
        ".codex/config.toml".to_string(),
        with_action(&repo_root.join(".codex/config.toml"), || {
            ensure_codex_config(&repo_root)
        })?,
    ));
    files.push((
        ".claude/settings.local.json".to_string(),
        with_action(&repo_root.join(".claude/settings.local.json"), || {
            ensure_claude_settings(&repo_root)
        })?,
    ));
    files.push((
        ".claude/.gitignore".to_string(),
        with_action(&repo_root.join(".claude/.gitignore"), || {
            ensure_claude_gitignore(&repo_root)
        })?,
    ));
    files.push((
        "CLAUDE.md".to_string(),
        with_action(&repo_root.join("CLAUDE.md"), || {
            ensure_managed_markdown(&repo_root.join("CLAUDE.md"))
        })?,
    ));
    files.push((
        "AGENTS.md".to_string(),
        with_action(&repo_root.join("AGENTS.md"), || {
            ensure_managed_markdown(&repo_root.join("AGENTS.md"))
        })?,
    ));

    Ok(EnableReport { repo_root, files })
}

/// ADPT-03: register/repair user-scope MCP entries so Ishoo + SEMMAP are available
/// in every repo for this user on this machine. This is still explicit enablement:
/// no startup/tool-call path calls it.
pub fn enable_user_adapters() -> Result<UserAdapterReport, String> {
    enable_user_adapters_at(default_user_adapter_paths()?)
}

/// ADPT-03 uninstall/repair backstop: remove only entries that look Ishoo-owned.
pub fn remove_user_adapters() -> Result<UserAdapterReport, String> {
    remove_user_adapters_at(default_user_adapter_paths()?)
}

/// ADPT-02: read-only effective readiness facts for the supported hosts. This never
/// writes; it only parses the same user/repo config shapes `enable` manages.
pub fn host_readiness(repo_root: &Path) -> Vec<HostReadinessReport> {
    let paths = default_user_config_paths();
    host_readiness_at(repo_root, &paths)
}

fn enable_user_adapters_at(paths: UserAdapterPaths) -> Result<UserAdapterReport, String> {
    let mut files = Vec::new();
    files.push((
        paths.codex_config.display().to_string(),
        with_action(&paths.codex_config, || ensure_codex_user_config(&paths))?,
    ));
    files.push((
        paths.claude_json.display().to_string(),
        with_action(&paths.claude_json, || ensure_claude_user_config(&paths))?,
    ));
    Ok(UserAdapterReport { files })
}

fn remove_user_adapters_at(paths: UserAdapterPaths) -> Result<UserAdapterReport, String> {
    let mut files = Vec::new();
    files.push((
        paths.codex_config.display().to_string(),
        with_action(&paths.codex_config, || remove_codex_user_config(&paths))?,
    ));
    files.push((
        paths.claude_json.display().to_string(),
        with_action(&paths.claude_json, || remove_claude_user_config(&paths))?,
    ));
    Ok(UserAdapterReport { files })
}

#[derive(Debug, Clone)]
struct UserConfigPaths {
    codex_config: PathBuf,
    claude_json: PathBuf,
}

fn default_user_adapter_paths() -> Result<UserAdapterPaths, String> {
    let config = default_user_config_paths();
    Ok(UserAdapterPaths {
        codex_config: config.codex_config,
        claude_json: config.claude_json,
        ishoo_command: find_command_on_path("ishoo")
            .ok_or_else(|| "cannot find `ishoo` on PATH for user MCP registration".to_string())?
            .display()
            .to_string(),
        semmap_command: find_command_on_path("semmap")
            .ok_or_else(|| "cannot find `semmap` on PATH for user MCP registration".to_string())?
            .display()
            .to_string(),
    })
}

fn default_user_config_paths() -> UserConfigPaths {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    UserConfigPaths {
        codex_config: codex_home.join("config.toml"),
        claude_json: home.join(".claude.json"),
    }
}

fn find_command_on_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(fs::canonicalize(&candidate).unwrap_or(candidate));
        }
    }
    None
}

fn host_readiness_at(repo_root: &Path, user_paths: &UserConfigPaths) -> Vec<HostReadinessReport> {
    vec![
        build_host_readiness(
            "Claude Code",
            inspect_claude_user_config(&user_paths.claude_json),
            inspect_claude_repo_config(&repo_root.join(".mcp.json")),
        ),
        build_host_readiness(
            "Codex",
            inspect_codex_user_config(&user_paths.codex_config),
            inspect_codex_repo_config(&repo_root.join(".codex/config.toml")),
        ),
    ]
}

fn build_host_readiness(
    host: &str,
    user_registration: HostConfigFact,
    repository_adapter: HostConfigFact,
) -> HostReadinessReport {
    let user_current = user_registration.state == "current";
    let repo_current = repository_adapter.state == "current";
    let repo_blocks_user = user_current
        && matches!(
            repository_adapter.state.as_str(),
            "drifted" | "unreadable" | "shadowed"
        );

    let mut repository_adapter = repository_adapter;
    let (effective_source, ready, result, primary_action, secondary_action) = if repo_blocks_user {
        repository_adapter.state = "shadowed".to_string();
        if repository_adapter.detail.is_none() {
            repository_adapter.detail = Some(
                "repository adapter overrides the current user/global registration".to_string(),
            );
        }
        (
            "repository",
            false,
            "repository_override_blocks_global",
            Some("Repair repository setup".to_string()),
            Some("Remove repository override or add shared repository setup".to_string()),
        )
    } else if user_current && repo_current {
        (
            "both",
            true,
            "ready_both",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if user_current {
        (
            "user",
            true,
            "ready_globally",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if repo_current {
        (
            "repository",
            true,
            "ready_repository",
            None,
            None,
        )
    } else {
        (
            "none",
            false,
            "setup_required",
            Some("Set up this repo for agents".to_string()),
            Some("Register user-wide setup".to_string()),
        )
    };

    HostReadinessReport {
        host: host.to_string(),
        user_registration,
        repository_adapter,
        effective_source: effective_source.to_string(),
        // Status is observational and cheap. Real host connection probes are left to
        // host commands / transcript tests; this field still distinguishes that no
        // connection check was performed from a measured failure.
        connectivity: "unchecked".to_string(),
        ready,
        result: result.to_string(),
        primary_action,
        secondary_action,
    }
}

fn inspect_codex_user_config(path: &Path) -> HostConfigFact {
    inspect_codex_config(path, "Codex user config")
}

fn inspect_codex_repo_config(path: &Path) -> HostConfigFact {
    inspect_codex_config(path, "Codex repository config")
}

fn inspect_codex_config(path: &Path, label: &str) -> HostConfigFact {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return fact("absent", path, None),
    };
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(_) => return fact("unreadable", path, Some(format!("{label} is not parseable TOML"))),
    };
    let Some(servers) = table.get("mcp_servers").and_then(|v| v.as_table()) else {
        return fact("absent", path, None);
    };
    config_fact_from_servers(
        path,
        servers.get("ishoo").map(ServerValue::Toml),
        servers.get("semmap").map(ServerValue::Toml),
    )
}

fn inspect_claude_user_config(path: &Path) -> HostConfigFact {
    inspect_claude_config(path, "Claude user config")
}

fn inspect_claude_repo_config(path: &Path) -> HostConfigFact {
    inspect_claude_config(path, "Claude repository config")
}

fn inspect_claude_config(path: &Path, label: &str) -> HostConfigFact {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return fact("absent", path, None),
    };
    let doc = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) if v.is_object() => v,
        _ => return fact("unreadable", path, Some(format!("{label} is not parseable JSON"))),
    };
    let Some(servers) = doc.get("mcpServers").and_then(|v| v.as_object()) else {
        return fact("absent", path, None);
    };
    config_fact_from_servers(
        path,
        servers.get("ishoo").map(ServerValue::Json),
        servers.get("semmap").map(ServerValue::Json),
    )
}

#[derive(Clone, Copy)]
enum ServerValue<'a> {
    Json(&'a serde_json::Value),
    Toml(&'a toml::Value),
}

fn config_fact_from_servers(
    path: &Path,
    ishoo: Option<ServerValue<'_>>,
    semmap: Option<ServerValue<'_>>,
) -> HostConfigFact {
    match (ishoo, semmap) {
        (None, None) => fact("absent", path, None),
        (Some(i), Some(s)) if server_is_owned(i, "ishoo") && server_is_owned(s, "semmap") => {
            fact("current", path, None)
        }
        (Some(_), Some(_)) => fact(
            "drifted",
            path,
            Some("ishoo/semmap entries exist but do not match the expected MCP command shape".to_string()),
        ),
        (None, Some(_)) => fact("drifted", path, Some("missing ishoo MCP server".to_string())),
        (Some(_), None) => fact("drifted", path, Some("missing semmap MCP server".to_string())),
    }
}

fn server_is_owned(value: ServerValue<'_>, expected_name: &str) -> bool {
    match value {
        ServerValue::Json(value) => claude_server_is_owned(value, expected_name),
        ServerValue::Toml(value) => codex_server_is_owned(value, expected_name),
    }
}

fn fact(state: &str, path: &Path, detail: Option<String>) -> HostConfigFact {
    HostConfigFact {
        state: state.to_string(),
        path: path.display().to_string(),
        detail,
    }
}

/// Run an idempotent materializer and classify the effect by comparing the file's
/// bytes before and after — so every materializer stays a simple `ensure`, and the
/// Created/Updated/Unchanged/Skipped signal is derived uniformly (and detects
/// idempotence precisely: identical bytes → `Unchanged`).
fn with_action<F>(path: &Path, materialize: F) -> Result<AdapterAction, String>
where
    F: FnOnce() -> Result<Materialized, String>,
{
    let before = fs::read(path).ok();
    match materialize()? {
        Materialized::Skipped(reason) => Ok(AdapterAction::Skipped(reason)),
        Materialized::Wrote => {
            let after = fs::read(path).ok();
            Ok(match (before, after) {
                (None, Some(_)) => AdapterAction::Created,
                (Some(b), Some(a)) if b == a => AdapterAction::Unchanged,
                (Some(_), Some(_)) => AdapterAction::Updated,
                // A materializer that reported `Wrote` but left no file is a fault, not
                // a normal outcome; surface it rather than silently claiming success.
                (_, None) => AdapterAction::Skipped("no file after write".to_string()),
            })
        }
    }
}

/// Outcome of a single materializer: it either performed its idempotent ensure, or
/// declined to touch the file (with a reason) so nothing is clobbered.
enum Materialized {
    Wrote,
    Skipped(String),
}

/// `.mcp.json`: register the `ishoo` and `semmap` MCP servers for Claude Code. Delegates
/// to the shared merge in `model_paths` (adds only missing server keys; a custom entry
/// and an unparseable file are left untouched — the `with_action` byte-diff then reports
/// the effect).
fn ensure_mcp_json(repo_root: &Path) -> Result<Materialized, String> {
    crate::model::ensure_mcp_json(repo_root);
    Ok(Materialized::Wrote)
}

/// `.codex/config.toml`: register the `ishoo`/`semmap` MCP servers for Codex. Appends a
/// `[mcp_servers.<name>]` table only when absent — text-append, so all existing tables,
/// keys, comments, and formatting are preserved exactly (never a reformat/clobber).
fn ensure_codex_config(repo_root: &Path) -> Result<Materialized, String> {
    let dir = repo_root.join(".codex");
    let path = dir.join("config.toml");

    let existing = fs::read_to_string(&path).unwrap_or_default();
    // Determine which TOML tables are already present. An unparseable file is left alone.
    let table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        match existing.parse::<toml::Table>() {
            Ok(table) => table,
            Err(_) => {
                return Ok(Materialized::Skipped(
                    ".codex/config.toml is not parseable TOML".to_string(),
                ))
            }
        }
    };

    let mut additions = String::new();
    append_codex_server(&table, &mut additions, "ishoo", Some("ishoo_candidates"));
    append_codex_server(&table, &mut additions, "semmap", Some("semmap_generate"));

    if additions.is_empty() {
        return Ok(Materialized::Wrote); // already current — no-op (byte-identical).
    }

    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&additions);
    // Trim the trailing blank line we appended after the last table for tidiness.
    while text.ends_with("\n\n") {
        text.pop();
    }
    fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

fn append_codex_server(
    table: &toml::Table,
    additions: &mut String,
    name: &str,
    approval_tool: Option<&str>,
) {
    if !toml_path_exists(table, &["mcp_servers", name]) {
        additions.push_str(&format!(
            "[mcp_servers.{name}]\ncommand = \"{name}\"\nargs = [\"mcp\"]\n\n"
        ));
    }
    if let Some(tool) = approval_tool {
        if !toml_path_exists(table, &["mcp_servers", name, "tools", tool]) {
            additions.push_str(&format!(
                "[mcp_servers.{name}.tools.{tool}]\napproval_mode = \"approve\"\n\n"
            ));
        }
    }
}

fn toml_path_exists(table: &toml::Table, path: &[&str]) -> bool {
    let mut value = match table.get(path[0]) {
        Some(value) => value,
        None => return false,
    };
    for key in &path[1..] {
        value = match value.as_table().and_then(|t| t.get(*key)) {
            Some(value) => value,
            None => return false,
        };
    }
    value.as_table().is_some()
}

fn ensure_codex_user_config(paths: &UserAdapterPaths) -> Result<Materialized, String> {
    let path = &paths.codex_config;
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        match existing.parse::<toml::Table>() {
            Ok(table) => table,
            Err(_) => {
                return Ok(Materialized::Skipped(format!(
                    "{} is not parseable TOML",
                    path.display()
                )))
            }
        }
    };

    if let Err(reason) = ensure_codex_user_server(
        &mut table,
        "ishoo",
        &paths.ishoo_command,
        Some("ishoo_candidates"),
    ) {
        return Ok(Materialized::Skipped(reason));
    }
    if let Err(reason) = ensure_codex_user_server(
        &mut table,
        "semmap",
        &paths.semmap_command,
        Some("semmap_generate"),
    ) {
        return Ok(Materialized::Skipped(reason));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

fn ensure_codex_user_server(
    table: &mut toml::Table,
    name: &str,
    command: &str,
    approval_tool: Option<&str>,
) -> Result<(), String> {
    let servers = table
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(servers) = servers.as_table_mut() else {
        return Err("~/.codex/config.toml `mcp_servers` is not a table".to_string());
    };

    if let Some(existing) = servers.get(name) {
        if !codex_server_is_owned(existing, name) {
            return Err(format!(
                "Codex user config already has a non-Ishoo-owned `{name}` MCP server; leaving it untouched"
            ));
        }
    }

    let server = servers
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(server) = server.as_table_mut() else {
        return Err(format!(
            "Codex user config `mcp_servers.{name}` is not a table"
        ));
    };
    server.insert(
        "command".to_string(),
        toml::Value::String(command.to_string()),
    );
    server.insert(
        "args".to_string(),
        toml::Value::Array(vec![toml::Value::String("mcp".to_string())]),
    );
    if let Some(tool) = approval_tool {
        let tools = server
            .entry("tools".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(tools) = tools.as_table_mut() else {
            return Err(format!(
                "Codex user config `mcp_servers.{name}.tools` is not a table"
            ));
        };
        let tool_table = tools
            .entry(tool.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(tool_table) = tool_table.as_table_mut() else {
            return Err(format!(
                "Codex user config `mcp_servers.{name}.tools.{tool}` is not a table"
            ));
        };
        tool_table.insert(
            "approval_mode".to_string(),
            toml::Value::String("approve".to_string()),
        );
    }
    Ok(())
}

fn codex_server_is_owned(value: &toml::Value, expected_name: &str) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };
    let Some(command) = table.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let args_are_mcp = table
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| args.len() == 1 && args[0].as_str() == Some("mcp"));
    command_name_matches(command, expected_name) && args_are_mcp
}

fn remove_codex_user_config(paths: &UserAdapterPaths) -> Result<Materialized, String> {
    let path = &paths.codex_config;
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
    };
    let mut table = match existing.parse::<toml::Table>() {
        Ok(table) => table,
        Err(_) => {
            return Ok(Materialized::Skipped(format!(
                "{} is not parseable TOML",
                path.display()
            )))
        }
    };
    let mut changed = false;
    if let Some(servers) = table.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
        for name in ["ishoo", "semmap"] {
            let remove = servers
                .get(name)
                .is_some_and(|server| codex_server_is_owned(server, name));
            if remove {
                servers.remove(name);
                changed = true;
            }
        }
    }
    if changed {
        let mut text = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }
    Ok(Materialized::Wrote)
}

fn ensure_claude_user_config(paths: &UserAdapterPaths) -> Result<Materialized, String> {
    let path = &paths.claude_json;
    let mut doc = match fs::read_to_string(path) {
        Err(_) => serde_json::json!({ "mcpServers": {} }),
        Ok(existing) => match serde_json::from_str::<serde_json::Value>(&existing) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Ok(Materialized::Skipped(format!(
                    "{} is not parseable JSON",
                    path.display()
                )))
            }
        },
    };

    if let Err(reason) = ensure_claude_user_server(&mut doc, "ishoo", &paths.ishoo_command) {
        return Ok(Materialized::Skipped(reason));
    }
    if let Err(reason) = ensure_claude_user_server(&mut doc, "semmap", &paths.semmap_command) {
        return Ok(Materialized::Skipped(reason));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    text.push('\n');
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

fn ensure_claude_user_server(
    doc: &mut serde_json::Value,
    name: &str,
    command: &str,
) -> Result<(), String> {
    let Some(root) = doc.as_object_mut() else {
        return Err("~/.claude.json is not a JSON object".to_string());
    };
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err("~/.claude.json `mcpServers` is not an object".to_string());
    };
    if let Some(existing) = servers.get(name) {
        if !claude_server_is_owned(existing, name) {
            return Err(format!(
                "Claude user config already has a non-Ishoo-owned `{name}` MCP server; leaving it untouched"
            ));
        }
    }
    servers.insert(
        name.to_string(),
        serde_json::json!({
            "type": "stdio",
            "command": command,
            "args": ["mcp"],
            "env": {}
        }),
    );
    Ok(())
}

fn claude_server_is_owned(value: &serde_json::Value, expected_name: &str) -> bool {
    let command = value.get("command").and_then(|v| v.as_str());
    let args_are_mcp = value
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| args.len() == 1 && args[0].as_str() == Some("mcp"));
    let stdio_or_absent = value
        .get("type")
        .and_then(|v| v.as_str())
        .is_none_or(|t| t == "stdio");
    command.is_some_and(|c| command_name_matches(c, expected_name))
        && args_are_mcp
        && stdio_or_absent
}

fn remove_claude_user_config(paths: &UserAdapterPaths) -> Result<Materialized, String> {
    let path = &paths.claude_json;
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
    };
    let mut doc = match serde_json::from_str::<serde_json::Value>(&existing) {
        Ok(v) if v.is_object() => v,
        _ => {
            return Ok(Materialized::Skipped(format!(
                "{} is not parseable JSON",
                path.display()
            )))
        }
    };
    let mut changed = false;
    if let Some(servers) = doc.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        for name in ["ishoo", "semmap"] {
            let remove = servers
                .get(name)
                .is_some_and(|server| claude_server_is_owned(server, name));
            if remove {
                servers.remove(name);
                changed = true;
            }
        }
    }
    if changed {
        let mut text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
        text.push('\n');
        fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }
    Ok(Materialized::Wrote)
}

fn command_name_matches(command: &str, expected_name: &str) -> bool {
    command == expected_name
        || Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(expected_name)
}

/// `.claude/settings.local.json`: merge the local Claude permission defaults users
/// currently copy into a repo. Existing permissions stay; missing allow entries are
/// appended. Unparseable or structurally incompatible JSON is skipped, never clobbered.
fn ensure_claude_settings(repo_root: &Path) -> Result<Materialized, String> {
    let dir = repo_root.join(".claude");
    let path = dir.join("settings.local.json");

    let mut doc = match fs::read_to_string(&path) {
        Err(_) => serde_json::json!({ "permissions": { "allow": [] } }),
        Ok(existing) => match serde_json::from_str::<serde_json::Value>(&existing) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Ok(Materialized::Skipped(
                    ".claude/settings.local.json is not parseable JSON".to_string(),
                ))
            }
        },
    };

    let Some(root) = doc.as_object_mut() else {
        return Ok(Materialized::Skipped(
            ".claude/settings.local.json is not a JSON object".to_string(),
        ));
    };
    let permissions = root
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let Some(permissions) = permissions.as_object_mut() else {
        return Ok(Materialized::Skipped(
            ".claude/settings.local.json `permissions` is not an object".to_string(),
        ));
    };
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let Some(allow) = allow.as_array_mut() else {
        return Ok(Materialized::Skipped(
            ".claude/settings.local.json `permissions.allow` is not an array".to_string(),
        ));
    };

    for command in CLAUDE_ALLOWED_COMMANDS {
        if !allow.iter().any(|v| v.as_str() == Some(command)) {
            allow.push(serde_json::Value::String((*command).to_string()));
        }
    }

    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

/// `.claude/.gitignore`: keep Claude's local scheduled-task lock out of source control.
/// Existing ignore content is preserved; the lock entry is appended only if absent.
fn ensure_claude_gitignore(repo_root: &Path) -> Result<Materialized, String> {
    let dir = repo_root.join(".claude");
    let path = dir.join(".gitignore");
    let entry = "scheduled_tasks.lock";

    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(Materialized::Wrote);
    }

    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(entry);
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

/// `CLAUDE.md` / `AGENTS.md`: upsert the marker-delimited managed block. A missing file
/// is created with just the block; an existing file keeps all user prose and only the
/// block between the markers is inserted (once, at the top) or refreshed in place.
fn ensure_managed_markdown(path: &Path) -> Result<Materialized, String> {
    let block = format!("{MANAGED_BEGIN}\n{MANAGED_BODY}\n{MANAGED_END}\n");

    let new_text = match fs::read_to_string(path) {
        Err(_) => block, // missing -> just the managed block.
        Ok(existing) => match (existing.find(MANAGED_BEGIN), existing.find(MANAGED_END)) {
            // Existing managed block -> replace it in place, preserving surrounding prose.
            (Some(start), Some(end_marker_start)) if end_marker_start >= start => {
                let end = end_marker_start + MANAGED_END.len();
                let mut out = String::with_capacity(existing.len());
                out.push_str(&existing[..start]);
                out.push_str(block.trim_end_matches('\n'));
                out.push_str(&existing[end..]);
                out
            }
            // No managed block yet -> prepend it, keeping all existing content below.
            _ => format!("{block}\n{existing}"),
        },
    };

    // Idempotent: only write when the content actually changes.
    if fs::read_to_string(path)
        .map(|c| c == new_text)
        .unwrap_or(false)
    {
        return Ok(Materialized::Wrote);
    }
    fs::write(path, new_text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

/// Walk up from `start` to the directory containing `.git`.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        dir
    }

    fn action(report: &EnableReport, file: &str) -> AdapterAction {
        report
            .files
            .iter()
            .find(|(f, _)| f == file)
            .map(|(_, a)| a.clone())
            .unwrap_or_else(|| panic!("no report for {file}"))
    }

    fn user_action(report: &UserAdapterReport, file: &Path) -> AdapterAction {
        let file = file.display().to_string();
        report
            .files
            .iter()
            .find(|(f, _)| f == &file)
            .map(|(_, a)| a.clone())
            .unwrap_or_else(|| panic!("no report for {file}"))
    }

    fn user_paths(dir: &tempfile::TempDir, ishoo: &str, semmap: &str) -> UserAdapterPaths {
        UserAdapterPaths {
            codex_config: dir.path().join("codex-home/config.toml"),
            claude_json: dir.path().join("home/.claude.json"),
            ishoo_command: ishoo.to_string(),
            semmap_command: semmap.to_string(),
        }
    }

    fn user_config_paths(paths: &UserAdapterPaths) -> UserConfigPaths {
        UserConfigPaths {
            codex_config: paths.codex_config.clone(),
            claude_json: paths.claude_json.clone(),
        }
    }

    fn host<'a>(report: &'a [HostReadinessReport], name: &str) -> &'a HostReadinessReport {
        report
            .iter()
            .find(|h| h.host == name)
            .unwrap_or_else(|| panic!("missing host report for {name}"))
    }

    #[test]
    fn enable_on_empty_repo_creates_all_adapters() {
        let dir = git_repo();
        let report = enable_repo_adapters(dir.path()).unwrap();
        for f in [
            ".mcp.json",
            ".codex/config.toml",
            ".claude/settings.local.json",
            ".claude/.gitignore",
            "CLAUDE.md",
            "AGENTS.md",
        ] {
            assert_eq!(action(&report, f), AdapterAction::Created, "{f} created");
            assert!(dir.path().join(f).exists(), "{f} exists");
        }
        // .mcp.json registers both servers.
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["ishoo"].is_object());
        assert!(mcp["mcpServers"]["semmap"].is_object());
        // .codex/config.toml registers both servers.
        let codex = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(codex.contains("[mcp_servers.ishoo]"));
        assert!(codex.contains("[mcp_servers.ishoo.tools.ishoo_candidates]"));
        assert!(codex.contains("[mcp_servers.semmap]"));
        assert!(codex.contains("[mcp_servers.semmap.tools.semmap_generate]"));
        // Claude local settings and ignore defaults are materialized.
        let claude_settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude_settings["permissions"]["allow"][0], "Bash(*)");
        assert_eq!(
            fs::read_to_string(dir.path().join(".claude/.gitignore")).unwrap(),
            "scheduled_tasks.lock\n"
        );
        // The markdown carries the managed block.
        let claude = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(claude.contains(MANAGED_BEGIN) && claude.contains(MANAGED_END));
        assert!(claude.contains("ishoo_brief"));
    }

    #[test]
    fn enable_is_idempotent_second_run_is_a_no_op() {
        let dir = git_repo();
        enable_repo_adapters(dir.path()).unwrap();
        let files = [
            ".mcp.json",
            ".codex/config.toml",
            ".claude/settings.local.json",
            ".claude/.gitignore",
            "CLAUDE.md",
            "AGENTS.md",
        ];
        let snapshot: Vec<_> = files
            .iter()
            .map(|f| fs::read(dir.path().join(f)).unwrap())
            .collect();

        let report = enable_repo_adapters(dir.path()).unwrap();
        for f in files {
            assert_eq!(
                action(&report, f),
                AdapterAction::Unchanged,
                "{f} unchanged on re-run"
            );
        }
        let after: Vec<_> = files
            .iter()
            .map(|f| fs::read(dir.path().join(f)).unwrap())
            .collect();
        assert_eq!(snapshot, after, "re-running enable produces zero diff");
    }

    #[test]
    fn enable_preserves_user_content_and_foreign_servers() {
        let dir = git_repo();
        // A foreign MCP server the user configured, plus user prose in CLAUDE.md, plus a
        // hand-written .codex/config.toml with a comment and another server.
        fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"other"}}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("CLAUDE.md"),
            "# My project\n\nSome important house rules the user wrote.\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            "# my codex config\n[mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        fs::write(
            dir.path().join(".claude/settings.local.json"),
            r#"{"permissions":{"allow":["Bash(custom *)"]},"keep":true}"#,
        )
        .unwrap();
        fs::write(dir.path().join(".claude/.gitignore"), "already.ignored\n").unwrap();

        let report = enable_repo_adapters(dir.path()).unwrap();
        assert_eq!(action(&report, ".mcp.json"), AdapterAction::Updated);

        // Foreign server survives; ours added.
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(
            mcp["mcpServers"]["other"].is_object(),
            "foreign server preserved"
        );
        assert!(mcp["mcpServers"]["ishoo"].is_object());

        // User prose survives; managed block added.
        let claude = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(
            claude.contains("house rules the user wrote"),
            "user prose preserved"
        );
        assert!(claude.contains(MANAGED_BEGIN));

        // The user's comment and foreign codex server survive; ours appended.
        let codex = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(
            codex.contains("# my codex config"),
            "user comment preserved"
        );
        assert!(
            codex.contains("[mcp_servers.other]"),
            "foreign codex server preserved"
        );
        assert!(codex.contains("[mcp_servers.ishoo]"));
        assert!(codex.contains("[mcp_servers.semmap]"));
        // And it is still valid TOML.
        codex
            .parse::<toml::Table>()
            .expect("codex config stays valid TOML");

        // Existing Claude settings survive; missing allow entries are appended.
        let settings: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(allow.iter().any(|v| v.as_str() == Some("Bash(custom *)")));
        assert!(allow.iter().any(|v| v.as_str() == Some("Bash(*)")));
        assert_eq!(settings["keep"], true);
        let claude_ignore = fs::read_to_string(dir.path().join(".claude/.gitignore")).unwrap();
        assert!(claude_ignore.contains("already.ignored"));
        assert!(claude_ignore.contains("scheduled_tasks.lock"));
    }

    #[test]
    fn enable_leaves_a_custom_ishoo_entry_and_unparseable_files_untouched() {
        let dir = git_repo();
        // A custom ishoo command the user set — must not be overwritten.
        fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"ishoo":{"command":"/custom/ishoo","args":["mcp"]}}}"#,
        )
        .unwrap();
        // An unparseable codex config — must be skipped, never clobbered.
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            "this is : not = valid toml [[[",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        fs::write(
            dir.path().join(".claude/settings.local.json"),
            "not json {{{",
        )
        .unwrap();

        let report = enable_repo_adapters(dir.path()).unwrap();

        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["ishoo"]["command"], "/custom/ishoo",
            "custom entry kept"
        );
        assert!(
            mcp["mcpServers"]["semmap"].is_object(),
            "semmap still added"
        );

        assert!(
            matches!(
                action(&report, ".codex/config.toml"),
                AdapterAction::Skipped(_)
            ),
            "unparseable codex config is skipped"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap(),
            "this is : not = valid toml [[[",
            "unparseable codex config left untouched"
        );
        assert!(
            matches!(
                action(&report, ".claude/settings.local.json"),
                AdapterAction::Skipped(_)
            ),
            "unparseable claude settings are skipped"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap(),
            "not json {{{",
            "unparseable claude settings left untouched"
        );
    }

    #[test]
    fn enable_materializes_at_the_enclosing_git_root_from_a_nested_dir() {
        let dir = git_repo();
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let report = enable_repo_adapters(&nested).unwrap();
        assert_eq!(
            report.repo_root,
            dir.path(),
            "resolves to the enclosing git root"
        );
        // Files land at the repo root, never the nested working dir.
        assert!(dir.path().join(".mcp.json").exists());
        assert!(!nested.join(".mcp.json").exists());
    }

    #[test]
    fn user_enable_creates_global_codex_and_claude_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = user_paths(&dir, "/opt/ishoo/bin/ishoo", "/opt/ishoo/bin/semmap");

        let report = enable_user_adapters_at(paths.clone()).unwrap();
        assert_eq!(
            user_action(&report, &paths.codex_config),
            AdapterAction::Created
        );
        assert_eq!(
            user_action(&report, &paths.claude_json),
            AdapterAction::Created
        );

        let codex = fs::read_to_string(&paths.codex_config).unwrap();
        assert!(codex.contains("command = \"/opt/ishoo/bin/ishoo\""));
        assert!(codex.contains("[mcp_servers.ishoo.tools.ishoo_candidates]"));
        assert!(codex.contains("command = \"/opt/ishoo/bin/semmap\""));
        assert!(codex.contains("[mcp_servers.semmap.tools.semmap_generate]"));

        let claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.claude_json).unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["ishoo"]["type"], "stdio");
        assert_eq!(
            claude["mcpServers"]["ishoo"]["command"],
            "/opt/ishoo/bin/ishoo"
        );
        assert_eq!(
            claude["mcpServers"]["semmap"]["args"],
            serde_json::json!(["mcp"])
        );
    }

    #[test]
    fn user_enable_is_idempotent_and_repairs_owned_command_paths() {
        let dir = tempfile::tempdir().unwrap();
        let old_paths = user_paths(&dir, "/old/bin/ishoo", "/old/bin/semmap");
        enable_user_adapters_at(old_paths).unwrap();

        let new_paths = user_paths(&dir, "/new/bin/ishoo", "/new/bin/semmap");
        let report = enable_user_adapters_at(new_paths.clone()).unwrap();
        assert_eq!(
            user_action(&report, &new_paths.codex_config),
            AdapterAction::Updated
        );
        assert_eq!(
            user_action(&report, &new_paths.claude_json),
            AdapterAction::Updated
        );
        assert!(fs::read_to_string(&new_paths.codex_config)
            .unwrap()
            .contains("command = \"/new/bin/ishoo\""));
        let claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&new_paths.claude_json).unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["semmap"]["command"], "/new/bin/semmap");

        let snapshot = [
            fs::read(&new_paths.codex_config).unwrap(),
            fs::read(&new_paths.claude_json).unwrap(),
        ];
        let report = enable_user_adapters_at(new_paths.clone()).unwrap();
        assert_eq!(
            user_action(&report, &new_paths.codex_config),
            AdapterAction::Unchanged
        );
        assert_eq!(
            user_action(&report, &new_paths.claude_json),
            AdapterAction::Unchanged
        );
        assert_eq!(
            snapshot,
            [
                fs::read(&new_paths.codex_config).unwrap(),
                fs::read(&new_paths.claude_json).unwrap(),
            ],
            "second run is byte-identical"
        );
    }

    #[test]
    fn user_enable_preserves_foreign_entries_and_skips_conflicting_ishoo() {
        let dir = tempfile::tempdir().unwrap();
        let paths = user_paths(&dir, "/opt/ishoo", "/opt/semmap");
        fs::create_dir_all(paths.codex_config.parent().unwrap()).unwrap();
        fs::write(
            &paths.codex_config,
            "[mcp_servers.other]\ncommand = \"other\"\nargs = [\"mcp\"]\n\n[mcp_servers.ishoo]\ncommand = \"not-ishoo\"\nargs = [\"mcp\"]\n",
        )
        .unwrap();
        fs::create_dir_all(paths.claude_json.parent().unwrap()).unwrap();
        fs::write(
            &paths.claude_json,
            r#"{"keep":true,"mcpServers":{"other":{"command":"other","args":["mcp"]},"ishoo":{"type":"stdio","command":"not-ishoo","args":["mcp"]}}}"#,
        )
        .unwrap();

        let report = enable_user_adapters_at(paths.clone()).unwrap();
        assert!(matches!(
            user_action(&report, &paths.codex_config),
            AdapterAction::Skipped(_)
        ));
        assert!(matches!(
            user_action(&report, &paths.claude_json),
            AdapterAction::Skipped(_)
        ));
        assert!(fs::read_to_string(&paths.codex_config)
            .unwrap()
            .contains("[mcp_servers.other]"));
        let claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.claude_json).unwrap()).unwrap();
        assert_eq!(claude["keep"], true);
        assert_eq!(claude["mcpServers"]["ishoo"]["command"], "not-ishoo");
    }

    #[test]
    fn user_remove_deletes_only_owned_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = user_paths(&dir, "/opt/ishoo", "/opt/semmap");
        enable_user_adapters_at(paths.clone()).unwrap();

        let mut codex = fs::read_to_string(&paths.codex_config).unwrap();
        codex.push_str("\n[mcp_servers.other]\ncommand = \"other\"\nargs = [\"mcp\"]\n");
        fs::write(&paths.codex_config, codex).unwrap();
        let mut claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.claude_json).unwrap()).unwrap();
        claude["mcpServers"]["other"] = serde_json::json!({"command": "other", "args": ["mcp"]});
        fs::write(
            &paths.claude_json,
            serde_json::to_string_pretty(&claude).unwrap(),
        )
        .unwrap();

        let report = remove_user_adapters_at(paths.clone()).unwrap();
        assert_eq!(
            user_action(&report, &paths.codex_config),
            AdapterAction::Updated
        );
        assert_eq!(
            user_action(&report, &paths.claude_json),
            AdapterAction::Updated
        );
        let codex = fs::read_to_string(&paths.codex_config).unwrap();
        assert!(!codex.contains("[mcp_servers.ishoo]"));
        assert!(!codex.contains("[mcp_servers.semmap]"));
        assert!(codex.contains("[mcp_servers.other]"));
        let claude: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.claude_json).unwrap()).unwrap();
        assert!(claude["mcpServers"]["ishoo"].is_null());
        assert!(claude["mcpServers"]["semmap"].is_null());
        assert_eq!(claude["mcpServers"]["other"]["command"], "other");
    }

    #[test]
    fn readiness_reports_setup_required_when_no_user_or_repo_config_exists() {
        let repo = git_repo();
        let user = tempfile::tempdir().unwrap();
        let paths = user_paths(&user, "/opt/ishoo", "/opt/semmap");

        let report = host_readiness_at(repo.path(), &user_config_paths(&paths));

        for name in ["Claude Code", "Codex"] {
            let h = host(&report, name);
            assert!(!h.ready, "{name} is not ready without any config");
            assert_eq!(h.effective_source, "none");
            assert_eq!(h.result, "setup_required");
            assert_eq!(h.user_registration.state, "absent");
            assert_eq!(h.repository_adapter.state, "absent");
            assert_eq!(h.primary_action.as_deref(), Some("Set up this repo for agents"));
        }
    }

    #[test]
    fn readiness_reports_global_only_as_ready_without_repo_prompt() {
        let repo = git_repo();
        let user = tempfile::tempdir().unwrap();
        let paths = user_paths(&user, "/opt/ishoo", "/opt/semmap");
        enable_user_adapters_at(paths.clone()).unwrap();

        let report = host_readiness_at(repo.path(), &user_config_paths(&paths));

        for name in ["Claude Code", "Codex"] {
            let h = host(&report, name);
            assert!(h.ready, "{name} is ready through user config");
            assert_eq!(h.effective_source, "user");
            assert_eq!(h.result, "ready_globally");
            assert_eq!(h.user_registration.state, "current");
            assert_eq!(h.repository_adapter.state, "absent");
            assert!(h.primary_action.is_none(), "no repair prompt for global-only readiness");
            assert_eq!(
                h.secondary_action.as_deref(),
                Some("Add shared repository setup")
            );
        }
    }

    #[test]
    fn readiness_reports_repo_only_and_both_as_ready() {
        let repo = git_repo();
        enable_repo_adapters(repo.path()).unwrap();
        let user = tempfile::tempdir().unwrap();
        let paths = user_paths(&user, "/opt/ishoo", "/opt/semmap");

        let repo_only = host_readiness_at(repo.path(), &user_config_paths(&paths));
        for name in ["Claude Code", "Codex"] {
            let h = host(&repo_only, name);
            assert!(h.ready, "{name} is ready through repo config");
            assert_eq!(h.effective_source, "repository");
            assert_eq!(h.result, "ready_repository");
            assert_eq!(h.repository_adapter.state, "current");
        }

        enable_user_adapters_at(paths.clone()).unwrap();
        let both = host_readiness_at(repo.path(), &user_config_paths(&paths));
        for name in ["Claude Code", "Codex"] {
            let h = host(&both, name);
            assert!(h.ready, "{name} remains ready with both configs");
            assert_eq!(h.effective_source, "both");
            assert_eq!(h.result, "ready_both");
            assert_eq!(h.user_registration.state, "current");
            assert_eq!(h.repository_adapter.state, "current");
        }
    }

    #[test]
    fn readiness_reports_repo_override_as_shadowing_global_registration() {
        let repo = git_repo();
        let user = tempfile::tempdir().unwrap();
        let paths = user_paths(&user, "/opt/ishoo", "/opt/semmap");
        enable_user_adapters_at(paths.clone()).unwrap();

        fs::write(
            repo.path().join(".mcp.json"),
            r#"{"mcpServers":{"ishoo":{"command":"not-ishoo","args":["mcp"]},"semmap":{"command":"semmap","args":["mcp"]}}}"#,
        )
        .unwrap();
        fs::create_dir_all(repo.path().join(".codex")).unwrap();
        fs::write(
            repo.path().join(".codex/config.toml"),
            "[mcp_servers.ishoo]\ncommand = \"not-ishoo\"\nargs = [\"mcp\"]\n\n[mcp_servers.semmap]\ncommand = \"semmap\"\nargs = [\"mcp\"]\n",
        )
        .unwrap();

        let report = host_readiness_at(repo.path(), &user_config_paths(&paths));
        for name in ["Claude Code", "Codex"] {
            let h = host(&report, name);
            assert!(!h.ready, "{name} global config is blocked by repo override");
            assert_eq!(h.effective_source, "repository");
            assert_eq!(h.result, "repository_override_blocks_global");
            assert_eq!(h.user_registration.state, "current");
            assert_eq!(h.repository_adapter.state, "shadowed");
            assert_eq!(h.primary_action.as_deref(), Some("Repair repository setup"));
        }
    }

    #[test]
    fn readiness_reports_unreadable_configs_as_unreadable_not_missing() {
        let repo = git_repo();
        let user = tempfile::tempdir().unwrap();
        let paths = user_paths(&user, "/opt/ishoo", "/opt/semmap");
        fs::create_dir_all(paths.codex_config.parent().unwrap()).unwrap();
        fs::write(&paths.codex_config, "not = valid [[[ toml").unwrap();
        fs::create_dir_all(paths.claude_json.parent().unwrap()).unwrap();
        fs::write(&paths.claude_json, "not json {{{").unwrap();
        fs::write(repo.path().join(".mcp.json"), "not json {{{").unwrap();
        fs::create_dir_all(repo.path().join(".codex")).unwrap();
        fs::write(repo.path().join(".codex/config.toml"), "bad [[[ toml").unwrap();

        let report = host_readiness_at(repo.path(), &user_config_paths(&paths));

        let claude = host(&report, "Claude Code");
        assert_eq!(claude.user_registration.state, "unreadable");
        assert_eq!(claude.repository_adapter.state, "unreadable");
        let codex = host(&report, "Codex");
        assert_eq!(codex.user_registration.state, "unreadable");
        assert_eq!(codex.repository_adapter.state, "unreadable");
    }
}
