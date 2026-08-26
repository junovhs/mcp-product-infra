//! Host config adapters for Claude Code and Codex.
//!
//! Copy-first extraction source: `origin/ishoo/src/model/adapters.rs`.
//! The retained behavior is intentionally conservative:
//! - explicit install only
//! - repo and user/global scopes
//! - no-clobber merges
//! - owned-entry detection before update/remove
//! - skipped file-level failures instead of whole-run clobbering
//! - readiness facts that explain effective host setup

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const MANAGED_BEGIN: &str = "<!-- mcp-product-infra:begin -->";
const MANAGED_END: &str = "<!-- mcp-product-infra:end -->";

/// How a host reaches the server. ADPT-02: a registration is stdio XOR http —
/// modelling it as an enum keeps "a command and a url" unrepresentable, which
/// matters because Codex picks stdio the moment a `command` key exists and then
/// rejects the `url` beside it as invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostTransport {
    /// The host spawns the server as a child process and owns its lifetime.
    Stdio { command: String, args: Vec<String> },
    /// The host connects to an already-running server at a URL and owns nothing.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// A host whose config this crate writes.
///
/// ADPT-05: hosts are named rather than implied so a server can be declared for
/// one and withheld from another. Before this existed, `install_repo` wrote every
/// server to every host file, which is only safe when the hosts compose their
/// scopes the same way — and they do not (see [`ScopeComposition`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Host {
    ClaudeCode,
    Codex,
}

/// How a host combines its user-scope and repo-scope entry for the same server.
///
/// This is the rule that makes per-host declaration necessary, so it is modelled
/// explicitly instead of living in a comment: getting it wrong is silent, and the
/// symptom (a host that cannot connect) does not point back at the config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeComposition {
    /// Codex: the two entries merge field-wise. A repo entry may safely omit what
    /// the user entry supplies — a tokenless repo entry still authenticates with
    /// the user scope's `Authorization` header.
    Merge,
    /// Claude Code: the repo entry replaces the user entry wholesale. Anything the
    /// repo entry omits is simply absent, so a tokenless repo entry connects with
    /// no credential and is refused — even when a good user-scope entry exists.
    /// A credentialed server therefore belongs in a per-project registration
    /// ([`HostInstall::install_user_project_at`]), not in a tracked repo file.
    Replace,
}

impl Host {
    pub fn scope_composition(self) -> ScopeComposition {
        match self {
            Host::ClaudeCode => ScopeComposition::Replace,
            Host::Codex => ScopeComposition::Merge,
        }
    }
}

fn all_hosts() -> BTreeSet<Host> {
    BTreeSet::from([Host::ClaudeCode, Host::Codex])
}

/// Build one of these with [`HostServer::stdio`] or [`HostServer::http`] and the
/// chaining setters — it is `#[non_exhaustive]` because scope state like
/// `repo_hosts` must stay internally consistent, and because a struct literal
/// downstream would break on every field added here (ADPT-05 added one).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostServer {
    pub name: String,
    pub transport: HostTransport,
    /// Hosts this server is written to at *repository* scope. Defaults to every
    /// host; narrow it with [`HostServer::repo_scope_hosts`] when a host's
    /// [`ScopeComposition`] makes a tracked repo entry harmful. User and
    /// per-project scopes are selected by which install call you make, so this
    /// governs `install_repo` only.
    pub repo_hosts: BTreeSet<Host>,
    /// Earlier registrations of this same server that this one replaces. An entry
    /// matching any of these still counts as ours, so switching transports is a
    /// migration rather than a collision with a stranger's config. Declared
    /// explicitly — we never guess that an unfamiliar entry is safe to rewrite.
    pub superseded: Vec<HostTransport>,
    pub env: BTreeMap<String, String>,
    pub codex_approval_tools: Vec<String>,
}

impl HostServer {
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            transport: HostTransport::Stdio {
                command: command.into(),
                args: args.into_iter().map(Into::into).collect(),
            },
            repo_hosts: all_hosts(),
            superseded: Vec::new(),
            env: BTreeMap::new(),
            codex_approval_tools: Vec::new(),
        }
    }

    /// ADPT-02: register a server the host connects to rather than spawns, so a
    /// host crash or cancel cannot take the server down with it.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: HostTransport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
            repo_hosts: all_hosts(),
            superseded: Vec::new(),
            env: BTreeMap::new(),
            codex_approval_tools: Vec::new(),
        }
    }

    /// Add an HTTP request header (e.g. `Authorization: Bearer <token>`). A no-op
    /// on a stdio server, which has no request headers to carry.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let HostTransport::Http { headers, .. } = &mut self.transport {
            headers.insert(key.into(), value.into());
        }
        self
    }

    /// Restrict which hosts receive this server at repository scope.
    ///
    /// The default is every host. Narrow it when a tracked repo entry would be
    /// wrong for a host — most often a credentialed HTTP server under a
    /// [`ScopeComposition::Replace`] host, where the repo entry would shadow a
    /// working user-scope entry with an unauthenticated one, and where writing the
    /// credential into the repo file instead would commit a secret.
    ///
    /// An empty set means the server is written at no repository scope at all.
    pub fn repo_scope_hosts(mut self, hosts: impl IntoIterator<Item = Host>) -> Self {
        self.repo_hosts = hosts.into_iter().collect();
        self
    }

    fn writes_to_repo_scope(&self, host: Host) -> bool {
        self.repo_hosts.contains(&host)
    }

    /// Declare an earlier registration this server replaces, so `enable` migrates
    /// it in place instead of reporting a stranger's entry and backing off.
    pub fn supersedes(mut self, previous: HostTransport) -> Self {
        self.superseded.push(previous);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn approve_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.codex_approval_tools.push(tool_name.into());
        self
    }
}

/// A Claude Code hook entry the install materializes into the repo's
/// `.claude/settings.local.json` — e.g. a PreToolUse guard the app's binary
/// serves (`ishoo agent-guard`). Matched-by-command for ownership, so foreign
/// hooks on the same event/matcher are never touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeHook {
    pub event: String,
    pub matcher: String,
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct HostInstall {
    pub app_name: String,
    pub servers: Vec<HostServer>,
    pub managed_markdown_body: Option<String>,
    pub managed_markdown_markers: Option<(String, String)>,
    pub backup_existing_managed_markdown: bool,
    pub claude_allowed_commands: Vec<String>,
    pub claude_hooks: Vec<ClaudeHook>,
}

impl HostInstall {
    pub fn new(app_name: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            servers: Vec::new(),
            managed_markdown_body: None,
            managed_markdown_markers: None,
            backup_existing_managed_markdown: false,
            claude_allowed_commands: Vec::new(),
            claude_hooks: Vec::new(),
        }
    }

    pub fn server(mut self, server: HostServer) -> Self {
        self.servers.push(server);
        self
    }

    pub fn managed_markdown_body(mut self, body: impl Into<String>) -> Self {
        self.managed_markdown_body = Some(body.into());
        self
    }

    /// Override the delimiters used for the managed CLAUDE.md / AGENTS.md block.
    ///
    /// The default product-owned markers remain `mcp-product-infra:begin/end`.
    /// Consumers with an established ownership contract can retain their own markers
    /// so an upgrade refreshes the existing block instead of adding a second one.
    pub fn managed_markdown_markers(
        mut self,
        begin: impl Into<String>,
        end: impl Into<String>,
    ) -> Self {
        self.managed_markdown_markers = Some((begin.into(), end.into()));
        self
    }

    /// Preserve a one-time `<file>.bak` before modifying an existing instruction file.
    pub fn backup_existing_managed_markdown(mut self) -> Self {
        self.backup_existing_managed_markdown = true;
        self
    }

    pub fn claude_allow(mut self, command: impl Into<String>) -> Self {
        self.claude_allowed_commands.push(command.into());
        self
    }

    /// Register a Claude Code hook to materialize into the repo's
    /// `.claude/settings.local.json` (e.g. `("PreToolUse", "Bash", "app agent-guard")`).
    /// Merged idempotently: identified by its exact command string, appended to an
    /// existing matcher group when one exists, and foreign hooks are never modified.
    pub fn claude_hook(
        mut self,
        event: impl Into<String>,
        matcher: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        self.claude_hooks.push(ClaudeHook {
            event: event.into(),
            matcher: matcher.into(),
            command: command.into(),
        });
        self
    }

    pub fn install_repo(&self, repo_root: &Path) -> Result<InstallReport, String> {
        let repo_root =
            find_git_root(repo_root).ok_or_else(|| "not inside a git repository".to_string())?;
        let mut files = Vec::new();
        files.push((
            ".mcp.json".to_string(),
            with_action(&repo_root.join(".mcp.json"), || {
                self.ensure_mcp_json(&repo_root)
            })?,
        ));
        files.push((
            ".codex/config.toml".to_string(),
            with_action(&repo_root.join(".codex/config.toml"), || {
                self.ensure_codex_repo_config(&repo_root)
            })?,
        ));
        files.push((
            ".codex/.gitignore".to_string(),
            with_action(&repo_root.join(".codex/.gitignore"), || {
                ensure_codex_gitignore(&repo_root)
            })?,
        ));
        files.push((
            ".claude/settings.local.json".to_string(),
            with_action(&repo_root.join(".claude/settings.local.json"), || {
                self.ensure_claude_settings(&repo_root)
            })?,
        ));
        files.push((
            ".claude/.gitignore".to_string(),
            with_action(&repo_root.join(".claude/.gitignore"), || {
                ensure_claude_gitignore(&repo_root)
            })?,
        ));
        if self.managed_markdown_body.is_some() {
            files.push((
                "CLAUDE.md".to_string(),
                with_action(&repo_root.join("CLAUDE.md"), || {
                    self.ensure_managed_markdown(&repo_root.join("CLAUDE.md"))
                })?,
            ));
            files.push((
                "AGENTS.md".to_string(),
                with_action(&repo_root.join("AGENTS.md"), || {
                    self.ensure_managed_markdown(&repo_root.join("AGENTS.md"))
                })?,
            ));
        }
        Ok(InstallReport {
            root: repo_root,
            files,
        })
    }

    /// The servers written to `host` at repository scope, in declaration order.
    fn repo_servers_for(&self, host: Host) -> impl Iterator<Item = &HostServer> {
        self.servers
            .iter()
            .filter(move |server| server.writes_to_repo_scope(host))
    }

    /// The servers deliberately withheld from `host` at repository scope. An
    /// install must sweep these, because a previous install may have written them.
    fn excluded_repo_servers_for(&self, host: Host) -> impl Iterator<Item = &HostServer> {
        self.servers
            .iter()
            .filter(move |server| !server.writes_to_repo_scope(host))
    }

    pub fn install_user(&self) -> Result<InstallReport, String> {
        let paths = default_user_config_paths();
        self.install_user_at(&paths.codex_config, &paths.claude_json)
            .map(|mut report| {
                report.root = paths.home;
                report
            })
    }

    /// Materialize user-scope registrations at explicit config paths.
    ///
    /// This supports host integrations that supply their own home/config roots in
    /// tests or embedded environments without mutating the process environment.
    pub fn install_user_at(
        &self,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Result<InstallReport, String> {
        let files = vec![
            (
                codex_config.display().to_string(),
                with_action(codex_config, || self.ensure_codex_user_config(codex_config))?,
            ),
            (
                claude_json.display().to_string(),
                with_action(claude_json, || self.ensure_claude_user_config(claude_json))?,
            ),
        ];
        Ok(InstallReport {
            root: codex_config
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            files,
        })
    }

    /// Materialize a Claude Code registration scoped to one project.
    ///
    /// Claude Code keys per-project MCP servers by canonical repository root under
    /// `projects.<root>.mcpServers` in `~/.claude.json`. That file is private and
    /// untracked, which makes it the only scope where a repo-specific entry can
    /// carry a credential: the tracked `.mcp.json` would both leak the secret and,
    /// under [`ScopeComposition::Replace`], shadow the user scope when it omits it.
    ///
    /// Only the named project's `mcpServers` map is touched. The top-level
    /// `mcpServers` map, every other project, and every other key inside this
    /// project (Claude keeps unrelated per-project state there) are preserved.
    pub fn install_user_project(&self, project_root: &Path) -> Result<InstallReport, String> {
        let paths = default_user_config_paths();
        self.install_user_project_at(&paths.claude_json, project_root)
    }

    /// [`HostInstall::install_user_project`] against an explicit `~/.claude.json`.
    pub fn install_user_project_at(
        &self,
        claude_json: &Path,
        project_root: &Path,
    ) -> Result<InstallReport, String> {
        let key = project_scope_key(project_root);
        let files = vec![(
            format!("{} [{}]", claude_json.display(), key),
            with_action(claude_json, || {
                self.ensure_claude_project_config(claude_json, &key)
            })?,
        )];
        Ok(InstallReport {
            root: project_root.to_path_buf(),
            files,
        })
    }

    /// Remove only owned per-project Claude registrations for `project_root`.
    pub fn remove_user_project_at(
        &self,
        claude_json: &Path,
        project_root: &Path,
    ) -> Result<InstallReport, String> {
        let key = project_scope_key(project_root);
        let files = vec![(
            format!("{} [{}]", claude_json.display(), key),
            with_action(claude_json, || {
                self.remove_claude_project_config(claude_json, &key)
            })?,
        )];
        Ok(InstallReport {
            root: project_root.to_path_buf(),
            files,
        })
    }

    pub fn remove_user(&self) -> Result<InstallReport, String> {
        let paths = default_user_config_paths();
        self.remove_user_at(&paths.codex_config, &paths.claude_json)
            .map(|mut report| {
                report.root = paths.home;
                report
            })
    }

    /// Remove only owned user-scope registrations at explicit config paths.
    pub fn remove_user_at(
        &self,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Result<InstallReport, String> {
        let files = vec![
            (
                codex_config.display().to_string(),
                with_action(codex_config, || self.remove_codex_user_config(codex_config))?,
            ),
            (
                claude_json.display().to_string(),
                with_action(claude_json, || self.remove_claude_user_config(claude_json))?,
            ),
        ];
        Ok(InstallReport {
            root: codex_config
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            files,
        })
    }

    pub fn readiness(&self, repo_root: &Path) -> Vec<HostReadinessReport> {
        let paths = default_user_config_paths();
        self.readiness_at(repo_root, &paths.codex_config, &paths.claude_json)
    }

    /// Report host readiness using explicit user-scope config paths.
    pub fn readiness_at(
        &self,
        repo_root: &Path,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Vec<HostReadinessReport> {
        vec![
            build_readiness(
                "Claude Code",
                inspect_claude_config(claude_json, &self.servers),
                inspect_claude_config(
                    &repo_root.join(".mcp.json"),
                    &self.repo_expected(Host::ClaudeCode),
                ),
            ),
            build_readiness(
                "Codex",
                inspect_codex_config(codex_config, &self.servers),
                inspect_codex_config(
                    &repo_root.join(".codex/config.toml"),
                    &self.repo_expected(Host::Codex),
                ),
            ),
        ]
    }

    /// What a host's repository adapter is expected to contain. Readiness must ask
    /// the same question the install answers, or a server deliberately withheld from
    /// a host reads as drift and a healthy user registration reads as needing setup.
    fn repo_expected(&self, host: Host) -> Vec<HostServer> {
        self.repo_servers_for(host).cloned().collect()
    }

    /// Report readiness when user/global registrations intentionally launch a
    /// different server shape than repository adapters (for example, a
    /// machine-wide hub versus a repository-scoped server). `self` describes
    /// the repository adapter; `user_install` describes the user registration.
    pub fn readiness_at_with_user_install(
        &self,
        user_install: &HostInstall,
        repo_root: &Path,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Vec<HostReadinessReport> {
        vec![
            build_readiness(
                "Claude Code",
                inspect_claude_config(claude_json, &user_install.servers),
                inspect_claude_config(
                    &repo_root.join(".mcp.json"),
                    &self.repo_expected(Host::ClaudeCode),
                ),
            ),
            build_readiness(
                "Codex",
                inspect_codex_config(codex_config, &user_install.servers),
                inspect_codex_config(
                    &repo_root.join(".codex/config.toml"),
                    &self.repo_expected(Host::Codex),
                ),
            ),
        ]
    }

    fn ensure_mcp_json(&self, repo_root: &Path) -> Result<Materialized, String> {
        let path = repo_root.join(".mcp.json");
        let mut doc = match fs::read_to_string(&path) {
            Err(_) => json!({ "mcpServers": {} }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(
                        ".mcp.json is not parseable JSON".to_string(),
                    ))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                ".mcp.json is not a JSON object".to_string(),
            ));
        };
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return Ok(Materialized::Skipped(
                ".mcp.json `mcpServers` is not an object".to_string(),
            ));
        };
        for server in self.repo_servers_for(Host::ClaudeCode) {
            match servers.get(&server.name) {
                // Already exactly what we write: nothing to do.
                Some(existing) if claude_entry_is_current(existing, server) => {}
                // Ours, but an earlier registration we have declared superseded —
                // migrate it in place rather than leaving the stale transport.
                Some(existing) if claude_server_is_owned(existing, server) => {
                    servers.insert(server.name.clone(), claude_server_json(server));
                }
                // Someone else's entry under our name: never touch it.
                Some(_) => {}
                None => {
                    servers.insert(server.name.clone(), claude_server_json(server));
                }
            }
        }
        // Converge an installation that predates the host restriction: an entry we
        // wrote before still shadows the user scope, so withholding the server from
        // this host has to remove it, not merely stop writing it. Only ever an entry
        // that is provably ours.
        for server in self.excluded_repo_servers_for(Host::ClaudeCode) {
            if servers
                .get(&server.name)
                .is_some_and(|existing| claude_server_is_owned(existing, server))
            {
                servers.remove(&server.name);
            }
        }
        write_json(&path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn ensure_codex_repo_config(&self, repo_root: &Path) -> Result<Materialized, String> {
        let dir = repo_root.join(".codex");
        if let Some(skipped) = unusable_directory(&dir) {
            return Ok(skipped);
        }
        let path = dir.join("config.toml");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let table = match parse_toml_materialized(&existing, ".codex/config.toml") {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        // First migrate any entry that is ours but expresses a superseded
        // transport. Done as a targeted text replacement so the user's comments
        // and foreign servers survive, which reserializing the document would not.
        let mut text = existing;
        let mut migrated = false;
        for server in self.repo_servers_for(Host::Codex) {
            let existing_entry = table
                .get("mcp_servers")
                .and_then(|v| v.as_table())
                .and_then(|servers| servers.get(&server.name));
            let Some(entry) = existing_entry else {
                continue;
            };
            let Some(entry_table) = entry.as_table() else {
                continue;
            };
            // Skip only an entry that is already exactly what we write. One that
            // is ours but carries stale keys falls through to be replaced.
            if codex_entry_is_current(entry_table, server) || !codex_server_is_owned(entry, server)
            {
                continue;
            }
            if let Some(updated) =
                replace_codex_server_block(&text, &server.name, &render_codex_server(server))
            {
                text = updated;
                migrated = true;
            }
        }
        // Symmetric convergence: drop an owned block for a server this host no
        // longer receives, so narrowing the host set repairs existing installs
        // instead of leaving the old entry behind.
        for server in self.excluded_repo_servers_for(Host::Codex) {
            let owned = table
                .get("mcp_servers")
                .and_then(|v| v.as_table())
                .and_then(|servers| servers.get(&server.name))
                .is_some_and(|entry| codex_server_is_owned(entry, server));
            if !owned {
                continue;
            }
            if let Some(updated) = replace_codex_server_block(&text, &server.name, "") {
                text = updated;
                migrated = true;
            }
        }

        let table = if migrated {
            match parse_toml_materialized(&text, ".codex/config.toml") {
                Ok(table) => table,
                Err(skipped) => return Ok(skipped),
            }
        } else {
            table
        };

        let mut additions = String::new();
        for server in self.repo_servers_for(Host::Codex) {
            append_codex_server(&table, &mut additions, server);
        }
        if additions.is_empty() && !migrated {
            return Ok(Materialized::Wrote);
        }
        fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        if additions.is_empty() {
            fs::write(&path, &text)
                .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
            return Ok(Materialized::Wrote);
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&additions);
        while text.ends_with("\n\n") {
            text.pop();
        }
        fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(Materialized::Wrote)
    }

    fn ensure_codex_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = fs::read_to_string(path).unwrap_or_default();
        let mut table = match parse_toml_materialized(&existing, &path.display().to_string()) {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        let servers = table
            .entry("mcp_servers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(servers) = servers.as_table_mut() else {
            return Ok(Materialized::Skipped(
                "Codex `mcp_servers` is not a table".to_string(),
            ));
        };
        for server in &self.servers {
            if let Some(existing) = servers.get(&server.name) {
                if !codex_server_is_owned(existing, server) {
                    return Ok(Materialized::Skipped(format!(
                        "Codex already has a non-owned `{}` MCP server; leaving it untouched",
                        server.name
                    )));
                }
            }
            servers.insert(server.name.clone(), codex_server_toml(server));
        }
        write_toml(path, &table)?;
        Ok(Materialized::Wrote)
    }

    fn remove_codex_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = match fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
        };
        let mut table = match parse_toml_materialized(&existing, &path.display().to_string()) {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        let mut changed = false;
        if let Some(servers) = table.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
            for server in &self.servers {
                if servers
                    .get(&server.name)
                    .is_some_and(|v| codex_server_is_owned(v, server))
                {
                    servers.remove(&server.name);
                    changed = true;
                }
            }
        }
        if changed {
            write_toml(path, &table)?;
        }
        Ok(Materialized::Wrote)
    }

    fn ensure_claude_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let mut doc = match fs::read_to_string(path) {
            Err(_) => json!({ "mcpServers": {} }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(format!(
                        "{} is not parseable JSON",
                        path.display()
                    )))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude config is not an object".to_string(),
            ));
        };
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `mcpServers` is not an object".to_string(),
            ));
        };
        if let Some(skipped) = upsert_claude_servers(servers, &self.servers) {
            return Ok(skipped);
        }
        write_json(path, &doc)?;
        Ok(Materialized::Wrote)
    }

    /// Write this install's servers into `projects.<key>.mcpServers`, creating the
    /// project entry when absent and disturbing nothing else in the file.
    fn ensure_claude_project_config(&self, path: &Path, key: &str) -> Result<Materialized, String> {
        let mut doc = match fs::read_to_string(path) {
            // Only a genuinely absent file starts from an empty document. A file we
            // cannot read (invalid UTF-8, permissions) is reported, never replaced —
            // treating it as absent would overwrite the user's whole config.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(e) => {
                return Ok(Materialized::Skipped(format!(
                    "{} could not be read: {e}",
                    path.display()
                )))
            }
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(format!(
                        "{} is not parseable JSON",
                        path.display()
                    )))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude config is not an object".to_string(),
            ));
        };
        let projects = root.entry("projects").or_insert_with(|| json!({}));
        let Some(projects) = projects.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `projects` is not an object".to_string(),
            ));
        };
        // Preserve any unrelated per-project state Claude keeps beside `mcpServers`.
        let project = projects.entry(key.to_string()).or_insert_with(|| json!({}));
        let Some(project) = project.as_object_mut() else {
            return Ok(Materialized::Skipped(format!(
                "Claude project entry for {key} is not an object"
            )));
        };
        let servers = project.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return Ok(Materialized::Skipped(format!(
                "Claude `mcpServers` for project {key} is not an object"
            )));
        };
        if let Some(skipped) = upsert_claude_servers(servers, &self.servers) {
            return Ok(skipped);
        }
        write_json(path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn remove_claude_project_config(&self, path: &Path, key: &str) -> Result<Materialized, String> {
        let existing = match fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
        };
        let mut doc = match serde_json::from_str::<Value>(&existing) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Ok(Materialized::Skipped(format!(
                    "{} is not parseable JSON",
                    path.display()
                )))
            }
        };
        let changed = doc
            .get_mut("projects")
            .and_then(|v| v.as_object_mut())
            .and_then(|projects| projects.get_mut(key))
            .and_then(|project| project.get_mut("mcpServers"))
            .and_then(|v| v.as_object_mut())
            .is_some_and(|servers| remove_owned_claude_servers(servers, &self.servers));
        if changed {
            write_json(path, &doc)?;
        }
        Ok(Materialized::Wrote)
    }

    fn remove_claude_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = match fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
        };
        let mut doc = match serde_json::from_str::<Value>(&existing) {
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
            changed = remove_owned_claude_servers(servers, &self.servers);
        }
        if changed {
            write_json(path, &doc)?;
        }
        Ok(Materialized::Wrote)
    }

    fn ensure_claude_settings(&self, repo_root: &Path) -> Result<Materialized, String> {
        let dir = repo_root.join(".claude");
        let path = dir.join("settings.local.json");
        let mut doc = match fs::read_to_string(&path) {
            Err(_) => json!({ "permissions": { "allow": [] } }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
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
                "Claude settings is not an object".to_string(),
            ));
        };
        let permissions = root.entry("permissions").or_insert_with(|| json!({}));
        let Some(permissions) = permissions.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `permissions` is not an object".to_string(),
            ));
        };
        let allow = permissions.entry("allow").or_insert_with(|| json!([]));
        let Some(allow) = allow.as_array_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `permissions.allow` is not an array".to_string(),
            ));
        };
        for command in &self.claude_allowed_commands {
            if !allow.iter().any(|v| v.as_str() == Some(command)) {
                allow.push(Value::String(command.clone()));
            }
        }
        if let Err(skipped) = merge_claude_hooks(root, &self.claude_hooks) {
            return Ok(skipped);
        }
        fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        write_json(&path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn ensure_managed_markdown(&self, path: &Path) -> Result<Materialized, String> {
        let body = self.managed_markdown_body.clone().unwrap_or_default();
        let (begin, end) = self
            .managed_markdown_markers
            .as_ref()
            .map(|(begin, end)| (begin.as_str(), end.as_str()))
            .unwrap_or((MANAGED_BEGIN, MANAGED_END));
        let block = format!("{begin}\n{body}\n{end}\n");
        let existing = fs::read_to_string(path).ok();
        let new_text = match &existing {
            None => block,
            Some(existing) => match (existing.find(begin), existing.find(end)) {
                (Some(start), Some(end_marker_start)) if end_marker_start >= start => {
                    let end_marker_end = end_marker_start + end.len();
                    let mut out = String::with_capacity(existing.len());
                    out.push_str(&existing[..start]);
                    out.push_str(block.trim_end_matches('\n'));
                    out.push_str(&existing[end_marker_end..]);
                    out
                }
                _ => format!("{block}\n{existing}"),
            },
        };
        if existing.as_ref().is_some_and(|text| text == &new_text) {
            return Ok(Materialized::Wrote);
        }
        if self.backup_existing_managed_markdown {
            if let Some(existing) = &existing {
                let mut backup = path.as_os_str().to_os_string();
                backup.push(".bak");
                let backup = PathBuf::from(backup);
                if !backup.exists() {
                    fs::write(&backup, existing)
                        .map_err(|e| format!("failed to write {}: {e}", backup.display()))?;
                }
            }
        }
        fs::write(path, new_text)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(Materialized::Wrote)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterAction {
    Created,
    Updated,
    Unchanged,
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

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub root: PathBuf,
    pub files: Vec<(String, AdapterAction)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfigFact {
    /// `current` | `absent` | `drifted` | `unreadable` | `shadowed`.
    pub state: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReadinessReport {
    pub host: String,
    pub user_registration: HostConfigFact,
    pub repository_adapter: HostConfigFact,
    /// `user` | `repository` | `both` | `none`.
    pub effective_source: String,
    /// `reachable` | `unreachable` | `unchecked`.
    ///
    /// Always `unchecked` today: nothing in this crate probes a host. Callers
    /// reach this report over the very transport whose health it would describe,
    /// so its arrival is already stronger proof of reachability than a probe
    /// could give, and a broken transport yields no report at all.
    pub connectivity: String,
    /// Whether the expected registration was found in the inspected config, and
    /// nothing more (ADPT-11).
    ///
    /// Deliberately NOT named `ready`. DEC-03 keeps process liveness, transport
    /// reachability, and application readiness as distinct states; this is a
    /// purely textual read of config files, so calling it readiness collapses
    /// reachability into configuration. On 2026-08-16 a host that could not
    /// authenticate at all reported `ready: true` / `ready_both` — every fact in
    /// the report was correct and only the word on top of them was false.
    pub configured: bool,
    /// `configured_both` | `configured_globally` | `configured_repository` |
    /// `setup_required` | `repository_override_blocks_global`.
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary_action: Option<String>,
}

enum Materialized {
    Wrote,
    Skipped(String),
}

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
                (_, None) => AdapterAction::Skipped("no file after write".to_string()),
            })
        }
    }
}

/// Render a server's complete Codex tables. Used both to append a missing entry
/// and to replace an owned-but-stale one during a transport migration.
fn render_codex_server(server: &HostServer) -> String {
    let mut out = String::new();
    match &server.transport {
        HostTransport::Stdio { command, args } => out.push_str(&format!(
            "[mcp_servers.{}]\ncommand = {:?}\nargs = [{}]\n\n",
            server.name,
            command,
            args.iter()
                .map(|arg| format!("{arg:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        HostTransport::Http { url, headers } => {
            out.push_str(&format!(
                "[mcp_servers.{}]\nurl = {:?}\n\n",
                server.name, url
            ));
            // The header table must follow the parent table's keys, never precede
            // them — a `[a.b]` header closes `[a]`, so emitting headers first
            // would strand `url` inside `http_headers`.
            if !headers.is_empty() {
                out.push_str(&format!("[mcp_servers.{}.http_headers]\n", server.name));
                for (key, value) in headers {
                    // Quote the key: HTTP header names may contain characters
                    // that are not valid TOML bare keys, and an unquoted `.`
                    // would silently become a nested table.
                    out.push_str(&format!("{key:?} = {value:?}\n"));
                }
                out.push('\n');
            }
        }
    }
    for tool in &server.codex_approval_tools {
        out.push_str(&format!(
            "[mcp_servers.{}.tools.{}]\napproval_mode = \"approve\"\n\n",
            server.name, tool
        ));
    }
    out
}

fn append_codex_server(table: &toml::Table, additions: &mut String, server: &HostServer) {
    if !toml_path_exists(table, &["mcp_servers", &server.name]) {
        additions.push_str(&render_codex_server(server));
        return;
    }
    // The entry exists; only its approval tables may still be missing.
    for tool in &server.codex_approval_tools {
        if !toml_path_exists(table, &["mcp_servers", &server.name, "tools", tool]) {
            additions.push_str(&format!(
                "[mcp_servers.{}.tools.{}]\napproval_mode = \"approve\"\n\n",
                server.name, tool
            ));
        }
    }
}

/// Replace every `[mcp_servers.<name>]` table (and its sub-tables) in `text` with
/// `replacement`, preserving everything else in the file byte-for-byte — comments
/// and foreign servers included. Returns None when the server is not present.
///
/// This exists because the repo config is maintained by text append precisely so
/// a user's comments survive; a migration still has to rewrite one entry in place,
/// and reserializing the whole document would discard those comments.
fn replace_codex_server_block(text: &str, name: &str, replacement: &str) -> Option<String> {
    let owned_header = format!("[mcp_servers.{name}]");
    let sub_prefix = format!("[mcp_servers.{name}.");
    let mut out: Vec<String> = Vec::new();
    let mut inserted = false;
    let mut skipping = false;
    let mut found = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // A new table header always ends any block we were skipping.
            skipping = trimmed == owned_header || trimmed.starts_with(&sub_prefix);
            if skipping {
                found = true;
                if !inserted {
                    inserted = true;
                    // An empty replacement deletes the block. Emitting no separator
                    // then leaves the surrounding blank lines as the user wrote them.
                    if !replacement.trim_end().is_empty() {
                        for replacement_line in replacement.trim_end().lines() {
                            out.push(replacement_line.to_string());
                        }
                        out.push(String::new());
                    }
                }
                continue;
            }
        }
        if !skipping {
            out.push(line.to_string());
        }
    }
    if !found {
        return None;
    }
    let mut result = out.join("\n");
    while result.ends_with("\n\n") {
        result.pop();
    }
    if !result.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
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

/// Is something already occupying `dir` that is not a directory we can write into?
///
/// `create_dir_all` reports that as `File exists (os error 17)`, and an `Err` out
/// of one adapter aborts the whole `install_repo` run, so a single stray path
/// costs the caller every other adapter file. Detecting it here keeps the failure
/// at file level, matching this module's policy that an unusable file is reported
/// as `Skipped` and never overwritten — we do not own it, so we do not remove it.
///
/// Checked through `symlink_metadata` so a dangling symlink (which `exists()`
/// reports as absent, yet `create_dir_all` still refuses) is caught, while a
/// symlink pointing at a real directory is left to proceed normally.
fn unusable_directory(dir: &Path) -> Option<Materialized> {
    if dir.is_dir() || fs::symlink_metadata(dir).is_err() {
        return None;
    }
    Some(Materialized::Skipped(format!(
        "{} exists but is not a directory; leaving it untouched",
        dir.display()
    )))
}

fn parse_toml_materialized(existing: &str, label: &str) -> Result<toml::Table, Materialized> {
    if existing.trim().is_empty() {
        return Ok(toml::Table::new());
    }
    existing
        .parse::<toml::Table>()
        .map_err(|_| Materialized::Skipped(format!("{label} is not parseable TOML")))
}

fn codex_server_toml(server: &HostServer) -> toml::Value {
    let mut table = toml::Table::new();
    match &server.transport {
        HostTransport::Stdio { command, args } => {
            table.insert("command".to_string(), toml::Value::String(command.clone()));
            table.insert(
                "args".to_string(),
                toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
            );
        }
        HostTransport::Http { url, headers } => {
            table.insert("url".to_string(), toml::Value::String(url.clone()));
            if !headers.is_empty() {
                table.insert(
                    "http_headers".to_string(),
                    toml::Value::Table(
                        headers
                            .iter()
                            .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                            .collect(),
                    ),
                );
            }
        }
    }
    if !server.codex_approval_tools.is_empty() {
        let mut tools = toml::Table::new();
        for tool in &server.codex_approval_tools {
            let mut mode = toml::Table::new();
            mode.insert(
                "approval_mode".to_string(),
                toml::Value::String("approve".to_string()),
            );
            tools.insert(tool.clone(), toml::Value::Table(mode));
        }
        table.insert("tools".to_string(), toml::Value::Table(tools));
    }
    toml::Value::Table(table)
}

/// The key Claude Code files a project's config under: its canonical root path.
///
/// Canonicalization is best-effort — a root that does not exist yet is keyed by
/// the path as given rather than failing the install.
fn project_scope_key(project_root: &Path) -> String {
    fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .display()
        .to_string()
}

/// Upsert every declared server into one Claude `mcpServers` map.
///
/// Returns `Some(Skipped)` when an entry under our name belongs to someone else:
/// the map is left as it was and the caller writes nothing. Shared by the user and
/// per-project scopes so both obey one ownership rule.
fn upsert_claude_servers(
    servers: &mut serde_json::Map<String, Value>,
    declared: &[HostServer],
) -> Option<Materialized> {
    for server in declared {
        if let Some(existing) = servers.get(&server.name) {
            if !claude_server_is_owned(existing, server) {
                return Some(Materialized::Skipped(format!(
                    "Claude already has a non-owned `{}` MCP server; leaving it untouched",
                    server.name
                )));
            }
        }
        servers.insert(server.name.clone(), claude_server_json(server));
    }
    None
}

/// Drop only owned entries from one Claude `mcpServers` map. Returns whether
/// anything was removed.
fn remove_owned_claude_servers(
    servers: &mut serde_json::Map<String, Value>,
    declared: &[HostServer],
) -> bool {
    let mut changed = false;
    for server in declared {
        if servers
            .get(&server.name)
            .is_some_and(|v| claude_server_is_owned(v, server))
        {
            servers.remove(&server.name);
            changed = true;
        }
    }
    changed
}

fn claude_server_json(server: &HostServer) -> Value {
    // Emit the minimal `{command, args}` entry — the implicit stdio `type` and an
    // empty `env` are omitted so output stays byte-identical to a hand-authored
    // registration and re-running enable on an existing file is a true no-op.
    // `env` is included only when the server actually carries variables.
    // `claude_server_is_owned` treats an absent `type` as stdio, so ownership
    // detection is unaffected.
    let mut entry = serde_json::Map::new();
    match &server.transport {
        HostTransport::Stdio { command, args } => {
            entry.insert("command".into(), Value::String(command.clone()));
            entry.insert(
                "args".into(),
                Value::Array(args.iter().cloned().map(Value::String).collect()),
            );
        }
        // ADPT-02: an HTTP entry must carry an explicit `type` — unlike stdio it
        // is not the implicit default, so omitting it would read as a malformed
        // stdio entry with no command.
        HostTransport::Http { url, headers } => {
            entry.insert("type".into(), Value::String("http".to_string()));
            entry.insert("url".into(), Value::String(url.clone()));
            if !headers.is_empty() {
                entry.insert(
                    "headers".into(),
                    Value::Object(
                        headers
                            .iter()
                            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                            .collect(),
                    ),
                );
            }
        }
    }
    if !server.env.is_empty() {
        entry.insert(
            "env".into(),
            Value::Object(
                server
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
    }
    Value::Object(entry)
}

/// How exactly an entry's transport has to line up with the one we declare.
///
/// Ownership and currency ask different questions of the same entry, and the
/// difference is not cosmetic: an HTTP registration's URL carries a `?root=`
/// query pinning an absolute repository path (ADPT-14), which is a fact about
/// one machine's filesystem. Judging ownership on the whole URL therefore makes
/// a config authored on another machine — a colleague's checkout, a second
/// machine, any clone at a different path — read as a stranger's entry, so the
/// installer skips it and can never repair it. The user sees `enable` report
/// the file `unchanged` while readiness simultaneously reports it `shadowed`.
///
/// Stdio has always drawn this line in the same place: `command_name_matches`
/// compares file names precisely because the absolute path to the executable is
/// machine-local too. [`MatchPrecision::Identity`] is that existing principle
/// applied to the HTTP transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchPrecision {
    /// Is this the same server, ignoring the parts that legitimately differ from
    /// one machine to the next? This is the ownership question.
    Identity,
    /// Is this byte-for-byte the registration we write today? This is the
    /// currency question, and a stale root must fail it so the entry is rewritten.
    Exact,
}

/// The part of an HTTP registration URL that identifies the *server* rather than
/// the machine: everything before the query string. The query holds `?root=`,
/// which differs per checkout and must not decide ownership.
fn http_identity(url: &str) -> &str {
    match url.split_once('?') {
        Some((identity, _)) => identity,
        None => url,
    }
}

/// Does this config entry express this transport, at the requested precision?
fn codex_matches_transport(
    table: &toml::Table,
    transport: &HostTransport,
    precision: MatchPrecision,
) -> bool {
    match transport {
        HostTransport::Stdio { command, args } => {
            // A stdio entry must not also carry a url: Codex picks stdio when a
            // `command` exists and then rejects the url beside it, so a mixed
            // entry is broken, not ours.
            table.get("url").is_none()
                && table
                    .get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(|actual| command_name_matches(actual, command))
                && table
                    .get("args")
                    .and_then(|v| v.as_array())
                    .is_some_and(|actual| {
                        actual
                            .iter()
                            .filter_map(|v| v.as_str())
                            .eq(args.iter().map(String::as_str))
                    })
        }
        // An HTTP entry is judged on its URL alone. The header table holds a
        // bearer token rotated independently of the config, so comparing it would
        // make every rotation read as foreign drift.
        HostTransport::Http { url, .. } => {
            let Some(actual) = table.get("url").and_then(|v| v.as_str()) else {
                return false;
            };
            table.get("command").is_none()
                && match precision {
                    MatchPrecision::Identity => http_identity(actual) == http_identity(url),
                    MatchPrecision::Exact => actual == url,
                }
        }
    }
}

/// Is this entry ours — either the registration we write today, or one we have
/// explicitly declared that it replaces? A superseded match is what makes a
/// transport switch a migration instead of a collision with a foreign entry.
fn codex_server_is_owned(value: &toml::Value, expected: &HostServer) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };
    codex_matches_transport(table, &expected.transport, MatchPrecision::Identity)
        || expected
            .superseded
            .iter()
            .any(|previous| codex_matches_transport(table, previous, MatchPrecision::Identity))
}

fn claude_matches_transport(
    value: &Value,
    transport: &HostTransport,
    precision: MatchPrecision,
) -> bool {
    match transport {
        HostTransport::Stdio { command, args } => {
            let stdio_or_absent = match value.get("type").and_then(|v| v.as_str()) {
                Some(t) => t == "stdio",
                None => true,
            };
            stdio_or_absent
                && value
                    .get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(|actual| command_name_matches(actual, command))
                && value
                    .get("args")
                    .and_then(|v| v.as_array())
                    .is_some_and(|actual| {
                        actual
                            .iter()
                            .filter_map(|v| v.as_str())
                            .eq(args.iter().map(String::as_str))
                    })
        }
        HostTransport::Http { url, .. } => {
            let Some(actual) = value.get("url").and_then(|v| v.as_str()) else {
                return false;
            };
            value.get("type").and_then(|v| v.as_str()) == Some("http")
                && match precision {
                    MatchPrecision::Identity => http_identity(actual) == http_identity(url),
                    MatchPrecision::Exact => actual == url,
                }
        }
    }
}

fn claude_server_is_owned(value: &Value, expected: &HostServer) -> bool {
    claude_matches_transport(value, &expected.transport, MatchPrecision::Identity)
        || expected
            .superseded
            .iter()
            .any(|previous| claude_matches_transport(value, previous, MatchPrecision::Identity))
}

/// The exact top-level keys the current rendering emits for `server`.
///
/// Ownership and currency are different questions. `codex_matches_transport` /
/// `claude_matches_transport` answer "is this entry ours?" from transport
/// identity alone — deliberately, because an HTTP entry's header values hold a
/// credential rotated independently of the config, so comparing them would make
/// every rotation read as foreign drift. But an entry can be ours and still
/// carry keys we no longer write. Comparing the key SET closes that gap without
/// reintroducing value comparison on the parts that legitimately move.
fn expected_codex_keys(server: &HostServer) -> BTreeSet<&'static str> {
    let mut keys = BTreeSet::new();
    match &server.transport {
        HostTransport::Stdio { .. } => {
            keys.insert("command");
            keys.insert("args");
        }
        HostTransport::Http { headers, .. } => {
            keys.insert("url");
            if !headers.is_empty() {
                keys.insert("http_headers");
            }
        }
    }
    if !server.codex_approval_tools.is_empty() {
        keys.insert("tools");
    }
    keys
}

fn expected_claude_keys(server: &HostServer) -> BTreeSet<&'static str> {
    let mut keys = BTreeSet::new();
    match &server.transport {
        HostTransport::Stdio { .. } => {
            keys.insert("command");
            keys.insert("args");
        }
        HostTransport::Http { headers, .. } => {
            keys.insert("type");
            keys.insert("url");
            if !headers.is_empty() {
                keys.insert("headers");
            }
        }
    }
    if !server.env.is_empty() {
        keys.insert("env");
    }
    keys
}

/// Is this entry already exactly the registration we write today — same
/// transport AND no key beyond what we emit? An owned entry that fails this is
/// stale and must be rewritten, so `install_repo` converges on our rendering
/// instead of merely being satisfied by it.
fn codex_entry_is_current(table: &toml::Table, server: &HostServer) -> bool {
    codex_matches_transport(table, &server.transport, MatchPrecision::Exact)
        && table.keys().map(String::as_str).collect::<BTreeSet<_>>() == expected_codex_keys(server)
}

fn claude_entry_is_current(value: &Value, server: &HostServer) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    claude_matches_transport(value, &server.transport, MatchPrecision::Exact)
        && object.keys().map(String::as_str).collect::<BTreeSet<_>>()
            == expected_claude_keys(server)
}

fn command_name_matches(actual: &str, expected: &str) -> bool {
    actual == expected
        || Path::new(actual).file_name().and_then(|name| name.to_str())
            == Path::new(expected)
                .file_name()
                .and_then(|name| name.to_str())
}

fn inspect_claude_config(path: &Path, expected: &[HostServer]) -> HostConfigFact {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return fact("absent", path, None),
    };
    let doc: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return fact("unreadable", path, Some("not parseable JSON".to_string())),
    };
    let Some(servers) = doc.get("mcpServers").and_then(|v| v.as_object()) else {
        return fact("absent", path, Some("missing mcpServers".to_string()));
    };
    for server in expected {
        match servers.get(&server.name) {
            Some(value) if claude_server_is_owned(value, server) => {}
            Some(_) => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("{} exists but is not owned/current", server.name)),
                )
            }
            None => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("missing {} MCP server", server.name)),
                )
            }
        }
    }
    fact("current", path, None)
}

fn inspect_codex_config(path: &Path, expected: &[HostServer]) -> HostConfigFact {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return fact("absent", path, None),
    };
    let table: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(_) => return fact("unreadable", path, Some("not parseable TOML".to_string())),
    };
    let Some(servers) = table.get("mcp_servers").and_then(|v| v.as_table()) else {
        return fact("absent", path, Some("missing mcp_servers".to_string()));
    };
    for server in expected {
        match servers.get(&server.name) {
            Some(value) if codex_server_is_owned(value, server) => {}
            Some(_) => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("{} exists but is not owned/current", server.name)),
                )
            }
            None => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("missing {} MCP server", server.name)),
                )
            }
        }
    }
    fact("current", path, None)
}

fn build_readiness(
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
    let (effective_source, configured, result, primary_action, secondary_action) = if repo_blocks_user {
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
            "configured_both",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if user_current {
        (
            "user",
            true,
            "configured_globally",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if repo_current {
        ("repository", true, "configured_repository", None, None)
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
        connectivity: "unchecked".to_string(),
        configured,
        result: result.to_string(),
        primary_action,
        secondary_action,
    }
}

fn fact(state: &str, path: &Path, detail: Option<String>) -> HostConfigFact {
    HostConfigFact {
        state: state.to_string(),
        path: path.display().to_string(),
        detail,
    }
}

/// Merge the install's Claude hooks into a settings document root. A hook is
/// identified by its exact command string; existing matcher groups gain the
/// command, foreign hooks and unknown keys are preserved byte-for-byte, and a
/// structurally alien `hooks` shape skips the file rather than clobbering it.
fn merge_claude_hooks(
    root: &mut serde_json::Map<String, Value>,
    hooks: &[ClaudeHook],
) -> Result<(), Materialized> {
    if hooks.is_empty() {
        return Ok(());
    }
    let hooks_root = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks_root) = hooks_root.as_object_mut() else {
        return Err(Materialized::Skipped(
            "Claude `hooks` is not an object".to_string(),
        ));
    };
    for hook in hooks {
        let event = hooks_root
            .entry(hook.event.clone())
            .or_insert_with(|| json!([]));
        let Some(groups) = event.as_array_mut() else {
            return Err(Materialized::Skipped(format!(
                "Claude `hooks.{}` is not an array",
                hook.event
            )));
        };
        let already_present = groups.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|v| v.as_array())
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry.get("command").and_then(|v| v.as_str()) == Some(&hook.command)
                    })
                })
        });
        if already_present {
            continue;
        }
        let entry = json!({ "type": "command", "command": hook.command });
        let matcher_group = groups.iter_mut().find(|group| {
            group.get("matcher").and_then(|v| v.as_str()) == Some(hook.matcher.as_str())
                && group.get("hooks").is_some_and(Value::is_array)
        });
        match matcher_group {
            Some(group) => {
                // Checked is_array above; push into the existing matcher group.
                group
                    .get_mut("hooks")
                    .and_then(|v| v.as_array_mut())
                    .expect("matcher group hooks is an array")
                    .push(entry);
            }
            None => groups.push(json!({ "matcher": hook.matcher, "hooks": [entry] })),
        }
    }
    Ok(())
}

/// Append one managed line to a host directory's `.gitignore`, creating the file
/// if it does not exist and leaving every other line — user rules included —
/// exactly as found.
///
/// Adding the line at most once is what makes repeated `enable` runs a no-op; the
/// membership test is on the trimmed line so a rule the user indented still
/// counts as present rather than being duplicated on every run.
fn ensure_managed_gitignore(dir: &Path, entry: &str) -> Result<Materialized, String> {
    if let Some(skipped) = unusable_directory(dir) {
        return Ok(skipped);
    }
    let path = dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(Materialized::Wrote);
    }
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(entry);
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

fn ensure_claude_gitignore(repo_root: &Path) -> Result<Materialized, String> {
    ensure_managed_gitignore(&repo_root.join(".claude"), "scheduled_tasks.lock")
}

/// The generated Codex config is machine-local and must not be committed.
///
/// Its `url` carries `?root=<absolute path>` (ADPT-14), which is a fact about one
/// developer's filesystem. Committed, it hands every other clone a root that does
/// not exist there — and because Codex merges a repo entry over the user entry,
/// the broken registration wins and Codex is unusable in that repo. This is the
/// `.claude/.gitignore` treatment applied to the file that actually carries a
/// machine-local path.
fn ensure_codex_gitignore(repo_root: &Path) -> Result<Materialized, String> {
    ensure_managed_gitignore(&repo_root.join(".codex"), "config.toml")
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_toml(path: &Path, table: &toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = toml::to_string_pretty(table).map_err(|e| e.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

struct UserConfigPaths {
    home: PathBuf,
    codex_config: PathBuf,
    claude_json: PathBuf,
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
        home,
    }
}

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

    #[test]
    fn repo_install_creates_expected_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let install = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]).approve_tool("todo_create"))
            .managed_markdown_body("Use todo_* MCP tools.")
            .claude_allow("Bash(todo *)");
        let report = install.install_repo(dir.path()).unwrap();
        assert_eq!(report.files.len(), 7);
        assert!(dir.path().join(".mcp.json").exists());
        assert!(dir.path().join(".codex/config.toml").exists());
        assert!(dir.path().join(".codex/.gitignore").exists());
        assert!(dir.path().join("CLAUDE.md").exists());
    }

    /// ADPT-09 — the generated Codex config pins an absolute repository root, so
    /// committing it ships one developer's filesystem layout to every clone. It
    /// has to be ignored where it is generated.
    #[test]
    fn the_generated_codex_config_is_ignored_and_a_user_rule_survives() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/.gitignore"),
            "# mine\nnotes.local.md\n",
        )
        .unwrap();

        let install = HostInstall::new("todo").server(HostServer::http(
            "todo",
            "http://127.0.0.1:7977/mcp?root=/x",
        ));
        install.install_repo(dir.path()).unwrap();

        let ignore = fs::read_to_string(dir.path().join(".codex/.gitignore")).unwrap();
        assert!(
            ignore.lines().any(|line| line.trim() == "config.toml"),
            "the generated config must be ignored: {ignore}"
        );
        assert!(
            ignore.contains("notes.local.md"),
            "user rule kept: {ignore}"
        );
        assert!(ignore.contains("# mine"), "user comment kept: {ignore}");

        // Idempotent: the managed line is added once, not once per run.
        install.install_repo(dir.path()).unwrap();
        let again = fs::read_to_string(dir.path().join(".codex/.gitignore")).unwrap();
        assert_eq!(again, ignore, "a second run must change nothing");
        assert_eq!(
            again.lines().filter(|l| l.trim() == "config.toml").count(),
            1,
            "exactly one managed rule: {again}"
        );
    }

    /// A `.gitignore` whose last line has no trailing newline must not have our
    /// rule welded onto it.
    #[test]
    fn a_gitignore_without_a_trailing_newline_gains_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(dir.path().join(".codex/.gitignore"), "notes.local.md").unwrap();

        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();

        let ignore = fs::read_to_string(dir.path().join(".codex/.gitignore")).unwrap();
        assert!(
            ignore.lines().any(|line| line.trim() == "notes.local.md"),
            "{ignore}"
        );
        assert!(
            ignore.lines().any(|line| line.trim() == "config.toml"),
            "{ignore}"
        );
    }

    /// A stray file where `.codex` should be a directory skips this one file
    /// rather than failing the whole install.
    #[test]
    fn a_stray_file_at_dot_codex_skips_the_gitignore_too() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".codex"), "not a directory").unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();

        let action = report
            .files
            .iter()
            .find(|(name, _)| name == ".codex/.gitignore")
            .map(|(_, action)| action.tag());
        assert_eq!(action, Some("skipped"), "{:?}", report.files);
    }

    /// ADPT-04 — the exact live case: a repo `.codex/config.toml` carrying our
    /// URL plus an `http_headers` table left over from when the credential lived
    /// at repo scope. Ownership matched on the URL, so the entry read as current
    /// and the stale bearer token survived every re-run of enable.
    #[test]
    fn a_repo_codex_entry_with_stale_headers_converges_and_spares_foreign_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            concat!(
                "# a comment the user wrote\n",
                "[mcp_servers.other]\n",
                "command = \"other\"\n",
                "args = [\"serve\"]\n",
                "\n",
                "[mcp_servers.todo]\n",
                "url = \"http://127.0.0.1:7977/mcp\"\n",
                "\n",
                "[mcp_servers.todo.http_headers]\n",
                "\"Authorization\" = \"Bearer leaked-token\"\n",
            ),
        )
        .unwrap();

        // Repo scope carries no token, so `http_headers` is a key we no longer emit.
        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let doc: toml::Table = raw.parse().expect("codex config stays valid TOML");
        let entry = doc["mcp_servers"]["todo"].as_table().unwrap();
        assert_eq!(entry["url"].as_str(), Some("http://127.0.0.1:7977/mcp"));
        assert!(
            !entry.contains_key("http_headers"),
            "the stale header table must be gone: {raw}"
        );
        assert!(
            !raw.contains("leaked-token"),
            "no trace of the stale credential: {raw}"
        );
        // Foreign server and user comment are untouched.
        assert_eq!(
            doc["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
        assert!(raw.contains("# a comment the user wrote"), "{raw}");

        // Converged: a second run is a byte-identical no-op.
        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap(),
            raw,
            "re-running over a converged file must change nothing"
        );
    }

    /// ADPT-08 — the exact live case: a `.codex/config.toml` committed on a Linux
    /// checkout and pulled onto a Windows machine. The URL differs only in the
    /// `?root=` query, which pins an absolute repository path. Ownership was
    /// whole-URL equality, so the entry read as a stranger's, `enable` skipped it
    /// and reported the file `unchanged` while readiness reported it `shadowed` —
    /// leaving Codex pointed at a path that does not exist on this machine, with
    /// no command able to repair it.
    #[test]
    fn a_repo_codex_entry_rooted_on_another_machine_is_owned_and_reroots() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            concat!(
                "# a comment the user wrote\n",
                "[mcp_servers.other]\n",
                "url = \"http://127.0.0.1:9999/mcp?root=/home/juno/todo\"\n",
                "\n",
                "[mcp_servers.todo]\n",
                "url = \"http://127.0.0.1:7977/mcp?root=/home/juno/todo\"\n",
            ),
        )
        .unwrap();

        let local = "http://127.0.0.1:7977/mcp?root=C:/Users/dev/todo";
        HostInstall::new("todo")
            .server(HostServer::http("todo", local))
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let doc: toml::Table = raw.parse().expect("codex config stays valid TOML");
        assert_eq!(
            doc["mcp_servers"]["todo"]["url"].as_str(),
            Some(local),
            "the foreign root must be rewritten to this machine's: {raw}"
        );
        assert!(
            doc["mcp_servers"]["todo"]["url"]
                .as_str()
                .is_some_and(|url| !url.contains("/home/juno")),
            "no trace of the other machine's root on our entry: {raw}"
        );

        // A server at a different port is a different server, not a stale copy of
        // ours: its machine-local root must survive untouched.
        assert_eq!(
            doc["mcp_servers"]["other"]["url"].as_str(),
            Some("http://127.0.0.1:9999/mcp?root=/home/juno/todo"),
            "a foreign server must not be re-rooted: {raw}"
        );
        assert!(raw.contains("# a comment the user wrote"), "{raw}");

        // Converged: a second run is a byte-identical no-op.
        HostInstall::new("todo")
            .server(HostServer::http("todo", local))
            .install_repo(dir.path())
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap(),
            raw,
            "re-running over a converged file must change nothing"
        );
    }

    /// Ownership ignores the machine-local query, but currency must not: an entry
    /// whose root is stale has to read as NOT current, or it would be recognized
    /// as ours and then left exactly as it was — the same dead end by another
    /// route. This pins the two predicates apart.
    #[test]
    fn a_stale_root_is_owned_but_not_current() {
        let server = HostServer::http("todo", "http://127.0.0.1:7977/mcp?root=C:/Users/dev/todo");
        let stale: toml::Table = "url = \"http://127.0.0.1:7977/mcp?root=/home/juno/todo\"\n"
            .parse()
            .unwrap();
        let foreign: toml::Table = "url = \"http://127.0.0.1:9999/mcp?root=/home/juno/todo\"\n"
            .parse()
            .unwrap();

        assert!(
            codex_server_is_owned(&toml::Value::Table(stale.clone()), &server),
            "an entry differing only in ?root= is ours"
        );
        assert!(
            !codex_entry_is_current(&stale, &server),
            "...but it is not current, so it gets rewritten"
        );
        assert!(
            !codex_server_is_owned(&toml::Value::Table(foreign), &server),
            "a different port is a different server"
        );
    }

    /// The Claude Code half of the same defect. Ishoo's rooted registration lives
    /// in the user's private per-project scope (ADPT-14), which is exactly where a
    /// root from a previous machine or a moved checkout goes stale.
    #[test]
    fn a_claude_entry_rooted_on_another_machine_is_owned_and_reroots() {
        let local = "http://127.0.0.1:7977/mcp?root=C:/Users/dev/todo";
        let server = HostServer::http("todo", local);
        let stale = json!({
            "type": "http",
            "url": "http://127.0.0.1:7977/mcp?root=/home/juno/todo"
        });
        let foreign = json!({
            "type": "http",
            "url": "http://127.0.0.1:9999/mcp?root=/home/juno/todo"
        });

        assert!(claude_server_is_owned(&stale, &server));
        assert!(!claude_entry_is_current(&stale, &server));
        assert!(!claude_server_is_owned(&foreign, &server));
    }

    /// A URL with no query at all still compares correctly — the user-scope
    /// registration is deliberately unrooted, and it must not suddenly match every
    /// rooted entry or fail to match itself.
    #[test]
    fn an_unrooted_url_matches_itself_and_not_a_different_server() {
        assert_eq!(
            http_identity("http://127.0.0.1:7977/mcp"),
            "http://127.0.0.1:7977/mcp"
        );
        assert_eq!(
            http_identity("http://127.0.0.1:7977/mcp?root=C:/x"),
            "http://127.0.0.1:7977/mcp"
        );
        let server = HostServer::http("todo", "http://127.0.0.1:7977/mcp");
        let unrooted: toml::Table = "url = \"http://127.0.0.1:7977/mcp\"\n".parse().unwrap();
        assert!(codex_entry_is_current(&unrooted, &server));
    }

    /// The `.mcp.json` half of the same defect.
    #[test]
    fn a_repo_mcp_json_entry_with_extra_keys_converges() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "other": { "command": "other", "args": ["serve"] },
                    "todo": {
                        "type": "http",
                        "url": "http://127.0.0.1:7977/mcp",
                        "headers": { "Authorization": "Bearer leaked-token" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let doc: Value = serde_json::from_str(&raw).unwrap();
        let entry = &doc["mcpServers"]["todo"];
        assert_eq!(entry["url"], "http://127.0.0.1:7977/mcp");
        assert_eq!(entry["type"], "http");
        assert!(entry.get("headers").is_none(), "stale headers gone: {raw}");
        assert!(!raw.contains("leaked-token"), "{raw}");
        // A foreign entry under a different name is never rewritten.
        assert_eq!(doc["mcpServers"]["other"]["command"], "other");

        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".mcp.json")).unwrap(),
            raw,
            "second run is a no-op"
        );
    }

    /// The rotation rationale must survive: when we DO emit headers, an entry
    /// whose header value differs is still ours and still current — comparing
    /// the value would make every credential rotation read as foreign drift.
    #[test]
    fn a_rotated_header_value_does_not_make_an_entry_stale() {
        let server = HostServer::http("todo", "http://127.0.0.1:7977/mcp")
            .header("Authorization", "Bearer new");
        let entry: toml::Table = concat!(
            "url = \"http://127.0.0.1:7977/mcp\"\n",
            "[http_headers]\n",
            "\"Authorization\" = \"Bearer old\"\n",
        )
        .parse()
        .unwrap();
        assert!(
            codex_entry_is_current(&entry, &server),
            "a rotated token is not drift"
        );

        let claude = json!({
            "type": "http",
            "url": "http://127.0.0.1:7977/mcp",
            "headers": { "Authorization": "Bearer old" }
        });
        assert!(claude_entry_is_current(&claude, &server));
    }

    /// ADPT-03 — a repo carrying a stray regular file named `.codex` used to make
    /// `create_dir_all` fail with `File exists`, and that error aborted the entire
    /// run, so the caller got nothing at all. One unusable path must cost only its
    /// own file.
    #[test]
    fn a_stray_file_at_dot_codex_skips_only_that_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".codex"), "").unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .managed_markdown_body("Use todo_* MCP tools.")
            .claude_allow("Bash(todo *)")
            .install_repo(dir.path())
            .expect("a stray .codex must not abort the run");

        let codex = report
            .files
            .iter()
            .find(|(name, _)| name == ".codex/config.toml")
            .map(|(_, action)| action)
            .expect("the codex entry is still reported");
        let AdapterAction::Skipped(reason) = codex else {
            panic!("expected the codex adapter to be skipped, got {codex:?}");
        };
        assert!(
            reason.contains(".codex"),
            "the reason names the offending path: {reason}"
        );

        // Every other adapter still materialized.
        for name in [
            ".mcp.json",
            ".claude/settings.local.json",
            ".claude/.gitignore",
            "CLAUDE.md",
            "AGENTS.md",
        ] {
            assert!(
                dir.path().join(name).exists(),
                "{name} must still be written"
            );
        }
        // The stray file is left exactly as we found it — we do not own it.
        assert!(dir.path().join(".codex").is_file());
    }

    /// A dangling symlink is the same failure wearing a different hat: `exists()`
    /// follows the link and reports absent, yet `create_dir_all` still refuses.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_at_dot_codex_skips_only_that_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join(".codex")).unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .install_repo(dir.path())
            .expect("a dangling .codex symlink must not abort the run");

        let codex = report
            .files
            .iter()
            .find(|(name, _)| name == ".codex/config.toml")
            .map(|(_, action)| action)
            .expect("the codex entry is still reported");
        assert!(
            matches!(codex, AdapterAction::Skipped(_)),
            "expected skipped, got {codex:?}"
        );
        assert!(dir.path().join(".mcp.json").exists());
    }

    /// The guard must not fire on the ordinary case it resembles: a symlink that
    /// points at a real directory is perfectly writable.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_dot_codex_directory_still_installs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let real = dir.path().join("elsewhere");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(".codex")).unwrap();

        HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .install_repo(dir.path())
            .unwrap();

        assert!(real.join("config.toml").exists());
    }

    #[test]
    fn mcp_json_server_entry_is_minimal_and_env_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .server(HostServer::stdio("keyed", "keyed", ["mcp"]).env("TOKEN", "abc"))
            .install_repo(dir.path())
            .unwrap();

        let doc: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        let plain = &doc["mcpServers"]["todo"];
        // Byte-parity with a hand-authored entry: no implicit `type`, no empty `env`.
        assert!(plain.get("type").is_none(), "no default type: {plain}");
        assert!(plain.get("env").is_none(), "no empty env: {plain}");
        assert_eq!(plain["command"], "todo");
        assert_eq!(plain["args"][0], "mcp");
        // A server that carries variables still emits its env.
        let keyed = &doc["mcpServers"]["keyed"];
        assert_eq!(keyed["env"]["TOKEN"], "abc");
        assert!(keyed.get("type").is_none());

        // Re-running enable is a true no-op on the minimal entry.
        let again = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .server(HostServer::stdio("keyed", "keyed", ["mcp"]).env("TOKEN", "abc"))
            .install_repo(dir.path())
            .unwrap();
        let mcp = again.files.iter().find(|(f, _)| f == ".mcp.json").unwrap();
        assert_eq!(mcp.1, AdapterAction::Unchanged, "re-enable is a no-op");
    }

    #[test]
    fn configurable_markers_update_an_existing_block_in_place() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let path = dir.path().join("CLAUDE.md");
        fs::write(
            &path,
            "user introduction\n<!-- ishoo:begin -->\nstale\n<!-- ishoo:end -->\nuser footer\n",
        )
        .unwrap();

        HostInstall::new("ishoo")
            .managed_markdown_body("fresh")
            .managed_markdown_markers("<!-- ishoo:begin -->", "<!-- ishoo:end -->")
            .install_repo(dir.path())
            .unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "user introduction\n<!-- ishoo:begin -->\nfresh\n<!-- ishoo:end -->\nuser footer\n"
        );
    }

    #[test]
    fn claude_hook_is_materialized_idempotently_into_repo_settings() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let install = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .claude_hook("PreToolUse", "Bash", "todo agent-guard");

        install.install_repo(dir.path()).unwrap();
        let path = dir.path().join(".claude/settings.local.json");
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(
            doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "todo agent-guard"
        );
        assert_eq!(doc["hooks"]["PreToolUse"][0]["hooks"][0]["type"], "command");

        // Second run is byte-identical (no duplicate hook entries).
        let before = fs::read(&path).unwrap();
        let report = install.install_repo(dir.path()).unwrap();
        let settings = report
            .files
            .iter()
            .find(|(f, _)| f == ".claude/settings.local.json")
            .unwrap();
        assert_eq!(settings.1, AdapterAction::Unchanged);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn claude_hook_joins_an_existing_matcher_group_and_preserves_foreign_hooks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        fs::write(
            dir.path().join(".claude/settings.local.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"my-linter"}]},{"matcher":"Write","hooks":[{"type":"command","command":"fmt"}]}],"Stop":[{"hooks":[{"type":"command","command":"notify"}]}]},"keep":true}"#,
        )
        .unwrap();

        HostInstall::new("todo")
            .claude_hook("PreToolUse", "Bash", "todo agent-guard")
            .install_repo(dir.path())
            .unwrap();

        let doc: Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        // Joined the existing Bash group, after the user's hook.
        let bash_hooks = doc["hooks"]["PreToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(bash_hooks[0]["command"], "my-linter");
        assert_eq!(bash_hooks[1]["command"], "todo agent-guard");
        // Foreign matcher group, foreign event, and unknown keys preserved.
        assert_eq!(doc["hooks"]["PreToolUse"][1]["matcher"], "Write");
        assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], "notify");
        assert_eq!(doc["keep"], true);
    }

    #[test]
    fn claude_hook_skips_an_alien_hooks_shape_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let original = r#"{"hooks":"custom-string-shape"}"#;
        fs::write(dir.path().join(".claude/settings.local.json"), original).unwrap();

        let report = HostInstall::new("todo")
            .claude_hook("PreToolUse", "Bash", "todo agent-guard")
            .install_repo(dir.path())
            .unwrap();
        let settings = report
            .files
            .iter()
            .find(|(f, _)| f == ".claude/settings.local.json")
            .unwrap();
        assert!(matches!(settings.1, AdapterAction::Skipped(_)));
        assert_eq!(
            fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap(),
            original,
            "alien shape left untouched"
        );
    }

    #[test]
    fn unparseable_repo_codex_config_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(dir.path().join(".codex/config.toml"), "not = [toml").unwrap();
        let install = HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp"]));
        let report = install.install_repo(dir.path()).unwrap();
        let codex = report
            .files
            .iter()
            .find(|(path, _)| path == ".codex/config.toml")
            .unwrap();
        assert!(matches!(codex.1, AdapterAction::Skipped(_)));
    }

    #[test]
    fn readiness_accepts_distinct_user_and_repository_server_shapes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let codex = dir.path().join("codex.toml");
        let claude = dir.path().join("claude.json");
        let repo_install =
            || HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp"]));
        let user_install =
            || HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp", "--hub"]));

        user_install().install_user_at(&codex, &claude).unwrap();
        let global = repo_install().readiness_at_with_user_install(
            &user_install(),
            dir.path(),
            &codex,
            &claude,
        );
        assert!(global.iter().all(|report| report.configured));
        assert!(global
            .iter()
            .all(|report| report.effective_source == "user"));

        repo_install().install_repo(dir.path()).unwrap();
        let both = repo_install().readiness_at_with_user_install(
            &user_install(),
            dir.path(),
            &codex,
            &claude,
        );
        assert!(both.iter().all(|report| report.configured));
        assert!(both.iter().all(|report| report.effective_source == "both"));
    }

    /// ADPT-11 (DEC-03): the verdict may only claim what config inspection can
    /// prove. Nothing in this crate contacts a host, so no report may say it is
    /// ready — it says it is configured, and connectivity stays `unchecked`.
    ///
    /// This is the regression for the 2026-08-16 false green, where a host that
    /// could not authenticate at all reported `ready: true` / `ready_both`.
    /// Asserting over the serialized form rather than the struct keeps the
    /// promise where consumers actually read it.
    #[test]
    fn readiness_never_claims_ready_from_config_inspection_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let codex = dir.path().join("codex.toml");
        let claude = dir.path().join("claude.json");
        let repo_install =
            || HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp"]));
        let user_install =
            || HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp", "--hub"]));

        user_install().install_user_at(&codex, &claude).unwrap();
        repo_install().install_repo(dir.path()).unwrap();
        let reports = repo_install().readiness_at_with_user_install(
            &user_install(),
            dir.path(),
            &codex,
            &claude,
        );

        assert!(!reports.is_empty(), "fixture must produce host reports");
        for report in &reports {
            // The fully-configured case is exactly the one that used to lie.
            assert!(report.configured, "fixture is fully configured: {report:?}");
            assert_eq!(report.result, "configured_both");
            assert_eq!(
                report.connectivity, "unchecked",
                "nothing here probes a host, so connectivity cannot be anything else"
            );

            let json = serde_json::to_value(report).expect("report serializes");
            let object = json.as_object().expect("report is a JSON object");
            assert!(
                !object.contains_key("ready"),
                "no consumer may read a `ready` field off config inspection: {json}"
            );
            let result = object["result"].as_str().unwrap_or_default();
            assert!(
                !result.starts_with("ready"),
                "verdict must not claim readiness: {result}"
            );
        }
    }

    #[test]
    fn http_server_registers_by_url_with_headers_and_no_command() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .header("Authorization", "Bearer secret-token")
                    .approve_tool("todo_candidates"),
            )
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let doc: toml::Table = raw.parse().expect("codex config stays valid TOML");
        let entry = doc["mcp_servers"]["todo"].as_table().unwrap();

        // The whole point: a `command` key would make Codex choose stdio and then
        // reject the url beside it.
        assert!(!entry.contains_key("command"), "no command key: {raw}");
        assert_eq!(entry["url"].as_str(), Some("http://127.0.0.1:7977/mcp"));
        assert_eq!(
            entry["http_headers"]["Authorization"].as_str(),
            Some("Bearer secret-token")
        );
        // Approval tables still ride along on the HTTP shape.
        assert_eq!(
            entry["tools"]["todo_candidates"]["approval_mode"].as_str(),
            Some("approve")
        );
    }

    #[test]
    fn http_entry_is_recognized_as_owned_so_readiness_is_current_not_drifted() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();

        let install = || {
            HostInstall::new("todo").server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .header("Authorization", "Bearer first-token"),
            )
        };
        install().install_repo(dir.path()).unwrap();

        let facts = install().readiness(dir.path());
        let codex = facts
            .iter()
            .find(|report| report.host == "Codex")
            .expect("codex readiness fact");
        assert!(codex.configured, "http entry must read as configured: {codex:?}");
        assert_eq!(codex.repository_adapter.state, "current");

        // A rotated bearer token must not turn our own entry into foreign drift:
        // the token lives outside the config's lifecycle.
        let rotated = HostInstall::new("todo").server(
            HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                .header("Authorization", "Bearer rotated-token"),
        );
        let facts = rotated.readiness(dir.path());
        let codex = facts.iter().find(|r| r.host == "Codex").unwrap();
        assert_eq!(
            codex.repository_adapter.state, "current",
            "token rotation must not read as drift"
        );
    }

    #[test]
    fn switching_a_repo_from_stdio_to_http_preserves_foreign_servers() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            "# hand-written\n[mcp_servers.other]\ncommand = \"other\"\nargs = []\n",
        )
        .unwrap();

        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let doc: toml::Table = raw.parse().expect("valid TOML");
        assert!(raw.contains("# hand-written"), "user comment preserved");
        assert_eq!(
            doc["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
        assert_eq!(
            doc["mcp_servers"]["todo"]["url"].as_str(),
            Some("http://127.0.0.1:7977/mcp")
        );
    }

    #[test]
    fn claude_http_entry_carries_an_explicit_type_and_url() {
        let server = HostServer::http("todo", "http://127.0.0.1:7977/mcp")
            .header("Authorization", "Bearer t");
        let json = claude_server_json(&server);
        assert_eq!(json["type"], "http");
        assert_eq!(json["url"], "http://127.0.0.1:7977/mcp");
        assert_eq!(json["headers"]["Authorization"], "Bearer t");
        assert!(json.get("command").is_none(), "http entry has no command");
        assert!(claude_server_is_owned(&json, &server));

        // A stdio entry must never be mistaken for our http registration.
        let stdio = HostServer::stdio("todo", "todo", ["mcp"]);
        assert!(!claude_server_is_owned(
            &claude_server_json(&stdio),
            &server
        ));
        assert!(!claude_server_is_owned(&json, &stdio));
    }

    /// The real migration: a repo already registered by stdio must be switched to
    /// http in place, keeping the user's comments and foreign servers.
    #[test]
    fn a_superseded_stdio_entry_is_migrated_to_http_in_place() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(
            dir.path().join(".codex/config.toml"),
            "# keep me\n[mcp_servers.todo]\ncommand = \"todo\"\nargs = [\"mcp\", \"--hub\"]\n\n\
             [mcp_servers.todo.tools.todo_candidates]\napproval_mode = \"approve\"\n\n\
             [mcp_servers.other]\ncommand = \"other\"\nargs = []\n",
        )
        .unwrap();

        HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .header("Authorization", "Bearer tok")
                    .approve_tool("todo_candidates")
                    .supersedes(HostTransport::Stdio {
                        command: "todo".to_string(),
                        args: vec!["mcp".to_string(), "--hub".to_string()],
                    }),
            )
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        let doc: toml::Table = raw.parse().expect("valid TOML after migration");
        let entry = doc["mcp_servers"]["todo"].as_table().unwrap();

        assert!(
            !entry.contains_key("command"),
            "stdio command must be gone after migration: {raw}"
        );
        assert_eq!(entry["url"].as_str(), Some("http://127.0.0.1:7977/mcp"));
        assert_eq!(
            entry["http_headers"]["Authorization"].as_str(),
            Some("Bearer tok")
        );
        assert_eq!(
            entry["tools"]["todo_candidates"]["approval_mode"].as_str(),
            Some("approve")
        );
        // Everything that is not ours survives untouched.
        assert!(raw.contains("# keep me"), "user comment preserved: {raw}");
        assert_eq!(
            doc["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );

        // And it is idempotent: running again changes nothing.
        let before = raw.clone();
        HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .header("Authorization", "Bearer tok")
                    .approve_tool("todo_candidates")
                    .supersedes(HostTransport::Stdio {
                        command: "todo".to_string(),
                        args: vec!["mcp".to_string(), "--hub".to_string()],
                    }),
            )
            .install_repo(dir.path())
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap(),
            before,
            "second enable must be a no-op"
        );
    }

    #[test]
    fn a_foreign_entry_under_our_name_is_never_migrated() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let original =
            "[mcp_servers.todo]\ncommand = \"someone-elses-binary\"\nargs = [\"serve\"]\n";
        fs::write(dir.path().join(".codex/config.toml"), original).unwrap();

        HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp").supersedes(
                    HostTransport::Stdio {
                        command: "todo".to_string(),
                        args: vec!["mcp".to_string()],
                    },
                ),
            )
            .install_repo(dir.path())
            .unwrap();

        let raw = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert_eq!(raw, original, "a stranger's entry must be left alone");
    }

    #[test]
    fn mcp_json_migrates_a_superseded_stdio_entry_but_spares_foreign_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"todo":{"command":"todo","args":["mcp","--hub"]},"other":{"command":"other","args":[]}}}"#,
        )
        .unwrap();

        HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .header("Authorization", "Bearer tok")
                    .supersedes(HostTransport::Stdio {
                        command: "todo".to_string(),
                        args: vec!["mcp".to_string(), "--hub".to_string()],
                    }),
            )
            .install_repo(dir.path())
            .unwrap();

        let doc: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(doc["mcpServers"]["todo"]["type"], "http");
        assert_eq!(
            doc["mcpServers"]["todo"]["url"],
            "http://127.0.0.1:7977/mcp"
        );
        assert!(doc["mcpServers"]["todo"].get("command").is_none());
        assert_eq!(doc["mcpServers"]["other"]["command"], "other");
    }

    #[test]
    fn a_mixed_command_and_url_entry_is_not_owned_stdio() {
        // Codex rejects this shape outright, so it is broken config, not ours.
        let entry: toml::Value = "command = \"todo\"\nargs = [\"mcp\"]\nurl = \"http://x/mcp\"\n"
            .parse::<toml::Table>()
            .map(toml::Value::Table)
            .unwrap();
        let stdio = HostServer::stdio("todo", "todo", ["mcp"]);
        assert!(!codex_server_is_owned(&entry, &stdio));
    }

    #[test]
    fn header_names_are_quoted_so_odd_names_cannot_break_the_file() {
        let server = HostServer::http("todo", "http://127.0.0.1:7977/mcp")
            .header("X.Weird+Name", "v")
            .header("Authorization", "Bearer t");
        let rendered = render_codex_server(&server);
        let full = rendered.to_string();
        let doc: toml::Table = full.parse().expect("odd header names keep the file valid");
        let headers = doc["mcp_servers"]["todo"]["http_headers"]
            .as_table()
            .unwrap();
        // The dotted name stays one literal key rather than becoming a nested table.
        assert_eq!(headers["X.Weird+Name"].as_str(), Some("v"));
        assert_eq!(headers["Authorization"].as_str(), Some("Bearer t"));
    }

    /// ADPT-05 (1) — a server withheld from Claude Code reaches `.codex/config.toml`
    /// and never `.mcp.json`. This is the whole point of per-host declaration: under
    /// `ScopeComposition::Replace` a tracked, tokenless repo entry would shadow a
    /// working user-scope entry and connect with no credential.
    #[test]
    fn a_codex_only_server_is_written_to_codex_and_absent_from_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let install = HostInstall::new("todo")
            .server(
                HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                    .repo_scope_hosts([Host::Codex]),
            )
            .server(HostServer::stdio("mapper", "mapper", ["mcp"]));
        install.install_repo(dir.path()).unwrap();

        let codex = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(codex.contains("[mcp_servers.todo]"), "{codex}");
        assert!(codex.contains("[mcp_servers.mapper]"), "{codex}");

        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        let servers = mcp["mcpServers"].as_object().unwrap();
        assert!(
            !servers.contains_key("todo"),
            "credentialed server must not reach the tracked repo file: {mcp}"
        );
        // An unrestricted server is unaffected — the default is still every host.
        assert!(servers.contains_key("mapper"), "{mcp}");
    }

    /// ADPT-05 (2) — the per-project registration is the only scope that can carry a
    /// credential for Claude Code, so it must land under the canonical root, keep its
    /// headers, and leave the top-level map and every other project alone.
    #[test]
    fn a_per_project_claude_registration_lands_scoped_and_spares_other_entries() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        fs::write(
            &claude_json,
            serde_json::to_string_pretty(&json!({
                "mcpServers": { "todo": { "type": "http", "url": "http://127.0.0.1:7977/mcp" } },
                "projects": {
                    "/somewhere/else": { "mcpServers": { "todo": { "command": "keep-me" } } }
                },
                "someUnrelatedTopLevelKey": 42
            }))
            .unwrap(),
        )
        .unwrap();

        let install = HostInstall::new("todo").server(
            HostServer::http("todo", "http://127.0.0.1:7977/mcp?root=/x")
                .header("Authorization", "Bearer secret"),
        );
        let report = install
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].1, AdapterAction::Updated);

        let doc: Value = serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        let key = project_scope_key(repo.path());
        let entry = &doc["projects"][&key]["mcpServers"]["todo"];
        assert_eq!(
            entry["url"].as_str(),
            Some("http://127.0.0.1:7977/mcp?root=/x")
        );
        assert_eq!(
            entry["headers"]["Authorization"].as_str(),
            Some("Bearer secret")
        );
        // Untouched: the user scope, a foreign project, and unrelated file state.
        assert_eq!(
            doc["mcpServers"]["todo"]["url"].as_str(),
            Some("http://127.0.0.1:7977/mcp")
        );
        assert!(doc["mcpServers"]["todo"].get("headers").is_none());
        assert_eq!(
            doc["projects"]["/somewhere/else"]["mcpServers"]["todo"]["command"].as_str(),
            Some("keep-me")
        );
        assert_eq!(doc["someUnrelatedTopLevelKey"].as_i64(), Some(42));
    }

    /// ADPT-05 (2, cont.) — per-project state Claude keeps beside `mcpServers`
    /// survives, because we merge into the existing project object.
    #[test]
    fn a_per_project_registration_preserves_sibling_project_state() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        let key = project_scope_key(repo.path());
        fs::write(
            &claude_json,
            serde_json::to_string(&json!({
                "projects": { &key: { "history": ["a", "b"], "allowedTools": [] } }
            }))
            .unwrap(),
        )
        .unwrap();

        HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();

        let doc: Value = serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert_eq!(doc["projects"][&key]["history"][1].as_str(), Some("b"));
        assert!(doc["projects"][&key]["mcpServers"]["todo"].is_object());
    }

    /// ADPT-05 (3) — re-running must be a true no-op, byte for byte, or every
    /// enable would churn the user's private config.
    #[test]
    fn re_running_a_per_project_registration_is_byte_identical() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        let install = HostInstall::new("todo").server(
            HostServer::http("todo", "http://127.0.0.1:7977/mcp")
                .header("Authorization", "Bearer t"),
        );

        let first = install
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();
        assert_eq!(first.files[0].1, AdapterAction::Created);
        let after_first = fs::read(&claude_json).unwrap();

        let second = install
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();
        assert_eq!(second.files[0].1, AdapterAction::Unchanged);
        assert_eq!(after_first, fs::read(&claude_json).unwrap());
    }

    /// ADPT-05 (4) — a stranger's entry under our name at project scope is reported
    /// and left exactly as it was, the same ownership rule the other scopes follow.
    #[test]
    fn a_foreign_per_project_entry_is_reported_and_left_untouched() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        let key = project_scope_key(repo.path());
        let original = serde_json::to_string(&json!({
            "projects": { &key: { "mcpServers": { "todo": { "command": "someone-elses" } } } }
        }))
        .unwrap();
        fs::write(&claude_json, &original).unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();

        match &report.files[0].1 {
            AdapterAction::Skipped(reason) => assert!(reason.contains("non-owned"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&claude_json).unwrap(), original);
    }

    /// ADPT-05 (5) — removal reaches the per-project scope it wrote, and stops there.
    #[test]
    fn removal_cleans_the_per_project_scope_only() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        let install =
            HostInstall::new("todo").server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"));
        install
            .install_user_at(&home.path().join("codex.toml"), &claude_json)
            .unwrap();
        install
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();
        let key = project_scope_key(repo.path());

        install
            .remove_user_project_at(&claude_json, repo.path())
            .unwrap();

        let doc: Value = serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert!(
            doc["projects"][&key]["mcpServers"]
                .as_object()
                .unwrap()
                .is_empty(),
            "project scope should be cleaned: {doc}"
        );
        // The user scope is a different registration and is not swept by this call.
        assert!(doc["mcpServers"]["todo"].is_object(), "{doc}");
    }

    /// ADPT-05 (6) — an unparseable config is reported and never overwritten.
    #[test]
    fn an_unparseable_claude_json_is_skipped_not_clobbered_at_project_scope() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        fs::write(&claude_json, "{ not json at all").unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();

        match &report.files[0].1 {
            AdapterAction::Skipped(reason) => assert!(reason.contains("not parseable"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(&claude_json).unwrap(),
            "{ not json at all"
        );
    }

    /// ADPT-05 (review, P1) — the case that actually matters: an install that
    /// already wrote the server to `.mcp.json` before it was withheld. Skipping it
    /// would leave the shadowing entry in place, so every affected repo would stay
    /// broken after the upgrade. Narrowing the host set must remove it.
    #[test]
    fn narrowing_the_host_set_removes_the_entry_a_previous_install_wrote() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let server = || HostServer::http("todo", "http://127.0.0.1:7977/mcp");

        // The old install: every host, including the tracked repo files.
        HostInstall::new("todo")
            .server(server())
            .install_repo(dir.path())
            .unwrap();
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["todo"].is_object(), "precondition");

        // The upgrade withholds it from Claude Code.
        HostInstall::new("todo")
            .server(server().repo_scope_hosts([Host::Codex]))
            .install_repo(dir.path())
            .unwrap();

        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(
            !mcp["mcpServers"].as_object().unwrap().contains_key("todo"),
            "stale shadowing entry survived the upgrade: {mcp}"
        );
        let codex = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(codex.contains("[mcp_servers.todo]"), "{codex}");
    }

    /// ADPT-05 (review, P1) — the symmetric Codex sweep, and it must spare a
    /// foreign server and the user's own comments while doing it.
    #[test]
    fn narrowing_the_host_set_removes_an_owned_codex_block_and_spares_foreign_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let server = || HostServer::http("todo", "http://127.0.0.1:7977/mcp");
        HostInstall::new("todo")
            .server(server())
            .server(HostServer::stdio("other", "other", ["serve"]))
            .install_repo(dir.path())
            .unwrap();

        HostInstall::new("todo")
            .server(server().repo_scope_hosts([Host::ClaudeCode]))
            .server(HostServer::stdio("other", "other", ["serve"]))
            .install_repo(dir.path())
            .unwrap();

        let codex = fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(
            !codex.contains("[mcp_servers.todo]"),
            "owned block should be swept: {codex}"
        );
        assert!(codex.contains("[mcp_servers.other]"), "{codex}");
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["todo"].is_object(), "{mcp}");
    }

    /// ADPT-05 (review, P1) — a file that exists but cannot be read is reported,
    /// never treated as absent and overwritten with a fresh document.
    #[test]
    fn an_unreadable_claude_json_is_reported_not_replaced() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let claude_json = home.path().join(".claude.json");
        // Invalid UTF-8: present and openable, but not readable as a string.
        fs::write(&claude_json, [0x7b, 0xff, 0xfe, 0x7d]).unwrap();

        let report = HostInstall::new("todo")
            .server(HostServer::http("todo", "http://127.0.0.1:7977/mcp"))
            .install_user_project_at(&claude_json, repo.path())
            .unwrap();

        match &report.files[0].1 {
            AdapterAction::Skipped(reason) => {
                assert!(reason.contains("could not be read"), "{reason}")
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        assert_eq!(fs::read(&claude_json).unwrap(), [0x7b, 0xff, 0xfe, 0x7d]);
    }

    /// ADPT-05 (review, P2) — readiness must ask what the install answers. A server
    /// withheld from Claude Code is not drift, and must not drag the host's
    /// readiness down when the user registration is good.
    #[test]
    fn readiness_does_not_call_a_withheld_repo_server_drift() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let codex_config = home.path().join("codex.toml");
        let claude_json = home.path().join(".claude.json");

        let install = HostInstall::new("todo").server(
            HostServer::http("todo", "http://127.0.0.1:7977/mcp").repo_scope_hosts([Host::Codex]),
        );
        install.install_repo(dir.path()).unwrap();
        install
            .install_user_at(&codex_config, &claude_json)
            .unwrap();

        let reports = install.readiness_at(dir.path(), &codex_config, &claude_json);
        let claude = reports.iter().find(|r| r.host == "Claude Code").unwrap();
        assert_ne!(
            claude.repository_adapter.state, "drifted",
            "a deliberately withheld server is not drift: {claude:?}"
        );
        assert!(claude.configured, "user registration is good: {claude:?}");
    }

    /// ADPT-05 — the composition rule is the reason the rest of this exists, so it
    /// is asserted rather than left to prose.
    #[test]
    fn hosts_declare_how_they_compose_scopes() {
        assert_eq!(
            Host::ClaudeCode.scope_composition(),
            ScopeComposition::Replace
        );
        assert_eq!(Host::Codex.scope_composition(), ScopeComposition::Merge);
    }
}
