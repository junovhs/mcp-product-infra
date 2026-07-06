//! The MCP tool registry (MCP-03 / DEC-49).
//!
//! One [`ToolSpec`] per agent-facing capability: a tool name, the inventory
//! capability it covers, the core function it binds to, a JSON-Schema for its
//! arguments, and a handler. `tools/list` is rendered from this registry and
//! `tools/call` dispatches through it, so adding a tool is a single entry.
//!
//! [`coverage_gaps`] diffs the registry against [`IN_SCOPE_CAPABILITIES`] — the
//! agent-facing core surface the MCP plan delivers, sourced from
//! `docs/enforcement-inventory.md` (the read + authoring + transition rows;
//! human-only surfaces like the picker and help prose are excluded per DEC-49).
//! A capability with no tool is a gap the coverage test names.
//!
//! Handlers are filled in by the slices that own them: `ishoo_status` ships with
//! MCP-02; the read tools land in MCP-05, authoring in MCP-04, transitions in
//! MCP-06. Until a slice lands, the entry carries `handler: None` and the server
//! answers a structured "not yet implemented" naming the owning issue.

use crate::model::{self, EditArgs, NewIssueInput, Status, Workspace};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

const MCP_MUTATION_ID_FIELD: &str = "ishoo:mcp:mutation_id";

/// Server-defined JSON-RPC error code: the tool is registered but its handler
/// has not landed yet (owned by a later slice).
pub const NOT_IMPLEMENTED: i64 = -32001;
/// JSON-RPC invalid-params, reused for tool-argument failures.
pub const INVALID_PARAMS: i64 = -32602;

/// A tool failure, mapped to a JSON-RPC error by the server.
pub struct ToolError {
    pub code: i64,
    pub message: String,
}

impl ToolError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
        }
    }
}

/// A handler returns the typed result value (becomes `structuredContent`), or a
/// [`ToolError`].
pub type ToolResult = Result<Value, ToolError>;
pub type Handler = fn(&Path, &Value) -> ToolResult;

/// A tool whose every call mutates the store (the common write tool).
pub fn mutates_always(_: &Value) -> bool {
    true
}

/// A tool whose calls never mutate the store (a pure read tool).
pub fn mutates_never(_: &Value) -> bool {
    false
}

/// Dispatch an op-dispatched tool (DEC-86): read `op` from the arguments and route
/// to the matching handler. Shared by `ishoo_plan` and the later entity tools so no
/// consolidated tool re-implements op parsing or the unknown-op error. The `op`
/// discriminator is stripped from the arguments the sub-handler receives, so each
/// handler sees exactly the fields the former per-verb tool did (e.g. a no-op guard
/// that counts arg keys must not count `op`).
pub fn dispatch_op(
    entity: &str,
    table: &[(&str, Handler)],
    path: &Path,
    args: &Value,
) -> ToolResult {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_params(format!("{entity} requires an `op` field")))?;
    for (name, handler) in table {
        if *name == op {
            let inner = match args {
                Value::Object(map) => {
                    let mut map = map.clone();
                    map.remove("op");
                    Value::Object(map)
                }
                other => other.clone(),
            };
            return handler(path, &inner);
        }
    }
    let known: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
    Err(ToolError::invalid_params(format!(
        "{entity}: unknown op '{op}'; expected one of {}",
        known.join("/")
    )))
}

/// Whether an op-dispatched call names one of `read_ops` (so it does not mutate).
/// A missing/unknown op is treated as non-read, so it takes the serial mutation
/// path and its handler reports the error there.
pub fn op_is_read(args: &Value, read_ops: &[&str]) -> bool {
    args.get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| read_ops.contains(&op))
}

/// One registered tool.
pub struct ToolSpec {
    /// MCP tool name (e.g. `ishoo_status`).
    pub name: &'static str,
    /// The inventory capability this tool covers (see [`IN_SCOPE_CAPABILITIES`]).
    pub capability: &'static str,
    /// The core fn this tool binds to (DEC-49 — documents the seam).
    pub core_fn: &'static str,
    /// Whether a successful call mutates the store, as a function of the call's
    /// arguments (so the server snapshots it to the `refs/ishoo/store` ref and
    /// best-effort pushes, like the CLI autocommit wrapper — FIX-76 /
    /// DEC-51/52/54). Most tools use [`mutates_always`]/[`mutates_never`]; an
    /// op-dispatched tool (DEC-86, e.g. `ishoo_plan`) inspects its `op` so a read op
    /// stays non-mutating and skips the snapshot.
    pub mutates_store: fn(&Value) -> bool,
    /// The issue that delivers (or delivered) the handler.
    pub owner_issue: &'static str,
    /// Human-/agent-readable description shown in `tools/list`.
    pub description: &'static str,
    /// Builds the tool's JSON-Schema for `arguments`.
    pub input_schema: fn() -> Value,
    /// The handler, or `None` while the owning slice is still pending.
    pub handler: Option<Handler>,
}

/// The agent-facing core surface the MCP chain delivers. The coverage test keeps
/// this in lockstep with the registry. (Consumed by the coverage check and tests;
/// allow(dead_code) so the bin build doesn't flag the contract API as unused.)
#[allow(dead_code)]
pub const IN_SCOPE_CAPABILITIES: &[&str] = &[
    "status",
    "brief",
    "show",
    "list",
    "candidates",
    "hero_signal",
    "plan",
    "new",
    "decompose",
    "resolve",
    "set_active",
    "start",
    "done",
    "delete",
    "decline",
    "supersede",
    "rename_id",
    "edit",
    "shelve",
    "comment",
    "admin",
    "decision",
];

/// How a root CLI command relates to the MCP agent surface (DEC-49).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliCapabilityClass {
    /// Agents need this command's behavior through MCP. It must have MCP coverage
    /// now or a follow-up issue named in the inventory.
    AgentRequired,
    /// Old or secondary vocabulary whose agent behavior is covered by another
    /// primary MCP tool.
    CompatibilityOnly,
    /// Not part of the interactive agent workflow: an automation/CI surface, or
    /// product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86)
    /// and reachable via CLI/UI, to be re-exposed additively later.
    ScriptOnly,
    /// Human/operator setup or management surface.
    HumanOnly,
    /// Hidden implementation backstop, not a promised user/agent capability.
    InternalOnly,
}

/// One audited CLI root command and its MCP parity status.
#[allow(dead_code)]
pub struct CliCapabilityClassification {
    pub command: &'static str,
    pub class: CliCapabilityClass,
    pub mcp_tools: &'static [&'static str],
    pub follow_up_issues: &'static [&'static str],
    pub rationale: &'static str,
}

/// Root-command MCP parity inventory. Tests compare this against Clap's command
/// registry so a new CLI command cannot appear without an explicit agent-surface
/// classification.
#[allow(dead_code)]
pub const CLI_CAPABILITY_INVENTORY: &[CliCapabilityClassification] = &[
    CliCapabilityClassification {
        command: "init",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "workspace/bootstrap setup, not normal in-repo agent workflow",
    },
    CliCapabilityClassification {
        command: "enable",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "explicit repo onboarding writes user-owned host-adapter files (DEC-88); \
                    humans trigger it through CLI/UI, not during normal MCP agent work",
    },
    CliCapabilityClassification {
        command: "list",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_list"],
        follow_up_issues: &[],
        rationale: "issue discovery/read surface",
    },
    CliCapabilityClassification {
        command: "labels",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "covered by inventory labels",
    },
    CliCapabilityClassification {
        command: "files",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "covered by inventory files",
    },
    CliCapabilityClassification {
        command: "refs",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "covered by inventory issue references",
    },
    CliCapabilityClassification {
        command: "show",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_show"],
        follow_up_issues: &[],
        rationale: "issue detail/read surface",
    },
    CliCapabilityClassification {
        command: "decline",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_decline"],
        follow_up_issues: &[],
        rationale: "issue retirement is agent-required; typed MCP parity shipped as ishoo_decline (CORE-07)",
    },
    CliCapabilityClassification {
        command: "supersede",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_supersede"],
        follow_up_issues: &[],
        rationale: "issue replacement is agent-required; typed MCP parity shipped as ishoo_supersede (CORE-07)",
    },
    CliCapabilityClassification {
        command: "shelve",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_shelve"],
        follow_up_issues: &[],
        rationale: "shelving retained-knowledge issues without done gates (DEC-90) is typed for agents",
    },
    CliCapabilityClassification {
        command: "decompose",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_decompose"],
        follow_up_issues: &[],
        rationale: "splitting a parent into children with durable lineage (DEC-61)",
    },
    CliCapabilityClassification {
        command: "new",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_new"],
        follow_up_issues: &[],
        rationale: "issue authoring with required Scope Contract schema",
    },
    CliCapabilityClassification {
        command: "edit",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_edit", "ishoo_resolve"],
        follow_up_issues: &["CORE-03"],
        rationale: "issue mutation/resolution is covered; retired status workflow remains CORE-03",
    },
    CliCapabilityClassification {
        command: "move",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "legacy markdown/document partition operation",
    },
    CliCapabilityClassification {
        command: "plan",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_plan"],
        follow_up_issues: &[],
        rationale: "full agent plan inspection + entry control (add/move/remove/clear/populate) + \
                    named-plan lifecycle (new/rename/use/deactivate/archive/delete/drop) is covered \
                    by typed plan tools; only the batch-TOML `plan generate` stays script-only",
    },
    CliCapabilityClassification {
        command: "apply",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "batch document-planning executor, not live agent workflow",
    },
    CliCapabilityClassification {
        command: "batch",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "script-oriented bulk mutation surface",
    },
    CliCapabilityClassification {
        command: "lint",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "agent hygiene/readiness check needs structured findings",
    },
    CliCapabilityClassification {
        command: "link",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "local/remote workspace topology setup",
    },
    CliCapabilityClassification {
        command: "relink",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "machine-local path repair for linked projects",
    },
    CliCapabilityClassification {
        command: "set",
        class: CliCapabilityClass::CompatibilityOnly,
        mcp_tools: &["ishoo_edit", "ishoo_set_active", "ishoo_done"],
        follow_up_issues: &["CORE-03"],
        rationale:
            "status shorthand; primary active/done paths are typed, retired statuses remain CORE-03",
    },
    CliCapabilityClassification {
        command: "rename-id",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_rename_id"],
        follow_up_issues: &[],
        rationale: "issue id recategorization updates refs/plan entries",
    },
    CliCapabilityClassification {
        command: "help",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &["ishoo_brief"],
        follow_up_issues: &[],
        rationale: "human/script help text; agents use tools/list plus ishoo_brief",
    },
    CliCapabilityClassification {
        command: "delete",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_delete"],
        follow_up_issues: &[],
        rationale: "issue deletion is typed and guarded for DONE records",
    },
    CliCapabilityClassification {
        command: "split",
        class: CliCapabilityClass::InternalOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "hidden legacy bulk/document operation",
    },
    CliCapabilityClassification {
        command: "archive",
        class: CliCapabilityClass::InternalOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "hidden legacy bulk/document operation",
    },
    CliCapabilityClassification {
        command: "start",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_start"],
        follow_up_issues: &[],
        rationale: "begin execution claim/worktree",
    },
    CliCapabilityClassification {
        command: "finish",
        class: CliCapabilityClass::CompatibilityOnly,
        mcp_tools: &["ishoo_done"],
        follow_up_issues: &[],
        rationale: "DEC-13 release/cleanup compatibility verb; dropped from the MCP surface (DEC-86), covered by ishoo_done",
    },
    CliCapabilityClassification {
        command: "done",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_done"],
        follow_up_issues: &[],
        rationale: "primary accepted-truth completion verb",
    },
    CliCapabilityClassification {
        command: "land",
        class: CliCapabilityClass::CompatibilityOnly,
        mcp_tools: &["ishoo_done"],
        follow_up_issues: &[],
        rationale: "compatibility alias/shared-tree completion path; dropped from the MCP surface (DEC-86), covered by ishoo_done",
    },
    CliCapabilityClassification {
        command: "reclaim",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "stale-claim recovery is part of agent execution recovery",
    },
    CliCapabilityClassification {
        command: "gc",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "orphaned worktree/claim cleanup is part of agent execution recovery",
    },
    CliCapabilityClassification {
        command: "doctor",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "store durability detect/heal (legacy in-tree store, dangling plan refs, \
                    unpublished refs) is part of agent execution recovery",
    },
    CliCapabilityClassification {
        command: "resolve-store",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "same-record store-conflict resolution (keep-mine/take-remote) is part of \
                    agent execution recovery when the store diverges (FEAT-23, DEC-50)",
    },
    CliCapabilityClassification {
        command: "migrate-stores",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "one-way store wire-format migration (v2 -> v3) is a human-gated operator \
                    action across the project Library; an agent must not trigger an irreversible \
                    fleet-wide flip that older binaries can no longer read (ARCH-04, DEC-62)",
    },
    CliCapabilityClassification {
        command: "claim-refresh",
        class: CliCapabilityClass::InternalOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "hidden claim heartbeat helper",
    },
    CliCapabilityClassification {
        command: "heatmap",
        class: CliCapabilityClass::InternalOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "hidden diagnostic surface",
    },
    CliCapabilityClassification {
        command: "dash",
        class: CliCapabilityClass::InternalOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "hidden diagnostic surface",
    },
    CliCapabilityClassification {
        command: "decision",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_decision"],
        follow_up_issues: &["DECI-01"],
        rationale:
            "agent ADR read/render/author/amend/supersede/delete path is fully covered; supersede-why rationale recording remains DECI-01",
    },
    CliCapabilityClassification {
        command: "milestone",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86); \
                    release planning stays CLI/UI-only, re-exposed additively later",
    },
    CliCapabilityClassification {
        command: "version",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86); \
                    version get/bump/set-source stays CLI/UI-only, re-exposed additively later",
    },
    CliCapabilityClassification {
        command: "charter",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &["ishoo_brief"],
        follow_up_issues: &[],
        rationale: "CORE-04: authoring/editing the project charter is a human/setup activity; \
                    agents CONSUME the charter through ishoo_brief (it is appended to the brief), \
                    so no dedicated charter management tool is on the agent surface",
    },
    CliCapabilityClassification {
        command: "epic",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86); \
                    epic workstreams stay CLI/UI-only, re-exposed additively later",
    },
    CliCapabilityClassification {
        command: "roadmap",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86); \
                    roadmap read/reorder stays CLI/UI-only, re-exposed additively later",
    },
    CliCapabilityClassification {
        command: "people",
        class: CliCapabilityClass::ScriptOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "product-domain breadth deferred off the agent surface in v1 (DEC-85/DEC-86); \
                    people register/list stays CLI/UI-only (as does the machine-local `people use`/\
                    `whoami`), re-exposed additively later",
    },
    CliCapabilityClassification {
        command: "comment",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_comment"],
        follow_up_issues: &[],
        rationale: "agent note add/list/edit/remove path is fully covered",
    },
    CliCapabilityClassification {
        command: "status",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_status"],
        follow_up_issues: &[],
        rationale: "agent orientation card",
    },
    CliCapabilityClassification {
        command: "preflight",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_admin"],
        follow_up_issues: &[],
        rationale: "readiness card should be structured for agents",
    },
    CliCapabilityClassification {
        command: "brief",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_brief"],
        follow_up_issues: &[],
        rationale: "agent protocol/orientation",
    },
    CliCapabilityClassification {
        command: "search-issues",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_candidates"],
        follow_up_issues: &[],
        rationale: "ISS-235: raw concept recall is composed into ishoo_candidates, the agent's \
                    prioritization read surface (concept ∪ safety/breaking anchor, minus blocked)",
    },
    CliCapabilityClassification {
        command: "candidates",
        class: CliCapabilityClass::AgentRequired,
        mcp_tools: &["ishoo_candidates"],
        follow_up_issues: &[],
        rationale: "ISS-235: the bounded next-work candidate set for charter→lens→sequence \
                    prioritization",
    },
    CliCapabilityClassification {
        command: "mcp",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "host adapter process entrypoint, not an MCP tool",
    },
    CliCapabilityClassification {
        command: "mcp-owner",
        class: CliCapabilityClass::HumanOnly,
        mcp_tools: &[],
        follow_up_issues: &[],
        rationale: "resident transport owner process entrypoint, not an MCP tool",
    },
];

/// The MCP-44 CLI↔MCP capability-parity inventory: every CLI command classified
/// (e.g. `AgentRequired`, `HumanOnly`) against the MCP tools that cover it, with
/// the rationale and any follow-up issues. It is the source of truth the parity
/// audit diffs against the registered `ishoo_*` tools so the agent surface stays
/// complete under DEC-49 (UI serves humans, CLI serves scripts, MCP serves agents).
#[allow(dead_code)]
pub fn cli_capability_inventory() -> &'static [CliCapabilityClassification] {
    CLI_CAPABILITY_INVENTORY
}

/// The full tool registry.
pub fn registry() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "ishoo_status",
            capability: "status",
            core_fn: "main_dispatch::status_report",
            mutates_store: mutates_never,
            owner_issue: "MCP-02",
            description: "Live orientation: workspace root, store health, active plan, current \
                          focus (claim/worktree/contract state), the recommended next command, \
                          and any gated command. Mirrors `ishoo status`.",
            input_schema: empty_object_schema,
            handler: Some(handle_status),
        },
        ToolSpec {
            name: "ishoo_brief",
            capability: "brief",
            core_fn: "main_dispatch::agent_brief",
            mutates_store: mutates_never,
            owner_issue: "MCP-32",
            description: "The full agent protocol (same as `ishoo brief`): SEMMAP-first workflow, \
                          Scope/Resolution Contracts, the land gates. Auto-loaded into context via \
                          the MCP initialize instructions; call this to re-read on demand (e.g. \
                          after context compaction).",
            input_schema: empty_object_schema,
            handler: Some(handle_brief),
        },
        ToolSpec {
            name: "ishoo_show",
            capability: "show",
            core_fn: "model::issue_view",
            mutates_store: mutates_never,
            owner_issue: "MCP-05",
            description: "Show one issue in full: fields plus derived scope/resolution \
                          completeness. Mirrors `ishoo show <id>`.",
            input_schema: id_arg_schema,
            handler: Some(handle_show),
        },
        ToolSpec {
            name: "ishoo_list",
            capability: "list",
            core_fn: "model::issue_list",
            mutates_store: mutates_never,
            owner_issue: "MCP-05",
            description: "List/query issues with stats. Mirrors `ishoo list`.",
            input_schema: list_args_schema,
            handler: Some(handle_list),
        },
        ToolSpec {
            name: "ishoo_candidates",
            capability: "candidates",
            core_fn: "model::candidate_gather::gather_candidates",
            mutates_store: mutates_never,
            owner_issue: "ISS-235",
            description: "Gather the bounded next-work candidate set for a `lens` (a short phrase \
                          of high-priority concepts you draft from the charter/current phase). \
                          Composes concept recall (ISS-234 search by meaning) with an always-on \
                          `safety`/`breaking` label anchor backstop, restricted to live issues, \
                          minus anything blocked by an unfinished dependency. Read-only; persists \
                          nothing (priority is relative/time-varying, never a stored field). Returns \
                          the candidates with why each was included (`by_concept`/`by_anchor`, score), \
                          `total_live` vs the candidate count, and `dropped_blocked`. Deep-read and \
                          sequence only this bounded set — not the whole store. Mirrors \
                          `ishoo candidates`.",
            input_schema: candidates_args_schema,
            handler: Some(handle_candidates),
        },
        ToolSpec {
            // DEC-86 rule 4 lists hero_signal among deferred product breadth, but it
            // is the only such entity with NO CLI/UI writer — removing it would orphan
            // its write-core and contradict DEC-80 (agents may contribute `active`).
            // Kept on the agent surface pending a DEC-80-vs-DEC-86 resolution (see MCP-61).
            name: "ishoo_hero_signal",
            capability: "hero_signal",
            core_fn: "model::record_home_hero_activity_signal",
            mutates_store: mutates_always,
            owner_issue: "MCP-55",
            description: "Record a typed, expiring Home hero activity signal. Agents may contribute \
                          only enum activity facts for the active state; this tool accepts no \
                          display prose and cannot produce attention.",
            input_schema: home_hero_signal_record_args_schema,
            handler: Some(handle_home_hero_signal_record),
        },
        ToolSpec {
            name: "ishoo_plan",
            capability: "plan",
            core_fn: "model::plan_* (op-dispatched)",
            mutates_store: plan_op_mutates,
            owner_issue: "MCP-57",
            description: "All plan operations, selected by `op` (DEC-86 — replaces the ishoo_plan_* \
                          tools). Reads: `next` (the ready front), `show` (a plan in full; optional \
                          `name`), `list` (every plan). Writes: `use`/`new`/`rename`/`deactivate`/\
                          `archive`/`delete`/`drop` (take `name`), `set` (`ids`), `add`/`move` \
                          (`ref` plus optional `after`/`before`), `remove` (`ref`), `populate` \
                          (optional `labels`), `clear`, `milestone` (`plan` plus optional \
                          `milestone`). Mirrors `ishoo plan <op>`; derived order from depends_on \
                          (via ishoo_edit) is usually preferable to manual sequencing.",
            input_schema: ishoo_plan_args_schema,
            handler: Some(handle_plan),
        },
        ToolSpec {
            name: "ishoo_new",
            capability: "new",
            core_fn: "model::create_issue",
            mutates_store: mutates_always,
            owner_issue: "MCP-04",
            description: "Create an issue with a complete Scope Contract. The four contract \
                          fields (concrete_change, main_surface, proof_of_done, out_of_scope) \
                          are schema-required and assembled into the canonical contract. Assess \
                          urgency at creation in `labels`: `urgent` interrupts across plans; \
                          `important` is high-value active-plan work; `mid` is normal work; \
                          `later` is deferred; `shelved` is retained knowledge excluded from \
                          normal next work. Choose one tier, or intentionally leave it unlabeled \
                          (DEC-90).",
            input_schema: new_args_schema,
            handler: Some(handle_new),
        },
        ToolSpec {
            name: "ishoo_decompose",
            capability: "decompose",
            core_fn: "model::decompose",
            mutates_store: mutates_always,
            owner_issue: "FEAT-15",
            description: "Split a parent issue into one or more child issues, recording the typed \
                          parent↔child decomposition lineage in one operation (DEC-61). Each child \
                          declares a full Scope Contract (concrete_change, main_surface, \
                          proof_of_done, out_of_scope) plus its ADR/blocker linkage, exactly like \
                          ishoo_new; children inherit the parent's plan. The relation is distinct \
                          from links/depends_on and does not retire the parent (it stays an \
                          umbrella). Use this for a scope-BLOCK-driven split so lineage is durable \
                          instead of orphaned sub-issues.",
            input_schema: decompose_args_schema,
            handler: Some(handle_decompose),
        },
        ToolSpec {
            name: "ishoo_resolve",
            capability: "resolve",
            core_fn: "model::cli_edit",
            mutates_store: mutates_always,
            owner_issue: "MCP-04",
            description: "Write an issue's Resolution Contract. The four contract fields \
                          (what_changed, why, verification, handoff) are schema-required and \
                          assembled into the canonical contract.",
            input_schema: resolve_args_schema,
            handler: Some(handle_resolve),
        },
        ToolSpec {
            name: "ishoo_edit",
            capability: "edit",
            core_fn: "model::cli_edit",
            mutates_store: mutates_always,
            owner_issue: "MCP-13",
            description: "Edit an existing issue's non-resolution fields by id: title, description \
                          (Scope Contract text), labels, files, links, depends_on, decisions, owner. \
                          Only provided fields change; a present empty value clears (\"\" for a \
                          scalar, [] for a list). Mirrors `ishoo edit` — incl. the brief's \
                          `--depends-on` blocker conversion. category is not editable (it is the id \
                          prefix — use ishoo_rename_id); resolution is owned by ishoo_resolve; \
                          status by set_active/start/land.",
            input_schema: edit_args_schema,
            handler: Some(handle_edit),
        },
        ToolSpec {
            name: "ishoo_comment",
            capability: "comment",
            core_fn: "model::*_comment_by_id (op-dispatched)",
            mutates_store: comment_op_mutates,
            owner_issue: "MCP-59",
            description: "All comment operations on an issue, selected by `op` (DEC-86 — replaces \
                          the ishoo_comment_* tools). Read: `list` (an issue's comments in order; \
                          `id`). Writes: `add` (`id`, `text`, optional `author`), `edit` (`id`, \
                          0-based `index`, `text`), `remove` (`id`, 0-based `index`). `index` is \
                          oldest-first, matching `op:list` order. Mirrors `ishoo comment <op>`.",
            input_schema: ishoo_comment_args_schema,
            handler: Some(handle_comment),
        },
        ToolSpec {
            name: "ishoo_admin",
            capability: "admin",
            core_fn: "model::{inventory,lint,preflight,doctor,reclaim,gc} (op-dispatched)",
            mutates_store: mutates_never,
            owner_issue: "MCP-60",
            description: "Maintenance & diagnostics, selected by `op` (DEC-86 — replaces the bare \
                          ishoo_inventory/lint/preflight/doctor/reclaim/gc tools). `op:inventory` \
                          (in-use labels/files/refs), `op:lint` (store-integrity findings; \
                          `strict:true`), `op:preflight` (land-readiness card for `id`), `op:doctor` \
                          (diagnose store durability; `fix:true` heals), `op:reclaim` (force-take a \
                          stale claim for `id`), `op:gc` (sweep orphaned worktrees/claims/branches). \
                          None snapshot the store ref. Mirrors the matching `ishoo` command.",
            input_schema: ishoo_admin_args_schema,
            handler: Some(handle_admin),
        },
        ToolSpec {
            name: "ishoo_delete",
            capability: "delete",
            core_fn: "model::Workspace::delete_issue",
            mutates_store: mutates_always,
            owner_issue: "MCP-15",
            description: "Permanently delete an issue by id and prune any dangling plan entries \
                          (mirrors `ishoo delete`), returning the deleted id/title and the \
                          pruned-entry count. A DONE issue is refused unless force:true — guarding \
                          landed history, since there is no interactive confirmation.",
            input_schema: delete_args_schema,
            handler: Some(handle_delete),
        },
        ToolSpec {
            name: "ishoo_shelve",
            capability: "shelve",
            core_fn: "model::cli_shelve",
            mutates_store: mutates_always,
            owner_issue: "URGE-05",
            description: "Shelve a `shelved`-labeled issue as retained knowledge (DEC-90): retire it \
                          (status declined, leaves the live queue, stays queryable per DEC-47) \
                          WITHOUT the normal implementation-completion gates. The issue must already \
                          carry the `shelved` tier label (add it via ishoo_edit) — use ishoo_delete \
                          to remove, or the CLI decline verb to retire a non-shelved issue. A \
                          non-empty `reason` is required. Mirrors `ishoo shelve`.",
            input_schema: shelve_args_schema,
            handler: Some(handle_shelve),
        },
        ToolSpec {
            name: "ishoo_decline",
            capability: "decline",
            core_fn: "model::cli_decline",
            mutates_store: mutates_always,
            owner_issue: "CORE-07",
            description: "Decline an issue (DEC-47): retire a rejected idea to status declined so it \
                          leaves the live queue and `plan next` but stays queryable in list/search as \
                          rejected knowledge. A non-empty `reason` is required. Use ishoo_supersede \
                          instead when the work is replaced by a specific issue, or ishoo_delete to \
                          erase a true typo/test record. Mirrors `ishoo decline`.",
            input_schema: decline_args_schema,
            handler: Some(handle_decline),
        },
        ToolSpec {
            name: "ishoo_supersede",
            capability: "supersede",
            core_fn: "model::cli_supersede",
            mutates_store: mutates_always,
            owner_issue: "CORE-07",
            description: "Supersede an issue with a replacement (DEC-47): retire the superseded issue \
                          (status declined, leaves the live queue, stays queryable) and record the \
                          replacement id so the replacement surfaces what it replaced. Requires \
                          `replacement` (an existing, different issue id) and a non-empty `reason`. \
                          Mirrors `ishoo supersede`.",
            input_schema: supersede_args_schema,
            handler: Some(handle_supersede),
        },
        ToolSpec {
            name: "ishoo_rename_id",
            capability: "rename_id",
            core_fn: "model::rename_issue_id",
            mutates_store: mutates_always,
            owner_issue: "MCP-30",
            description: "Rename an issue's id (mirrors `ishoo rename-id`) — the proper way to \
                          re-categorize (change the prefix), since the id is the primary key and \
                          not an editable field. Updates links/depends_on references and plan \
                          entries across all plans. Returns old_id and new_id.",
            input_schema: rename_id_args_schema,
            handler: Some(handle_rename_id),
        },
        ToolSpec {
            name: "ishoo_set_active",
            capability: "set_active",
            core_fn: "model::gates::set_active",
            mutates_store: mutates_always,
            owner_issue: "MCP-06",
            description: "Set an issue active (subject to the plan-front and scope-contract gates). \
                          Returns a structured BeginVerdict.",
            input_schema: id_arg_schema,
            handler: Some(handle_set_active),
        },
        ToolSpec {
            name: "ishoo_start",
            capability: "start",
            core_fn: "model::gates::start",
            mutates_store: mutates_always,
            owner_issue: "MCP-06",
            description: "Claim an issue and create its execution worktree (after the begin \
                          gates). Returns a structured BeginVerdict.",
            input_schema: id_arg_schema,
            handler: Some(handle_start),
        },
        ToolSpec {
            name: "ishoo_done",
            capability: "done",
            core_fn: "model::gates::land",
            mutates_store: mutates_always,
            owner_issue: "MCP-43",
            description: "Complete an issue through the primary DEC-13 lifecycle verb. On a live \
                          execution worktree this auto-commits from the Resolution Contract, runs \
                          the gates, fast-forwards main, pushes, and tears down the worktree; \
                          without a worktree it follows the shared-tree completion gates. \
                          Compatibility alias: ishoo_land.",
            input_schema: land_args_schema,
            handler: Some(handle_land),
        },
        ToolSpec {
            name: "ishoo_decision",
            capability: "decision",
            core_fn: "model::decision_* (op-dispatched)",
            mutates_store: decision_op_mutates,
            owner_issue: "MCP-58",
            description: "All ADR operations, selected by `op` (DEC-86 — replaces the \
                          ishoo_decision_* tools). Reads: `show` (one ADR in full; `id`), `list` \
                          (id/title/status/labels; optional `label`/`text` filters), `adr` (render \
                          one ADR as canonical markdown; `id`). Writes: `new` (author PROPOSED — \
                          `title`/`decision`/`problem` required, plus optional section fields), \
                          `accept` (`id`), `edit` (`id` + section fields; `confirm:true`), \
                          `supersede` (`superseded_id`/`new_id`/`reason`), `delete` (`id`, \
                          `confirm:true` — discouraged, prefer supersede per DEC-12). Mirrors \
                          `ishoo decision <op>`.",
            input_schema: ishoo_decision_args_schema,
            handler: Some(handle_decision),
        },
    ]
}

/// Capabilities declared in-scope that have no registered tool. Empty means full
/// coverage. [`registry`] is the source of truth for what is covered.
#[allow(dead_code)]
pub fn coverage_gaps() -> Vec<&'static str> {
    let covered: HashSet<&str> = registry().iter().map(|tool| tool.capability).collect();
    gaps_against(&covered)
}

/// Pure coverage diff: which in-scope capabilities are absent from `covered`.
#[allow(dead_code)]
fn gaps_against(covered: &HashSet<&str>) -> Vec<&'static str> {
    IN_SCOPE_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| !covered.contains(capability))
        .collect()
}

// --- JSON-Schema builders -------------------------------------------------

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

/// `ishoo_lint` arguments: an optional `strict` flag adding the export-drift and
/// malformed-contract checks on top of the store-integrity base (MCP-45).
/// `ishoo_admin` (MCP-60 / DEC-86): `op` selects the maintenance/diagnostic
/// operation; the remaining properties are the union of every op's fields.
fn ishoo_admin_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ["inventory", "lint", "preflight", "doctor", "reclaim", "gc", "resolve-store"],
                "description": "Which maintenance/diagnostic operation to run."
            },
            "strict": { "type": "boolean", "description": "lint: also run strict checks (export drift + malformed contracts)." },
            "id": { "type": "string", "description": "Issue id — for preflight/reclaim." },
            "fix": { "type": "boolean", "description": "doctor: apply the bounded heal after diagnosis (default false)." },
            "side": { "type": "string", "enum": ["keep-mine", "take-remote", "newest"], "description": "resolve-store: how to resolve each conflicting record — keep-mine, take-remote, or newest (newest-mutation-wins, the diverged-store recovery default that backs up both tips first); omit to only list the conflicting records (DEC-50/FEAT-32)." }
        },
        "required": ["op"],
        "additionalProperties": false
    })
}

fn id_arg_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "Issue id, e.g. MCP-03" } },
        "required": ["id"],
        "additionalProperties": false
    })
}

/// `ishoo_candidates` arguments (ISS-235): the required `lens` phrase plus an
/// optional `top` cap on the concept-recall hits unioned into the set.
fn candidates_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "lens": {
                "type": "string",
                "description": "A short phrase of high-priority concepts (from the charter / current phase) to rank candidates by meaning"
            },
            "top": {
                "type": "integer",
                "minimum": 1,
                "description": "Max concept-search hits to union into the candidate set (default 12)"
            }
        },
        "required": ["lens"],
        "additionalProperties": false
    })
}

fn home_hero_signal_record_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation_id": {
                "type": "string",
                "description": "Stable idempotency key for this agent operation; a repeated id replaces the prior signal"
            },
            "kind": {
                "type": "string",
                "enum": [
                    "working",
                    "running_checks",
                    "preparing_resolution",
                    "waiting_on_tool",
                    "handoff_ready"
                ],
                "description": "Enum-only activity signal. No display text is accepted."
            },
            "source": {
                "type": "string",
                "description": "Attribution for the signal source, such as the agent or host id"
            },
            "ttl_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600,
                "description": "Optional lifetime in seconds before the signal expires; default 300"
            }
        },
        "required": ["operation_id", "kind", "source"],
        "additionalProperties": false
    })
}

fn land_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id to land" }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

/// The structured ADR section properties shared by decision new/edit.
fn decision_section_properties() -> Value {
    let str_array = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "title": { "type": "string" },
        "decision": { "type": "string" },
        "problem": { "type": "string" },
        "scope": { "type": "string" },
        "rule": { "type": "string" },
        "consequences": { "type": "string" },
        "alternatives_rejected": { "type": "string" },
        "operational_impact": { "type": "string" },
        "supporting_note": { "type": "string" },
        "related_issues": str_array,
        "related_files": str_array,
        "tags": str_array
    })
}

/// `ishoo_decision` (MCP-58 / DEC-86): `op` selects the operation; the remaining
/// properties are the union of every op's fields (ADR section fields plus the
/// ids/filters/flags the ops use). Permissive by design; the handlers validate.
fn ishoo_decision_args_schema() -> Value {
    let mut props = decision_section_properties();
    props["op"] = json!({
        "type": "string",
        "enum": ["show", "list", "adr", "new", "accept", "edit", "supersede", "delete"],
        "description": "Which ADR operation to run (mirrors `ishoo decision <op>`)."
    });
    props["id"] =
        json!({ "type": "string", "description": "ADR id — for show/adr/accept/edit/delete." });
    props["label"] =
        json!({ "type": "string", "description": "Filter by domain label — for list." });
    props["text"] =
        json!({ "type": "string", "description": "Filter by text substring — for list." });
    props["superseded_id"] =
        json!({ "type": "string", "description": "OLD ADR id — for supersede." });
    props["new_id"] =
        json!({ "type": "string", "description": "NEW ADR id that replaces it — for supersede." });
    props["reason"] = json!({ "type": "string", "description": "Why the old ADR is replaced — for supersede (DECI-01)." });
    props["confirm"] = json!({
        "type": "boolean",
        "description": "Must be true for edit (typos/wording only — meaning changes supersede, DEC-12) and delete (discouraged; prefer supersede)."
    });
    json!({
        "type": "object",
        "properties": props,
        "required": ["op"],
        "additionalProperties": false
    })
}

fn rename_id_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Current issue id" },
            "new_id": { "type": "string", "description": "New id, e.g. CLI-03 (re-categorizes the prefix)" }
        },
        "required": ["id", "new_id"],
        "additionalProperties": false
    })
}

fn delete_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id to delete" },
            "force": {
                "type": "boolean",
                "description": "Required to delete a DONE issue (default false)"
            }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

fn handle_shelve(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let reason = required_str(args, "reason")?;
    let mut workspace = load_workspace(path)?;
    model::cli_shelve(&mut workspace, &id, &reason).map_err(ToolError::invalid_params)?;
    Ok(json!({
        "id": id,
        "shelved": true,
        "status": "declined",
        "reason": reason,
    }))
}

fn shelve_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id to shelve (must carry the `shelved` label)" },
            "reason": {
                "type": "string",
                "description": "Required non-empty rationale for shelving this retained-knowledge issue"
            }
        },
        "required": ["id", "reason"],
        "additionalProperties": false
    })
}

/// CORE-07 (DEC-47/DEC-49): decline a rejected idea. Shares `model::cli_decline`
/// with the CLI `decline` verb, so the required-rationale rule lives at the core.
fn handle_decline(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let reason = required_str(args, "reason")?;
    let mut workspace = load_workspace(path)?;
    model::cli_decline(&mut workspace, &id, &reason).map_err(ToolError::invalid_params)?;
    Ok(json!({
        "id": id,
        "status": "declined",
        "reason": reason,
    }))
}

fn decline_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id to decline (retire as a rejected idea)" },
            "reason": {
                "type": "string",
                "description": "Required non-empty rationale for declining this issue"
            }
        },
        "required": ["id", "reason"],
        "additionalProperties": false
    })
}

/// CORE-07 (DEC-47/DEC-49): supersede an issue with a replacement. Shares
/// `model::cli_supersede` with the CLI `supersede` verb — the existence/self-ref
/// checks and required rationale are enforced once, at the core.
fn handle_supersede(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let replacement = required_str(args, "replacement")?;
    let reason = required_str(args, "reason")?;
    let mut workspace = load_workspace(path)?;
    model::cli_supersede(&mut workspace, &id, &replacement, &reason)
        .map_err(ToolError::invalid_params)?;
    Ok(json!({
        "id": id,
        "status": "declined",
        "superseded_by": replacement.trim(),
        "reason": reason,
    }))
}

fn supersede_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id being superseded (retired)" },
            "replacement": {
                "type": "string",
                "description": "The replacement issue id — must exist and differ from `id`"
            },
            "reason": {
                "type": "string",
                "description": "Required non-empty rationale for the replacement"
            }
        },
        "required": ["id", "replacement", "reason"],
        "additionalProperties": false
    })
}

/// `ishoo_comment` (MCP-59 / DEC-86): `op` selects the operation; the remaining
/// properties are the union of every op's fields. Permissive; the handlers validate.
fn ishoo_comment_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ["list", "add", "edit", "remove"],
                "description": "Which comment operation to run (mirrors `ishoo comment <op>`)."
            },
            "id": { "type": "string", "description": "Issue id to act on (all ops)." },
            "text": { "type": "string", "description": "Comment body — for add/edit." },
            "author": { "type": "string", "description": "Optional author; defaults to the issue owner — for add." },
            "index": { "type": "integer", "minimum": 0, "description": "0-based, oldest-first comment index (as op:list orders them) — for edit/remove." }
        },
        "required": ["op"],
        "additionalProperties": false
    })
}

/// `ishoo_plan` (MCP-57 / DEC-86): `op` selects the operation; the remaining
/// properties are the union of every op's fields (each op uses only its own — see
/// the tool description for which). Kept permissive (no per-op `required`) because
/// JSON-Schema cannot cheaply express per-op requirements; the handlers validate.
fn ishoo_plan_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": [
                    "next", "show", "list", "use", "set", "add", "move", "remove",
                    "clear", "populate", "new", "rename", "deactivate", "archive",
                    "delete", "drop", "milestone"
                ],
                "description": "Which plan operation to run (mirrors `ishoo plan <op>`)."
            },
            "name": { "type": "string", "description": "Plan name — for show/use/new/rename/archive/delete/drop." },
            "ref": { "type": "string", "description": "Issue id — for add/move/remove." },
            "after": { "type": "string", "description": "Anchor issue id to position after — add/move." },
            "before": { "type": "string", "description": "Anchor issue id to position before — add/move." },
            "ids": { "type": "array", "items": { "type": "string" }, "description": "Ordered issue ids — for set." },
            "labels": { "type": "array", "items": { "type": "string" }, "description": "Filter labels — for populate." },
            "plan": { "type": "string", "description": "Plan name or id — for milestone." },
            "milestone": { "type": "string", "description": "Milestone id to link, omit to clear — for milestone." }
        },
        "required": ["op"],
        "additionalProperties": false
    })
}

fn list_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "description": "Filter by status: active, backlog, done" },
            "label": { "type": "string", "description": "Filter by label (issue must carry it)" },
            "prefix": { "type": "string", "description": "Filter by id category prefix, e.g. FIX, MCP" },
            "text": { "type": "string", "description": "Substring filter over id/title/description/labels" },
            "recorded_after": { "type": "string", "description": "Only issues recorded on/after this ISO date (YYYY-MM-DD)" },
            "recorded_before": { "type": "string", "description": "Only issues recorded on/before this ISO date (YYYY-MM-DD)" },
            "compact": { "type": "boolean", "description": "Return a bounded, paginated projection (id/title/status/recorded_at/plan/labels) instead of full records — use this to audit a large store without truncation" },
            "limit": { "type": "integer", "description": "Max compact rows to return (default 200, max 1000)" },
            "offset": { "type": "integer", "description": "Compact-mode pagination offset (default 0)" }
        },
        "additionalProperties": false
    })
}

/// `ishoo_new` arguments: the four Scope Contract fields are required, alongside
/// title, a named plan home, and explicit ADR/blocker linkage (ADR-02 / DEC-43).
fn new_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "mutation_id": {
                "type": "string",
                "description": "Optional idempotency key for retrying a timed-out ishoo_new call"
            },
            "category": { "type": "string", "description": "Id prefix, e.g. fix, cli (default iss)" },
            "plan": { "type": "string", "description": "Existing plan name, or new:\"<name>\" (DEC-55)" },
            "concrete_change": { "type": "string" },
            "main_surface": { "type": "string" },
            "proof_of_done": { "type": "string" },
            "out_of_scope": { "type": "string" },
            "decisions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Governing DEC ids; [] asserts none constrains it (ADR-02)"
            },
            "depends_on": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Blocking issue ids; [] asserts none blocks it (DEC-43)"
            },
            "labels": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional canonical labels, max 5; unknown labels are stripped with a warning in the result. Assess urgency at creation: urgent interrupts across plans; important is high-value active-plan work; mid is normal work; later is deferred; shelved is retained knowledge excluded from normal next work. Choose one tier, or intentionally leave it unlabeled (DEC-90)."
            },
            "files": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional source files this issue touches — matches ishoo_edit (MCP-50)"
            }
        },
        "required": [
            "title", "plan", "concrete_change", "main_surface", "proof_of_done",
            "out_of_scope", "decisions", "depends_on"
        ],
        "additionalProperties": false
    })
}

/// `ishoo_decompose` arguments: the parent id plus a non-empty `children` array,
/// each child carrying the same Scope Contract + ADR/blocker fields as `ishoo_new`
/// (DEC-61). Children inherit the parent's plan, so no per-child plan is accepted.
fn decompose_args_schema() -> Value {
    let str_array = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "properties": {
            "parent": { "type": "string", "description": "Id of the parent issue to split (DEC-61)" },
            "children": {
                "type": "array",
                "minItems": 1,
                "description": "One or more child issues to carve from the parent; each inherits the parent's plan.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "category": { "type": "string", "description": "Id prefix, e.g. fix, cli (default iss)" },
                        "concrete_change": { "type": "string" },
                        "main_surface": { "type": "string" },
                        "proof_of_done": { "type": "string" },
                        "out_of_scope": { "type": "string" },
                        "decisions": str_array.clone(),
                        "depends_on": str_array
                    },
                    "required": [
                        "title", "concrete_change", "main_surface", "proof_of_done",
                        "out_of_scope", "decisions", "depends_on"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["parent", "children"],
        "additionalProperties": false
    })
}

/// `ishoo_resolve` arguments: the issue id plus the four Resolution Contract
/// fields, all required.
fn resolve_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "what_changed": { "type": "string" },
            "why": { "type": "string" },
            "verification": { "type": "string" },
            "handoff": { "type": "string" }
        },
        "required": ["id", "what_changed", "why", "verification", "handoff"],
        "additionalProperties": false
    })
}

/// `ishoo_edit` arguments: id plus optional non-resolution fields. A present empty
/// value clears: `""` for a scalar (title/description/owner), `[]` for a list. An
/// absent key leaves the field unchanged. (`category` is not here — see rename.)
fn edit_args_schema() -> Value {
    let str_array = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Issue id to edit" },
            "title": { "type": "string" },
            "description": { "type": "string", "description": "Full description / Scope Contract text" },
            "labels": str_array,
            "files": str_array,
            "links": str_array,
            "depends_on": str_array,
            "decisions": str_array,
            "owner": { "type": "string" }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

// --- Handlers -------------------------------------------------------------

/// `ishoo_status` — serialize the typed orientation report (MCP-02).
fn handle_status(path: &Path, _args: &Value) -> ToolResult {
    // FIX-122 (DEC-77): load the store through the fallible MCP loader so a
    // missing/uninitialized store becomes a structured tool error (naming the
    // path + `ishoo init`), never the CLI's `process::exit` that would kill the
    // stdio transport. We only build the report once the store has loaded.
    let workspace = load_workspace(path)?;
    let report = crate::main_dispatch::status_report_with_workspace(path, workspace);
    let mut value = serde_json::to_value(&report).map_err(|error| {
        ToolError::invalid_params(format!("Failed to serialize status: {error}"))
    })?;
    if let (Some(map), Some(startup)) = (
        value.as_object_mut(),
        super::mcp_startup_store_sync_for(path),
    ) {
        map.insert("mcp_startup_store_sync".to_string(), startup);
    }
    Ok(value)
}

/// `ishoo_brief` (MCP-32) — the full agent protocol text on demand, from the same
/// source as the CLI `ishoo brief` (honoring the `~/.ishoo/brief.md` override).
fn handle_brief(path: &Path, _args: &Value) -> ToolResult {
    Ok(json!({ "brief": crate::main_dispatch::agent_brief(path) }))
}

/// `ishoo_show` (MCP-05) — the typed view of one issue (record + derived contract
/// completeness), via the `issue_view` core fn.
fn handle_show(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let workspace = load_workspace(path)?;
    match model::issue_view(&workspace, &id) {
        Some(view) => {
            let mut value = serde_json::to_value(&view).map_err(|e| {
                ToolError::invalid_params(format!("Failed to serialize issue: {e}"))
            })?;
            qualify_issue_projection_metadata(&mut value);
            with_governing(path, value, Some(&id))
        }
        None => Err(ToolError::invalid_params(format!("issue '{id}' not found"))),
    }
}

/// `ishoo_candidates` (ISS-235) — the bounded next-work candidate set for a lens,
/// via the `candidate_gather::gather_candidates` core fn. Read-only. Returns the
/// candidates (with inclusion reasons + score), `total_live`, `concept_hits`,
/// `dropped_blocked`, and a `note` when concept search was unavailable.
fn handle_candidates(path: &Path, args: &Value) -> ToolResult {
    let lens = required_str(args, "lens")?;
    let top = args
        .get("top")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(12);
    let set = model::candidate_gather::gather_candidates(path, &lens, top)
        .map_err(ToolError::invalid_params)?;
    let candidates: Vec<Value> = set
        .candidates
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "title": c.title,
                "labels": c.labels,
                "concept_score": c.concept_score,
                "rank_score": c.rank_score,
                "by_concept": c.by_concept,
                "by_anchor": c.by_anchor,
            })
        })
        .collect();
    Ok(json!({
        "lens": set.lens,
        "candidates": candidates,
        "candidate_count": set.candidates.len(),
        "total_live": set.total_live,
        "concept_hits": set.concept_hits,
        "concept_search_ran": set.concept_search_ran,
        "dropped_blocked": set.dropped_blocked,
        "note": set.note,
    }))
}

fn handle_home_hero_signal_record(path: &Path, args: &Value) -> ToolResult {
    reject_unknown_keys(args, &["operation_id", "kind", "source", "ttl_secs"])?;
    let operation_id = required_str(args, "operation_id")?;
    let kind = required_str(args, "kind")?;
    let source = required_str(args, "source")?;
    let ttl_secs = optional_u64(args, "ttl_secs")?;

    let outcome = model::record_home_hero_activity_signal(
        path,
        model::HomeHeroActivityInput {
            operation_id,
            kind,
            source,
            ttl_secs,
        },
    )
    .map_err(ToolError::invalid_params)?;

    serde_json::to_value(&outcome).map_err(|e| {
        ToolError::invalid_params(format!(
            "Failed to serialize Home hero activity signal: {e}"
        ))
    })
}

/// Attach `governing_decisions` (the ACCEPTED, non-superseded ADRs) to an
/// orientation/transition payload (MCP-31), so an agent is shown the binding ADRs
/// at the moment it begins work — parity with the CLI card on show/set-active/start.
/// A non-object value is returned unchanged.
fn with_governing(path: &Path, mut value: Value, issue_id: Option<&str>) -> ToolResult {
    if let Some(obj) = value.as_object_mut() {
        let workspace = load_workspace(path)?;
        let refs = issue_id
            .and_then(|id| workspace.issues.iter().find(|issue| issue.id == id))
            .map(|issue| model::governing_decisions_for_issue(&workspace.decisions, issue))
            .unwrap_or_else(|| model::governing_decisions(&workspace.decisions));
        let govs = serde_json::to_value(refs)
            .map_err(|e| ToolError::invalid_params(format!("Failed to serialize ADRs: {e}")))?;
        obj.insert("governing_decisions".to_string(), govs);
    }
    Ok(value)
}

/// `ishoo_list` (MCP-05) — stats plus the query-filtered issue list, via the
/// `issue_list` core fn. Optional `status` and `label` filters mirror the CLI.
fn handle_list(path: &Path, args: &Value) -> ToolResult {
    let workspace = load_workspace(path)?;
    let mut query = model::IssueQuery::default();
    if let Some(status) = optional_str(args, "status") {
        let parsed = model::parse_cli_status(&status).map_err(ToolError::invalid_params)?;
        query.statuses.push(parsed);
    }
    if let Some(label) = optional_str(args, "label") {
        query.labels_all.push(label);
    }
    // MCP-39: richer filters for auditing a large store (apply to both shapes).
    if let Some(prefix) = optional_str(args, "prefix") {
        query.category_prefix.push(prefix);
    }
    if let Some(text) = optional_str(args, "text") {
        query.text = Some(text);
    }
    query.recorded_after = optional_str(args, "recorded_after");
    query.recorded_before = optional_str(args, "recorded_before");

    // MCP-39: opt-in compact, paginated projection — one bounded row per issue so an
    // agent can audit hundreds of issues without a payload that truncates client-side.
    // The default (full sectioned records + stats) is unchanged for back-compat.
    if args
        .get("compact")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let plan_of = issue_plan_index(path);
        let matched: Vec<&model::Issue> = workspace
            .issues
            .iter()
            .filter(|i| query.matches(i))
            .collect();
        let total = matched.len();
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 1000) as usize)
            .unwrap_or(200);
        let issues: Vec<Value> = matched
            .iter()
            .skip(offset)
            .take(limit)
            .map(|issue| {
                json!({
                    "id": issue.id,
                    "title": issue.title,
                    "status": issue.status.label(),
                    "recorded_at": issue.recorded_at,
                    "plan": plan_of
                        .get(issue.id.as_str())
                        .cloned()
                        .unwrap_or_else(|| "Backlog".to_string()),
                    "labels": issue.labels,
                })
            })
            .collect();
        let returned = issues.len();
        return Ok(json!({
            "issues": issues,
            "total": total,
            "offset": offset,
            "limit": limit,
            "returned": returned,
        }));
    }

    let list = model::issue_list(&workspace, &query, model::SectionMode::Status);
    let mut value = serde_json::to_value(&list)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize list: {e}")))?;
    qualify_issue_projection_metadata(&mut value);
    Ok(value)
}

/// MCP-39: map each issue id -> the plan that holds it (a named plan, else the
/// default Backlog), so the compact projection can show plan membership without a
/// per-issue store walk.
fn issue_plan_index(path: &Path) -> std::collections::HashMap<String, String> {
    let plans = model::AllPlans::load(path);
    let mut index = std::collections::HashMap::new();
    for entry in &plans.default_plan.entries {
        index.insert(entry.issue_id.clone(), "Backlog".to_string());
    }
    for named in &plans.named {
        for entry in &named.plan.entries {
            index.insert(entry.issue_id.clone(), named.name.clone());
        }
    }
    index
}

/// MCP read tools speak to agents, so do not expose the legacy `source_file`
/// field as if it were live authority. It is a document/export projection label;
/// the canonical issue record is the store-backed issue object.
fn qualify_issue_projection_metadata(value: &mut Value) {
    if let Some(issue) = value.get_mut("issue") {
        qualify_one_issue_projection(issue);
    }
    if let Some(issues) = value.get_mut("issues").and_then(Value::as_array_mut) {
        for issue in issues {
            qualify_one_issue_projection(issue);
        }
    }
}

fn qualify_one_issue_projection(issue: &mut Value) {
    let document = issue
        .get("source_file")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| model::default_document_name())
        .to_string();

    let Some(obj) = issue.as_object_mut() else {
        return;
    };
    obj.remove("source_file");
    let export_path = format!("docs/issues/{document}");
    obj.insert(
        "export_metadata".to_string(),
        json!({
            "document": document,
            "export_path": export_path,
            "freshness": "unknown",
            "authoritative": false,
        }),
    );
}

/// `ishoo_plan_next` (MCP-05) — the active plan's next ready item (or null), via
/// the `plan_next` core fn.
/// The plan ops that only read the store (so a call naming one skips the
/// post-call snapshot and takes the concurrent read path, not the serial queue).
const PLAN_READ_OPS: &[&str] = &["next", "show", "list"];

/// Whether an `ishoo_plan` call mutates the store — every op except the reads.
fn plan_op_mutates(args: &Value) -> bool {
    !op_is_read(args, PLAN_READ_OPS)
}

/// `ishoo_plan` (MCP-57 / DEC-86) — the single op-dispatched plan tool. Routes
/// `op` to the former per-verb handler; each reads its own fields from `args`.
fn handle_plan(path: &Path, args: &Value) -> ToolResult {
    dispatch_op(
        "ishoo_plan",
        &[
            ("next", handle_plan_next),
            ("show", handle_plan_show),
            ("list", handle_plan_list),
            ("use", handle_plan_use),
            ("set", handle_plan_set),
            ("add", handle_plan_add),
            ("move", handle_plan_move),
            ("remove", handle_plan_remove),
            ("clear", handle_plan_clear),
            ("populate", handle_plan_populate),
            ("new", handle_plan_new),
            ("rename", handle_plan_rename),
            ("deactivate", handle_plan_deactivate),
            ("archive", handle_plan_archive),
            ("delete", handle_plan_delete),
            ("drop", handle_plan_drop),
            ("milestone", handle_plan_milestone),
        ],
        path,
        args,
    )
}

fn handle_plan_next(path: &Path, _args: &Value) -> ToolResult {
    let next = model::plan_next(path);
    serde_json::to_value(&next)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize plan next: {e}")))
}

/// `ishoo_plan_show` (MCP-16) — the active plan in full (derived order + overlays
/// + ready front), via the `plan_view` core fn.
fn handle_plan_show(path: &Path, args: &Value) -> ToolResult {
    // MCP-34: with `name`, inspect that plan read-only (no active-plan change);
    // without it, the active plan as before.
    let view = match optional_str(args, "name") {
        Some(name) => model::plan_view_named(path, &name).map_err(ToolError::invalid_params)?,
        None => model::plan_view(path),
    };
    serde_json::to_value(&view)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize plan: {e}")))
}

/// `ishoo_plan_list` (MCP-22) — every plan (default Backlog + named) with active
/// flag, entry count, and archived flag, so plan_use has discoverable names.
fn handle_plan_list(path: &Path, _args: &Value) -> ToolResult {
    // MCP-34: each plan carries open/done counts (via plan_inventory) so an agent
    // can audit stale or done-only plans in one read.
    let summaries = model::plan_inventory(path);
    let active_plan = summaries
        .iter()
        .find(|p| p.active)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| model::BACKLOG_NAME.to_string());
    serde_json::to_value(&summaries)
        .map(|plans| json!({ "active_plan": active_plan, "plans": plans }))
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize plans: {e}")))
}

/// `ishoo_plan_use` (MCP-16/MCP-22) — activate a plan by name. A named plan is
/// resolved first (so a plan literally named "Backlog" is reachable); only when no
/// named plan matches and the name is the default label do we deactivate to the
/// default plan. Structural, not a magic string. Returns the canonical active label.
fn handle_plan_use(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    match model::AllPlans::switch_to(path, &name) {
        Ok(()) => {}
        Err(named_err) => {
            if name.eq_ignore_ascii_case(model::BACKLOG_NAME) {
                model::AllPlans::deactivate(path).map_err(ToolError::invalid_params)?;
            } else {
                return Err(ToolError::invalid_params(named_err));
            }
        }
    }
    let plans = model::AllPlans::load(path);
    Ok(json!({ "active_plan": plans.active_label() }))
}

/// `ishoo_plan_set` (MCP-16) — reconstruct the active plan from an ordered id list,
/// via the shared `plan_set_active` core. Validates every id; the plan is left
/// unchanged if any fail.
fn handle_plan_set(path: &Path, args: &Value) -> ToolResult {
    let ids = required_str_array(args, "ids")?;
    let outcome = model::plan_set_active(path, &ids).map_err(ToolError::invalid_params)?;
    Ok(json!({
        "active_plan": outcome.label,
        "wholesale_replacement": outcome.wholesale,
        "entry_count": ids.len(),
    }))
}

/// Serialize a plan-entry mutation outcome (MCP-52).
fn plan_entry_outcome_json(outcome: &model::PlanEntryOutcome) -> Value {
    json!({
        "ref": format!("{}/{}", outcome.project_key, outcome.issue_id),
        "project_key": outcome.project_key,
        "issue_id": outcome.issue_id,
        "active_plan": outcome.plan_label,
        "changed": outcome.changed,
    })
}

/// Resolve the optional `after`/`before` anchor; at most one may be given.
fn plan_anchor<'a>(
    after: &'a Option<String>,
    before: &'a Option<String>,
) -> Result<Option<(&'a str, bool)>, ToolError> {
    match (after, before) {
        (Some(_), Some(_)) => Err(ToolError::invalid_params(
            "pass at most one of 'after'/'before', not both",
        )),
        (Some(a), None) => Ok(Some((a.as_str(), true))),
        (None, Some(b)) => Ok(Some((b.as_str(), false))),
        (None, None) => Ok(None),
    }
}

/// `ishoo_plan_add` (MCP-52) — add an issue to the active plan, optionally anchored,
/// via the shared `plan_add_entry` core (print-free).
fn handle_plan_add(path: &Path, args: &Value) -> ToolResult {
    let plan_ref = required_str(args, "ref")?;
    let after = optional_str(args, "after");
    let before = optional_str(args, "before");
    let anchor = plan_anchor(&after, &before)?;
    let outcome =
        model::plan_add_entry(path, &plan_ref, anchor).map_err(ToolError::invalid_params)?;
    Ok(plan_entry_outcome_json(&outcome))
}

/// `ishoo_plan_move` (MCP-52) — reposition an active-plan entry adjacent to an
/// anchor (exactly one of after/before), via the shared `plan_move_entry` core.
fn handle_plan_move(path: &Path, args: &Value) -> ToolResult {
    let plan_ref = required_str(args, "ref")?;
    let after = optional_str(args, "after");
    let before = optional_str(args, "before");
    let (anchor_ref, is_after) = match plan_anchor(&after, &before)? {
        Some((anchor, is_after)) => (anchor, is_after),
        None => {
            return Err(ToolError::invalid_params(
                "ishoo_plan_move requires an anchor: pass exactly one of 'after'/'before'",
            ))
        }
    };
    let outcome = model::plan_move_entry(path, &plan_ref, anchor_ref, is_after)
        .map_err(ToolError::invalid_params)?;
    Ok(plan_entry_outcome_json(&outcome))
}

/// `ishoo_plan_remove` (MCP-52) — drop one entry from the active plan, via the
/// shared `plan_remove_entry` core.
fn handle_plan_remove(path: &Path, args: &Value) -> ToolResult {
    let plan_ref = required_str(args, "ref")?;
    let outcome = model::plan_remove_entry(path, &plan_ref).map_err(ToolError::invalid_params)?;
    Ok(plan_entry_outcome_json(&outcome))
}

/// `ishoo_plan_clear` (MCP-52) — empty the active plan, via the shared
/// `plan_clear_entries` core.
fn handle_plan_clear(path: &Path, _args: &Value) -> ToolResult {
    let label = model::plan_clear_entries(path).map_err(ToolError::invalid_params)?;
    Ok(json!({ "active_plan": label, "cleared": true }))
}

/// `ishoo_plan_populate` (MCP-52) — prepend non-done local issues (optionally
/// label-filtered) to the active plan, via the shared `plan_populate` core.
fn handle_plan_populate(path: &Path, args: &Value) -> ToolResult {
    let labels = match args.get("labels") {
        None => None,
        Some(_) => Some(required_str_array(args, "labels")?.join(",")),
    };
    let outcome =
        model::plan_populate(path, labels.as_deref()).map_err(ToolError::invalid_params)?;
    Ok(json!({ "active_plan": outcome.plan_label, "prepended": outcome.count }))
}

/// `ishoo_plan_new` (MCP-52) — create a named plan and switch to it, via the
/// `AllPlans::new_plan` core.
fn handle_plan_new(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    let plan_id = model::AllPlans::new_plan(path, &name).map_err(ToolError::invalid_params)?;
    Ok(json!({ "name": name, "plan_id": plan_id, "active": true }))
}

/// `ishoo_plan_rename` (MCP-52) — rename the active named plan, via the
/// `AllPlans::rename_active` core.
fn handle_plan_rename(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    model::AllPlans::rename_active(path, &name).map_err(ToolError::invalid_params)?;
    Ok(json!({ "active_plan": name.trim() }))
}

/// `ishoo_plan_deactivate` (MCP-52) — set the active plan back to Backlog, via the
/// `AllPlans::deactivate` core.
fn handle_plan_deactivate(path: &Path, _args: &Value) -> ToolResult {
    model::AllPlans::deactivate(path).map_err(ToolError::invalid_params)?;
    Ok(json!({ "active_plan": model::BACKLOG_NAME }))
}

/// `ishoo_plan_archive` (MCP-52) — hide a named plan from the default list, via the
/// `AllPlans::archive_plan` core (DEC-26).
fn handle_plan_archive(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    model::AllPlans::archive_plan(path, &name).map_err(ToolError::invalid_params)?;
    Ok(json!({ "archived": name }))
}

/// `ishoo_plan_delete` (MCP-52) — delete a named plan only if empty, via the
/// `AllPlans::delete_empty_plan` core (DEC-26).
fn handle_plan_delete(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    model::AllPlans::delete_empty_plan(path, &name).map_err(ToolError::invalid_params)?;
    Ok(json!({ "deleted": name }))
}

/// `ishoo_plan_drop` (MCP-52) — delete a named plan regardless of entries, via the
/// `AllPlans::drop_plan` core.
fn handle_plan_drop(path: &Path, args: &Value) -> ToolResult {
    let name = required_str(args, "name")?;
    model::AllPlans::drop_plan(path, &name).map_err(ToolError::invalid_params)?;
    Ok(json!({ "dropped": name }))
}

/// `ishoo_plan_milestone` (ROAD-03 / DEC-73) — link a named plan to a milestone,
/// or clear the link when `milestone` is omitted, via `AllPlans::set_plan_milestone`.
fn handle_plan_milestone(path: &Path, args: &Value) -> ToolResult {
    let plan = required_str(args, "plan")?;
    let milestone = optional_str(args, "milestone");
    model::AllPlans::set_plan_milestone(path, &plan, milestone.as_deref())
        .map_err(ToolError::invalid_params)?;
    Ok(json!({ "plan": plan, "milestone": milestone }))
}

/// `ishoo_set_active` (MCP-06) — begin gates + set active, via `model::gates`.
fn handle_set_active(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let verdict = model::gates::set_active(path, &id).map_err(ToolError::invalid_params)?;
    let value = serde_json::to_value(&verdict)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize verdict: {e}")))?;
    with_governing(path, value, Some(&id))
}

/// `ishoo_start` (MCP-06) — begin gates + claim/worktree, via `model::gates`.
fn handle_start(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let verdict = model::gates::start(path, &id).map_err(ToolError::invalid_params)?;
    let value = serde_json::to_value(&verdict)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize verdict: {e}")))?;
    with_governing(path, value, Some(&id))
}

fn claim_info_value(info: &model::git_remote::ClaimInfo) -> Value {
    json!({
        "issue_id": info.issue_id,
        "timestamp_secs": info.timestamp_secs,
        "hostname": info.hostname,
        "person": info.person,
    })
}

fn push_outcome_value(outcome: model::git_remote::PushOutcome) -> Value {
    match outcome {
        model::git_remote::PushOutcome::Pushed => json!({ "state": "pushed" }),
        model::git_remote::PushOutcome::NoRemote => json!({ "state": "no_remote" }),
        model::git_remote::PushOutcome::Deferred(reason) => {
            json!({ "state": "deferred", "reason": reason })
        }
    }
}

/// `ishoo_reclaim` (MCP-47) — force-take only a stale claim. Fresh claims return
/// a structured BLOCKED verdict instead of asking the agent to shell out.
fn handle_reclaim(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let repo_root = model::git_remote::find_repo_root(path).map_err(ToolError::invalid_params)?;
    let claim = match model::git_remote::inspect_claim(&repo_root, &id)
        .map_err(ToolError::invalid_params)?
    {
        Some(info) => info,
        None => {
            return Ok(json!({
                "status": "BLOCKED",
                "reclaimed": false,
                "reason": "not_claimed",
                "id": id,
                "next": format!("ishoo_start {id}")
            }));
        }
    };
    let stale =
        model::git_remote::is_claim_stale(&claim, model::git_remote::CLAIM_STALE_THRESHOLD_SECS);
    if !stale {
        return Ok(json!({
            "status": "BLOCKED",
            "reclaimed": false,
            "reason": "fresh_claim",
            "id": id,
            "stale": false,
            "claim": claim_info_value(&claim),
        }));
    }

    let push = model::git_remote::reclaim(&repo_root, &id).map_err(ToolError::invalid_params)?;
    let new_claim = model::git_remote::inspect_claim(&repo_root, &id)
        .map_err(ToolError::invalid_params)?
        .map(|info| claim_info_value(&info));
    Ok(json!({
        "status": "RECLAIMED",
        "reclaimed": true,
        "id": id,
        "stale": true,
        "previous_claim": claim_info_value(&claim),
        "claim": new_claim,
        "claim_push": push_outcome_value(push),
    }))
}

fn gc_report_value(report: &model::git_remote::GcReport) -> Value {
    json!({
        "empty": report.is_empty(),
        "removed_worktrees": report.removed_worktrees,
        "removed_claims": report.removed_claims,
        "removed_branches": report.removed_branches,
        "kept_unmerged_branches": report.kept_unmerged_branches,
    })
}

/// `ishoo_gc` (MCP-47) — sweep orphaned execution substrate through the shared
/// recovery core. This mutates claim/worktree/branch substrate, not issue records.
fn handle_gc(path: &Path, _args: &Value) -> ToolResult {
    let repo_root = model::git_remote::find_repo_root(path).map_err(ToolError::invalid_params)?;
    let report = model::git_remote::gc(&repo_root).map_err(ToolError::invalid_params)?;
    Ok(json!({
        "status": if report.is_empty() { "CLEAN" } else { "CLEANED" },
        "report": gc_report_value(&report),
    }))
}

fn store_drift_label(drift: model::git_remote::StoreDrift) -> &'static str {
    match drift {
        model::git_remote::StoreDrift::NoRef => "no_ref",
        model::git_remote::StoreDrift::InSync => "in_sync",
        model::git_remote::StoreDrift::Behind => "behind",
        model::git_remote::StoreDrift::Ahead => "ahead",
    }
}

fn doctor_report_value(report: &model::doctor::DoctorReport) -> Value {
    json!({
        "healthy": report.is_healthy(),
        "tracked_store_paths": report.tracked_store_paths,
        "dangling_plan_refs": report.dangling_plan_refs.iter().map(|d| json!({
            "plan": d.plan,
            "issue_id": d.issue_id,
        })).collect::<Vec<_>>(),
        "ahead": report.ahead.iter().map(|a| json!({
            "local_ref": a.local_ref,
            "remote_ref": a.remote_ref,
            "ahead_by": a.ahead_by,
        })).collect::<Vec<_>>(),
        "store_drift": store_drift_label(report.store_drift),
        "store_unreadable": report.store_unreadable,
    })
}

fn heal_outcome_value(outcome: &model::doctor::HealOutcome) -> Value {
    json!({
        "untracked_paths": outcome.untracked_paths,
        "resnapshotted": outcome.resnapshotted,
        "reconciled_from_ref": outcome.reconciled_from_ref,
        "publish": outcome.publish,
        "unrecoverable_danglers": outcome.unrecoverable_danglers.iter().map(|d| json!({
            "plan": d.plan,
            "issue_id": d.issue_id,
        })).collect::<Vec<_>>(),
    })
}

/// `ishoo_doctor` (MCP-47) — diagnose read-only by default; fix:true runs the
/// bounded Ishoo-owned heal and returns before/after facts.
fn handle_doctor(path: &Path, args: &Value) -> ToolResult {
    let fix = args.get("fix").and_then(Value::as_bool).unwrap_or(false);
    let report = model::doctor::diagnose(path).map_err(ToolError::invalid_params)?;
    if !fix {
        return Ok(json!({
            "status": if report.is_healthy() { "HEALTHY" } else { "FAULTS" },
            "fixed": false,
            "report": doctor_report_value(&report),
        }));
    }

    let outcome = model::doctor::heal(path, &report).map_err(ToolError::invalid_params)?;
    let after = model::doctor::diagnose(path).map_err(ToolError::invalid_params)?;
    let partial = !outcome.unrecoverable_danglers.is_empty() || !after.is_healthy();
    Ok(json!({
        "status": if partial { "PARTIAL" } else { "HEALED" },
        "fixed": true,
        "report": doctor_report_value(&report),
        "heal": heal_outcome_value(&outcome),
        "after": doctor_report_value(&after),
    }))
}

/// `ishoo_land` (MCP-06 / DEC-70) — structural gates (scope, contracts,
/// store-integrity) + git integration + land, via `model::gates`. The control
/// surface runs no build/test gate; verification lives in the issue's contract.
fn handle_land(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let verdict = model::gates::land(path, &id).map_err(ToolError::invalid_params)?;
    serde_json::to_value(&verdict)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize verdict: {e}")))
}

/// Build the full typed JSON view of one ADR. `Decision` is not `Serialize`
/// (the model stays decoupled from the wire format), so the view is assembled
/// here from its fields — the MCP-layer pattern of typed views, not raw leakage.
fn decision_view(decision: &model::Decision) -> Value {
    json!({
        "id": decision.decision_id,
        "title": decision.title,
        "status": decision.status.label(),
        "decision": decision.decision,
        "problem": decision.problem,
        "scope": decision.scope,
        "rule": decision.rule,
        "consequences": decision.consequences,
        "alternatives_rejected": decision.alternatives_rejected,
        "operational_impact": decision.operational_impact,
        "supporting_note": decision.supporting_note,
        "related_issues": decision.related_issues,
        "related_files": decision.related_files,
        "supersedes": decision.supersedes,
        "superseded_by": decision.superseded_by,
        "tags": decision.tags,
        "recorded_at": decision.recorded_at,
        "closed_at": decision.closed_at,
    })
}

/// `ishoo_decision_show` (MCP-12) — the full structured ADR by id, so an agent can
/// read a governing decision before changing code without shelling to the CLI.
/// The decision ops that only read the store (skip the post-call snapshot).
const DECISION_READ_OPS: &[&str] = &["show", "list", "adr"];

/// Whether an `ishoo_decision` call mutates the store — every op except the reads.
fn decision_op_mutates(args: &Value) -> bool {
    !op_is_read(args, DECISION_READ_OPS)
}

/// `ishoo_decision` (MCP-58 / DEC-86) — the single op-dispatched ADR tool.
fn handle_decision(path: &Path, args: &Value) -> ToolResult {
    dispatch_op(
        "ishoo_decision",
        &[
            ("show", handle_decision_show),
            ("list", handle_decision_list),
            ("adr", handle_decision_adr),
            ("new", handle_decision_new),
            ("accept", handle_decision_accept),
            ("edit", handle_decision_edit),
            ("supersede", handle_decision_supersede),
            ("delete", handle_decision_delete),
        ],
        path,
        args,
    )
}

fn handle_decision_show(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let workspace = load_workspace(path)?;
    match workspace.decisions.iter().find(|d| d.decision_id == id) {
        Some(decision) => Ok(decision_view(decision)),
        None => Err(ToolError::invalid_params(format!(
            "decision '{id}' not found"
        ))),
    }
}

/// `ishoo_decision_list` (MCP-12 / DEC-62) — every ADR as id/title/status/labels,
/// optionally filtered by an exact domain label and/or a free-text substring (over
/// id/title/body/tags), mirroring how issues are queried in `ishoo_list`.
fn handle_decision_list(path: &Path, args: &Value) -> ToolResult {
    let workspace = load_workspace(path)?;
    let label = optional_str(args, "label");
    let text = optional_str(args, "text");
    let decisions: Vec<Value> =
        model::filter_decisions(&workspace.decisions, label.as_deref(), text.as_deref())
            .iter()
            .map(|d| {
                json!({
                    "id": d.decision_id,
                    "title": d.title,
                    "status": d.status.label(),
                    "labels": d.tags,
                })
            })
            .collect();
    Ok(json!({ "decisions": decisions }))
}

/// `ishoo_inventory` (MCP-18) — the in-use labels/files/refs catalog with usage
/// counts, aggregated directly off the issue records (mirrors `ishoo labels` /
/// `files` / `refs`, which only print).
/// `ishoo_admin` (MCP-60 / DEC-86) — the single op-dispatched maintenance tool.
/// None of its ops snapshot the store ref (each operates on git refs/worktrees or
/// reads), so the tool is `mutates_never` and the dispatcher needs no per-op class.
fn handle_admin(path: &Path, args: &Value) -> ToolResult {
    dispatch_op(
        "ishoo_admin",
        &[
            ("inventory", handle_inventory),
            ("lint", handle_lint),
            ("preflight", handle_preflight),
            ("doctor", handle_doctor),
            ("reclaim", handle_reclaim),
            ("gc", handle_gc),
            ("resolve-store", handle_resolve_store),
        ],
        path,
        args,
    )
}

/// `ishoo_admin op:resolve-store` (FEAT-23/DEC-50) — resolve a same-record store
/// conflict. With no `side`, lists the conflicting record paths; with `keep-mine` /
/// `take-remote`, keeps that side of every conflicting record and converges (merge +
/// push). Manages the store ref directly (like gc/reclaim), so the tool stays
/// `mutates_never` — no post-op snapshot is needed.
fn handle_resolve_store(path: &Path, args: &Value) -> ToolResult {
    use model::git_remote::{resolve_store_conflict, store_conflict_paths, ConflictSide};
    match optional_str(args, "side") {
        None => {
            let paths = store_conflict_paths(path).map_err(ToolError::invalid_params)?;
            let count = paths.len();
            Ok(json!({ "conflicts": paths, "count": count }))
        }
        Some(side) => {
            let choice = match side.as_str() {
                "keep-mine" => ConflictSide::KeepMine,
                "take-remote" => ConflictSide::TakeRemote,
                "newest" => ConflictSide::Newest,
                other => {
                    return Err(ToolError::invalid_params(format!(
                        "unknown side '{other}' (use keep-mine, take-remote, or newest)"
                    )))
                }
            };
            let report = resolve_store_conflict(path, choice).map_err(ToolError::invalid_params)?;
            Ok(json!({
                "resolved": true,
                "side": side,
                "outcome": format!("{:?}", report.outcome),
                "conflicts": report.resolved.iter()
                    .map(|r| json!({ "path": r.path, "kept": r.kept }))
                    .collect::<Vec<_>>(),
                "backups": report.backups,
            }))
        }
    }
}

fn handle_inventory(path: &Path, _args: &Value) -> ToolResult {
    use std::collections::BTreeMap;
    let workspace = load_workspace(path)?;
    let mut labels: BTreeMap<String, usize> = BTreeMap::new();
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    let mut links: BTreeMap<String, usize> = BTreeMap::new();
    let mut depends_on: BTreeMap<String, usize> = BTreeMap::new();
    for issue in &workspace.issues {
        for l in &issue.labels {
            *labels.entry(l.to_ascii_lowercase()).or_default() += 1;
        }
        for f in &issue.files {
            *files.entry(f.clone()).or_default() += 1;
        }
        for r in &issue.links {
            *links.entry(r.clone()).or_default() += 1;
        }
        for d in &issue.depends_on {
            *depends_on.entry(d.clone()).or_default() += 1;
        }
    }
    let to_arr = |m: BTreeMap<String, usize>| -> Vec<Value> {
        m.into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect()
    };
    Ok(json!({
        "labels": to_arr(labels),
        "files": to_arr(files),
        "links": to_arr(links),
        "depends_on": to_arr(depends_on),
    }))
}

/// `ishoo_lint` (MCP-45) — run the lint checks via the shared `all_lint_findings`
/// core (DEC-49, print-free) and return structured findings. `ok` is true when no
/// findings; `strict` echoes the requested mode.
fn handle_lint(path: &Path, args: &Value) -> ToolResult {
    let strict = args.get("strict").and_then(Value::as_bool).unwrap_or(false);
    let findings = model::all_lint_findings(path, strict).map_err(ToolError::invalid_params)?;
    let findings_json: Vec<Value> = findings
        .iter()
        .map(|f| json!({ "file": f.file, "line": f.line, "message": f.message }))
        .collect();
    Ok(json!({
        "ok": findings.is_empty(),
        "strict": strict,
        "count": findings.len(),
        "findings": findings_json,
    }))
}

/// `ishoo_preflight` (MCP-46) — the typed land-readiness report via the shared
/// `model::preflight::preflight_report` core (DEC-49, print-free). Serializes the
/// typed report directly; an unknown issue id is an invalid-params error.
fn handle_preflight(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let report =
        model::preflight::preflight_report(path, &id).map_err(ToolError::invalid_params)?;
    serde_json::to_value(&report)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize report: {e}")))
}

/// The comment ops that only read the store (skip the post-call snapshot).
const COMMENT_READ_OPS: &[&str] = &["list"];

/// Whether an `ishoo_comment` call mutates the store — every op except `list`.
fn comment_op_mutates(args: &Value) -> bool {
    !op_is_read(args, COMMENT_READ_OPS)
}

/// `ishoo_comment` (MCP-59 / DEC-86) — the single op-dispatched comment tool.
fn handle_comment(path: &Path, args: &Value) -> ToolResult {
    dispatch_op(
        "ishoo_comment",
        &[
            ("list", handle_comment_list),
            ("add", handle_comment_add),
            ("edit", handle_comment_edit),
            ("remove", handle_comment_remove),
        ],
        path,
        args,
    )
}

/// `ishoo_comment_add` (MCP-17) — append a comment via the `add_comment_by_id`
/// core (print-free), defaulting the author to the issue owner, then persist.
fn handle_comment_add(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let text = required_str(args, "text")?;
    let mut workspace = load_workspace(path)?;
    // Default the author to the issue's owner (mirrors the CLI).
    let author = optional_str(args, "author").unwrap_or_else(|| {
        workspace
            .issues
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.owner_id.clone())
            .unwrap_or_default()
    });
    let comment = model::add_comment_by_id(&mut workspace.issues, &id, &author, &text)
        .map_err(ToolError::invalid_params)?;
    workspace.save().map_err(ToolError::invalid_params)?;
    Ok(json!({ "author": comment.author, "at": comment.at, "text": comment.text }))
}

/// `ishoo_comment_list` (MCP-17) — an issue's comments in order, read directly off
/// the issue record.
fn handle_comment_list(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let workspace = load_workspace(path)?;
    let issue = workspace
        .issues
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(|| ToolError::invalid_params(format!("issue '{id}' not found")))?;
    serde_json::to_value(json!({ "id": id, "comments": &issue.comments }))
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize comments: {e}")))
}

/// `ishoo_comment_edit` (MCP-53) — replace one comment's text via the
/// `edit_comment_by_id` core (print-free), then persist. Index is 0-based,
/// oldest-first, matching ishoo_comment_list order.
fn handle_comment_edit(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let index = required_index(args, "index")?;
    let text = required_str(args, "text")?;
    let mut workspace = load_workspace(path)?;
    let comment = model::edit_comment_by_id(&mut workspace.issues, &id, index, &text)
        .map_err(ToolError::invalid_params)?;
    workspace.save().map_err(ToolError::invalid_params)?;
    Ok(json!({ "author": comment.author, "at": comment.at, "text": comment.text }))
}

/// `ishoo_comment_remove` (MCP-53) — remove one comment via the
/// `delete_comment_by_id` core (print-free), then persist. Returns the removed
/// comment. Index is 0-based, oldest-first.
fn handle_comment_remove(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let index = required_index(args, "index")?;
    let mut workspace = load_workspace(path)?;
    let comment = model::delete_comment_by_id(&mut workspace.issues, &id, index)
        .map_err(ToolError::invalid_params)?;
    workspace.save().map_err(ToolError::invalid_params)?;
    Ok(json!({ "author": comment.author, "at": comment.at, "text": comment.text }))
}

/// `ishoo_decision_adr` (MCP-53) — render one ADR as canonical ADR markdown via the
/// shared `adr_markdown` core (print-free). Read-only.
fn handle_decision_adr(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let workspace = load_workspace(path)?;
    match workspace.decisions.iter().find(|d| d.decision_id == id) {
        Some(decision) => Ok(json!({ "id": id, "markdown": model::adr_markdown(decision) })),
        None => Err(ToolError::invalid_params(format!(
            "decision '{id}' not found"
        ))),
    }
}

/// `ishoo_rename_id` (MCP-30) — rename an issue's id via the `rename_issue_id` core
/// (which updates links/depends_on across issues), then rewrite plan entries
/// referencing the old id in every plan (default + named). The proper way to
/// re-categorize, since the id is the primary key, not an editable field.
fn handle_rename_id(path: &Path, args: &Value) -> ToolResult {
    let old_id = required_str(args, "id")?;
    let new_id = required_str(args, "new_id")?;
    let mut workspace = load_workspace(path)?;
    model::rename_issue_id(&mut workspace.issues, &old_id, &new_id)
        .map_err(ToolError::invalid_params)?;
    workspace.save().map_err(ToolError::invalid_params)?;

    // Rewrite plan entries referencing the old id across all plans (more thorough
    // than the CLI, which only touches the legacy single plan).
    let mut all = model::AllPlans::load(path);
    let mut changed = false;
    let mut plans: Vec<&mut model::Plan> = vec![&mut all.default_plan];
    plans.extend(all.named.iter_mut().map(|np| &mut np.plan));
    for plan in plans {
        for entry in plan.entries.iter_mut() {
            if entry.project_key == "local" && entry.issue_id == old_id {
                entry.issue_id.clone_from(&new_id);
                changed = true;
            }
        }
    }
    if changed {
        all.save(path).map_err(ToolError::invalid_params)?;
    }
    Ok(json!({ "old_id": old_id, "new_id": new_id }))
}

/// `ishoo_delete` (MCP-15) — permanently delete an issue and prune dangling plan
/// entries, mirroring the CLI's `delete --force` sequence (delete_issue persists,
/// then prune_issue_everywhere). No confirmation prompt: the tool call is intent.
fn handle_delete(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let mut workspace = load_workspace(path)?;
    // Guard landed history: a DONE issue is only deleted with explicit force, since
    // the tool has no interactive confirmation the human CLI gets.
    let is_done = workspace
        .issues
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.status == Status::Done)
        .unwrap_or(false);
    if is_done && !force {
        return Err(ToolError::invalid_params(format!(
            "{id} is DONE; refusing to delete landed work without force:true"
        )));
    }
    let deleted = workspace
        .delete_issue(&id)
        .map_err(ToolError::invalid_params)?;
    let pruned = model::AllPlans::prune_issue_everywhere(path, "local", &id)
        .map_err(ToolError::invalid_params)?;
    Ok(json!({
        "id": deleted.id,
        "title": deleted.title,
        "deleted": true,
        "pruned_plan_entries": pruned,
    }))
}

/// The presence-aware array reader (MCP-26): a present array (even empty) returns
/// `Some` so the field is set or cleared; an absent key returns `None`, leaving it
/// unchanged; non-string items are ignored. Callers adapt the `Vec` to the core's
/// type — `.unwrap_or_default()` for a plain `Vec`, `.map(|v| v.join(","))` for a
/// CSV core. Scalars use [`present_str`]; contract-required arrays use
/// [`required_str_array`].
fn opt_str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// A scalar string argument by presence, not emptiness (MCP-24): a present key
/// returns `Some` even when empty (so `""` clears the field, like a present empty
/// array clears a list field); an absent key returns `None`, leaving it unchanged.
/// The scalar counterpart to [`opt_str_array`].
fn present_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `ishoo_edit` (MCP-13/MCP-24) — edit an existing issue's non-resolution fields
/// via the shared `cli_edit` core (which validates refs and persists). Only
/// provided keys change; a present empty value (`""` for a scalar, `[]` for a
/// list) clears that field. `category` is not editable here — it is the id prefix,
/// changed with `ishoo_rename_id`.
fn handle_edit(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let edit = EditArgs {
        title: present_str(args, "title"),
        description: present_str(args, "description"),
        owner: present_str(args, "owner"),
        labels: opt_str_array(args, "labels").map(|v| v.join(",")),
        files: opt_str_array(args, "files").map(|v| v.join(",")),
        links: opt_str_array(args, "links").map(|v| v.join(",")),
        depends_on: opt_str_array(args, "depends_on").map(|v| v.join(",")),
        decisions: opt_str_array(args, "decisions").map(|v| v.join(",")),
        ..EditArgs::default()
    };
    if !edit.has_changes() {
        return Err(ToolError::invalid_params(
            "ishoo_edit requires at least one field to change besides id".to_string(),
        ));
    }
    let mut workspace = load_workspace(path)?;
    let outcome = model::cli_edit(&mut workspace, &id, &edit).map_err(ToolError::invalid_params)?;
    serde_json::to_value(&outcome)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize outcome: {e}")))
}

/// `ishoo_decision_new` (MCP-19) — author an ADR via the `build_decision` core, then
/// persist it. Created PROPOSED; the id is allocated by the core.
fn handle_decision_new(path: &Path, args: &Value) -> ToolResult {
    let input = model::NewDecisionInput {
        title: required_str(args, "title")?,
        decision: required_str(args, "decision")?,
        problem: required_str(args, "problem")?,
        scope: optional_str(args, "scope").unwrap_or_default(),
        rule: optional_str(args, "rule").unwrap_or_default(),
        consequences: optional_str(args, "consequences").unwrap_or_default(),
        alternatives_rejected: optional_str(args, "alternatives_rejected").unwrap_or_default(),
        operational_impact: optional_str(args, "operational_impact").unwrap_or_default(),
        supporting_note: optional_str(args, "supporting_note").unwrap_or_default(),
        related_issues: opt_str_array(args, "related_issues").unwrap_or_default(),
        related_files: opt_str_array(args, "related_files").unwrap_or_default(),
        tags: opt_str_array(args, "tags").unwrap_or_default(),
        status: model::DecisionStatus::default(),
    };
    let workspace = load_workspace(path)?;
    // Same rigor as issue authoring: related_issues must exist (ADR-02 parity).
    model::validate_reference_values(&workspace, "", &input.related_issues, "related_issues")
        .map_err(ToolError::invalid_params)?;
    // DEC-62: report any non-canonical tags the core stripped (warn-and-strip parity).
    let (_, stripped_labels) = model::project_store::filter_decision_labels(input.tags.clone())
        .map_err(ToolError::invalid_params)?;
    let decision = model::build_decision(&workspace, &input).map_err(ToolError::invalid_params)?;
    let id = decision.decision_id.clone();
    model::Workspace::persist_decisions(path, vec![decision]).map_err(ToolError::invalid_params)?;
    Ok(json!({ "id": id, "status": "PROPOSED", "stripped_labels": stripped_labels }))
}

/// `ishoo_decision_accept` (MCP-19) — accept an ADR via `accept_decision`, then
/// persist the changed record.
fn handle_decision_accept(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let mut workspace = load_workspace(path)?;
    model::accept_decision(&mut workspace.decisions, &id).map_err(ToolError::invalid_params)?;
    let changed = workspace
        .decisions
        .iter()
        .find(|d| d.decision_id == id)
        .cloned()
        .ok_or_else(|| ToolError::invalid_params(format!("decision '{id}' not found")))?;
    model::Workspace::persist_decisions(path, vec![changed]).map_err(ToolError::invalid_params)?;
    Ok(json!({ "id": id, "status": "ACCEPTED" }))
}

/// `ishoo_decision_edit` (MCP-19) — amend an ADR's sections via `patch_decision`,
/// then persist. Only provided fields change.
fn handle_decision_edit(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    // No-op guard (parity with ishoo_edit): an id-only call changes nothing, so
    // reject it rather than push an empty store commit. `confirm` is a control flag,
    // not a content field, so it does not count as a change.
    let has_change = args
        .as_object()
        .map(|o| o.keys().any(|k| k != "id" && k != "confirm"))
        .unwrap_or(false);
    if !has_change {
        return Err(ToolError::invalid_params(
            "ishoo_decision_edit requires at least one field to change besides id".to_string(),
        ));
    }
    // DEC-12 confirm-gate: edit mutates an ADR IN PLACE, which is only for typos,
    // wording, or truthing a stale headline — never a change of meaning. Without
    // confirm:true, return a NON-mutating handshake pointing at supersede, then let
    // the caller re-affirm. This makes "supersede, don't edit" a gate, not a hope.
    let confirmed = args
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !confirmed {
        return Ok(json!({
            "status": "confirmation_required",
            "id": id,
            "message": "decision_edit changes this ADR IN PLACE — intended only for typos, wording, \
                        or truthing a stale headline, NOT for changing what the decision means. Per \
                        DEC-12, any change of meaning must be a new ADR that supersedes this one so \
                        the reasoning chain survives. If this is genuinely a clarification, re-call \
                        ishoo_decision_edit with confirm:true.",
            "to_change_meaning": "ishoo_decision_new + ishoo_decision_supersede"
        }));
    }
    let patch = model::DecisionPatch {
        title: optional_str(args, "title"),
        decision: optional_str(args, "decision"),
        problem: optional_str(args, "problem"),
        scope: optional_str(args, "scope"),
        rule: optional_str(args, "rule"),
        consequences: optional_str(args, "consequences"),
        alternatives_rejected: optional_str(args, "alternatives_rejected"),
        operational_impact: optional_str(args, "operational_impact"),
        supporting_note: optional_str(args, "supporting_note"),
        related_issues: opt_str_array(args, "related_issues"),
        related_files: opt_str_array(args, "related_files"),
        tags: opt_str_array(args, "tags"),
    };
    let mut workspace = load_workspace(path)?;
    if let Some(refs) = &patch.related_issues {
        model::validate_reference_values(&workspace, "", refs, "related_issues")
            .map_err(ToolError::invalid_params)?;
    }
    // DEC-62: report any non-canonical tags the core stripped on this edit.
    let stripped_labels = match &patch.tags {
        Some(tags) => {
            model::project_store::filter_decision_labels(tags.clone())
                .map_err(ToolError::invalid_params)?
                .1
        }
        None => Vec::new(),
    };
    model::patch_decision(&mut workspace.decisions, &id, &patch)
        .map_err(ToolError::invalid_params)?;
    let changed = workspace
        .decisions
        .iter()
        .find(|d| d.decision_id == id)
        .cloned()
        .ok_or_else(|| ToolError::invalid_params(format!("decision '{id}' not found")))?;
    model::Workspace::persist_decisions(path, vec![changed]).map_err(ToolError::invalid_params)?;
    Ok(json!({ "id": id, "stripped_labels": stripped_labels }))
}

/// `ishoo_decision_supersede` (MCP-49) — mark OLD superseded by NEW and link the
/// pair via `supersede_decision`, then persist both changed records. This is the
/// DEC-12 way to change a decision: the replacement is authored separately, and
/// both records stay so the reasoning chain is preserved.
fn handle_decision_supersede(path: &Path, args: &Value) -> ToolResult {
    let superseded_id = required_str(args, "superseded_id")?;
    let new_id = required_str(args, "new_id")?;
    // DECI-01 / DEC-12: a supersession must record why the old decision is replaced.
    let reason = required_str(args, "reason")?;
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ToolError::invalid_params(
            "supersession requires a non-empty reason (why the old decision is replaced) — DECI-01"
                .to_string(),
        ));
    }
    let mut workspace = load_workspace(path)?;
    let affected_live_issues: Vec<String> = workspace
        .issues
        .iter()
        .filter(|issue| issue.carries_live_work())
        .filter(|issue| issue.decision_refs.iter().any(|r| r == &superseded_id))
        .map(|issue| issue.id.clone())
        .collect();
    model::supersede_decision(&mut workspace.decisions, &superseded_id, &new_id)
        .map_err(ToolError::invalid_params)?;
    for decision in workspace.decisions.iter_mut() {
        decision.apply_supersede_reason(&superseded_id, &new_id, reason);
    }
    let changed: Vec<_> = workspace
        .decisions
        .iter()
        .filter(|d| d.decision_id == superseded_id || d.decision_id == new_id)
        .cloned()
        .collect();
    model::Workspace::persist_decisions(path, changed).map_err(ToolError::invalid_params)?;
    Ok(json!({
        "superseded": superseded_id,
        "superseded_by": new_id,
        "reason": reason,
        "affected_live_issues": affected_live_issues,
        "affected_live_issue_count": affected_live_issues.len(),
        "status": "SUPERSEDED"
    }))
}

/// `ishoo_decision_delete` (MCP-49) — permanently remove an ADR. DISCOURAGED per
/// DEC-12 (supersede preserves reasoning; delete loses it), so it requires an
/// explicit confirm:true and otherwise returns a non-mutating handshake.
fn handle_decision_delete(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let confirmed = args
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !confirmed {
        return Ok(json!({
            "status": "confirmation_required",
            "id": id,
            "message": "Deleting an ADR is the DISCOURAGED path (DEC-12): going forward, supersede \
                        instead so the reasoning chain is preserved. Delete only genuine noise with \
                        no live architectural constraint. Re-call ishoo_decision_delete with \
                        confirm:true if you are sure.",
            "prefer": "ishoo_decision_supersede"
        }));
    }
    let mut workspace = load_workspace(path)?;
    let deleted = workspace
        .delete_decision(&id)
        .map_err(ToolError::invalid_params)?;
    Ok(json!({
        "id": deleted.decision_id,
        "title": deleted.title,
        "deleted": true
    }))
}

/// `ishoo_new` (MCP-04) — assemble the Scope Contract from required typed fields
/// and create the issue through the `create_issue` core fn (DEC-49). Because the
/// fields arrive as JSON, multi-line text with backticks / `$()` / newlines
/// survives byte-exact (no shell parsing).
fn handle_new(path: &Path, args: &Value) -> ToolResult {
    let title = required_str(args, "title")?;
    let plan = required_str(args, "plan")?;
    let concrete_change = required_str(args, "concrete_change")?;
    let main_surface = required_str(args, "main_surface")?;
    let proof_of_done = required_str(args, "proof_of_done")?;
    let out_of_scope = required_str(args, "out_of_scope")?;
    let category = optional_str(args, "category").unwrap_or_else(|| "iss".to_string());
    let decisions = normalize_none_sentinel(required_str_array(args, "decisions")?);
    let depends_on = normalize_none_sentinel(required_str_array(args, "depends_on")?);
    let files = opt_str_array(args, "files").unwrap_or_default();

    // MCP-50 / DEC-25: validate labels at creation with the same registry the CLI/edit
    // use — strip unknown labels (reported in the result), hard-error only on > max.
    let label_registry = crate::model::project_store::ProjectStore::load(path)
        .map(|s| {
            if s.labels.entries.is_empty() {
                crate::model::project_store::LabelRegistry::default()
            } else {
                s.labels
            }
        })
        .unwrap_or_default();
    let (labels, stripped_labels) = label_registry
        .filter_new_labels(opt_str_array(args, "labels").unwrap_or_default())
        .map_err(ToolError::invalid_params)?;
    let has_urgency_tier_label = model::project_store::has_urgency_tier(&labels);
    let mutation_id = optional_str(args, "mutation_id").unwrap_or_else(|| {
        generated_new_mutation_id(&NewMutationSeed {
            title: &title,
            category: &category,
            plan: &plan,
            concrete_change: &concrete_change,
            main_surface: &main_surface,
            proof_of_done: &proof_of_done,
            out_of_scope: &out_of_scope,
            decisions: &decisions,
            depends_on: &depends_on,
        })
    });

    let description = format!(
        "**Concrete change:** {concrete_change}\n\n\
         **Main surface:** {main_surface}\n\n\
         **Proof of done:** {proof_of_done}\n\n\
         **Out of scope:** {out_of_scope}"
    );
    // Defensive: the assembled contract must validate (it will, by construction).
    let scope = model::validate_scope_contract(&description);
    if !scope.complete {
        return Err(ToolError::invalid_params(format!(
            "assembled Scope Contract is incomplete (missing: {})",
            scope.missing.join(", ")
        )));
    }

    let mut workspace = load_workspace(path)?;
    let plans = model::AllPlans::load(path);
    if let Some(outcome) = completed_new_mutation(&workspace, &mutation_id) {
        ensure_completed_new_plan_membership(path, &plans, &plan, &outcome.id)?;
        return new_outcome_value(&outcome, &mutation_id, "already_created", &[], None);
    }

    // Resolve the plan home first (fail fast on a bad plan), before any write.
    let plan_target =
        model::resolve_new_plan_target(&plans, Some(&plan)).map_err(ToolError::invalid_params)?;

    model::validate_decision_refs(&workspace, &decisions).map_err(ToolError::invalid_params)?;
    model::validate_reference_values(&workspace, "", &depends_on, "depends_on")
        .map_err(ToolError::invalid_params)?;

    let outcome = model::create_issue(
        &mut workspace,
        &NewIssueInput {
            title,
            category,
            status: Status::Backlog,
            labels,
            description,
            source_file: model::default_document_name().to_string(),
        },
        decisions,
        depends_on,
    )
    .map_err(ToolError::invalid_params)?;
    if let Some(issue) = workspace
        .issues
        .iter_mut()
        .find(|issue| issue.id == outcome.id)
    {
        // MCP-50: files are not part of NewIssueInput (build_issue starts empty), so
        // apply them here alongside the mutation-id stamp, mirroring ishoo_edit.
        if !files.is_empty() {
            issue.files = files.clone();
        }
        issue
            .extra_fields
            .push((MCP_MUTATION_ID_FIELD.to_string(), mutation_id.clone()));
    }
    workspace
        .save()
        .map_err(|e| ToolError::invalid_params(format!("save failed: {e}")))?;

    // Enroll into the declared plan (DEC-46/55), mirroring the CLI path.
    let mut plans = model::AllPlans::load(path);
    model::enroll_issue_in_declared_plan(&mut plans, plan_target, &outcome.id)
        .map_err(ToolError::invalid_params)?;
    plans
        .save(path)
        .map_err(|e| ToolError::invalid_params(format!("plan enrollment failed: {e}")))?;
    maybe_delay_new_response_for_tests();

    new_outcome_value(
        &outcome,
        &mutation_id,
        "created",
        &stripped_labels,
        Some(has_urgency_tier_label),
    )
}

/// `ishoo_decompose` (FEAT-15) — split a parent into child issues, recording the
/// typed parent↔child lineage via the `model::decompose` core fn (DEC-49/DEC-61).
/// Each child arrives as a JSON object so its multi-line Scope Contract survives
/// byte-exact (no shell parsing). Mirrors `handle_new`'s save ordering: the
/// workspace is saved first, then children are enrolled into the inherited plan
/// against a freshly loaded plan snapshot (STOR-14 generation guard).
fn handle_decompose(path: &Path, args: &Value) -> ToolResult {
    let parent = required_str(args, "parent")?;
    let children_val = args
        .get("children")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::invalid_params("missing required field 'children' (array)"))?;
    if children_val.is_empty() {
        return Err(ToolError::invalid_params(
            "'children' must list at least one child issue",
        ));
    }

    let mut workspace = load_workspace(path)?;
    let mut specs = Vec::with_capacity(children_val.len());
    for child in children_val {
        let title = required_str(child, "title")?;
        let category = optional_str(child, "category").unwrap_or_else(|| "iss".to_string());
        let concrete_change = required_str(child, "concrete_change")?;
        let main_surface = required_str(child, "main_surface")?;
        let proof_of_done = required_str(child, "proof_of_done")?;
        let out_of_scope = required_str(child, "out_of_scope")?;
        let decisions = normalize_none_sentinel(required_str_array(child, "decisions")?);
        let depends_on = normalize_none_sentinel(required_str_array(child, "depends_on")?);

        let description = format!(
            "**Concrete change:** {concrete_change}\n\n\
             **Main surface:** {main_surface}\n\n\
             **Proof of done:** {proof_of_done}\n\n\
             **Out of scope:** {out_of_scope}"
        );
        let scope = model::validate_scope_contract(&description);
        if !scope.complete {
            return Err(ToolError::invalid_params(format!(
                "child '{title}' Scope Contract is incomplete (missing: {})",
                scope.missing.join(", ")
            )));
        }
        model::validate_decision_refs(&workspace, &decisions).map_err(ToolError::invalid_params)?;
        model::validate_reference_values(&workspace, "", &depends_on, "depends_on")
            .map_err(ToolError::invalid_params)?;

        specs.push(model::ChildSpec {
            input: NewIssueInput {
                title,
                category,
                status: Status::Backlog,
                labels: Vec::new(),
                description,
                source_file: model::default_document_name().to_string(),
            },
            decision_refs: decisions,
            depends_on,
        });
    }

    let plans = model::AllPlans::load(path);
    let outcome: model::DecomposeOutcome = model::decompose(&mut workspace, &plans, &parent, specs)
        .map_err(ToolError::invalid_params)?;
    workspace
        .save()
        .map_err(|e| ToolError::invalid_params(format!("save failed: {e}")))?;

    // Enroll the children into the parent's inherited plan, if any, against a fresh
    // snapshot taken after the workspace save (STOR-14). When the parent is only in
    // the default Backlog plan, the children stay there too (DEC-42) — no-op.
    if let Some(plan_id) = &outcome.inherited_plan_id {
        let mut plans = model::AllPlans::load(path);
        for child in &outcome.children {
            model::enroll_issue_in_declared_plan(
                &mut plans,
                model::NewPlanTarget::ExistingNamed(plan_id.clone()),
                &child.id,
            )
            .map_err(ToolError::invalid_params)?;
        }
        plans
            .save(path)
            .map_err(|e| ToolError::invalid_params(format!("plan enrollment failed: {e}")))?;
    }

    serde_json::to_value(&outcome)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize outcome: {e}")))
}

fn completed_new_mutation(
    workspace: &Workspace,
    mutation_id: &str,
) -> Option<model::CreateOutcome> {
    workspace
        .issues
        .iter()
        .find(|issue| {
            issue
                .extra_fields
                .iter()
                .any(|(key, value)| key == MCP_MUTATION_ID_FIELD && value == mutation_id)
        })
        .map(|issue| model::CreateOutcome {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status,
            // Idempotent retry recovery: the advisory was surfaced on the original
            // create; don't recompute it for a duplicate-request replay.
            similar_retired: Vec::new(),
        })
}

fn ensure_completed_new_plan_membership(
    path: &Path,
    plans: &model::AllPlans,
    plan: &str,
    issue_id: &str,
) -> Result<(), ToolError> {
    let mut plans = plans.clone();
    let target = resolve_retry_new_plan_target(&plans, plan).map_err(ToolError::invalid_params)?;
    model::enroll_issue_in_declared_plan(&mut plans, target, issue_id)
        .map_err(ToolError::invalid_params)?;
    plans
        .save(path)
        .map_err(|e| ToolError::invalid_params(format!("plan enrollment failed: {e}")))?;
    Ok(())
}

fn resolve_retry_new_plan_target(
    plans: &model::AllPlans,
    plan: &str,
) -> Result<model::NewPlanTarget, String> {
    let raw = plan.trim();
    if raw.len() >= 4 && raw[..4].eq_ignore_ascii_case("new:") {
        let name = trim_retry_plan_name(raw[4..].trim());
        if plans
            .named
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name))
        {
            return Ok(model::NewPlanTarget::ExistingNamed(name.to_string()));
        }
    }
    model::resolve_new_plan_target(plans, Some(plan))
}

fn trim_retry_plan_name(raw: &str) -> &str {
    raw.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw)
}

fn new_outcome_value(
    outcome: &model::CreateOutcome,
    mutation_id: &str,
    status: &str,
    stripped_labels: &[String],
    has_urgency_tier_label: Option<bool>,
) -> ToolResult {
    let mut value = serde_json::to_value(outcome)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize outcome: {e}")))?;
    if let Value::Object(map) = &mut value {
        map.insert(
            "mutation".to_string(),
            json!({ "id": mutation_id, "status": status }),
        );
        // MCP-50 / DEC-25: report any unknown labels that were stripped at creation.
        if !stripped_labels.is_empty() {
            map.insert("stripped_labels".to_string(), json!(stripped_labels));
        }
        if has_urgency_tier_label == Some(false) {
            map.insert(
                "urgency_assessment".to_string(),
                json!({
                    "status": "not_set",
                    "guidance": "Assess urgency at creation: urgent interrupts across plans; important is high-value active-plan work; mid is normal work; later is deferred; shelved is retained knowledge excluded from normal next work. Choose one tier label, or intentionally leave the issue unlabeled (DEC-90)."
                }),
            );
        }
    }
    Ok(value)
}

struct NewMutationSeed<'a> {
    title: &'a str,
    category: &'a str,
    plan: &'a str,
    concrete_change: &'a str,
    main_surface: &'a str,
    proof_of_done: &'a str,
    out_of_scope: &'a str,
    decisions: &'a [String],
    depends_on: &'a [String],
}

fn generated_new_mutation_id(seed: &NewMutationSeed<'_>) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in [
        "ishoo_new/v1",
        seed.title,
        seed.category,
        seed.plan,
        seed.concrete_change,
        seed.main_surface,
        seed.proof_of_done,
        seed.out_of_scope,
    ] {
        hash_part(&mut hash, part);
    }
    for decision in seed.decisions {
        hash_part(&mut hash, decision);
    }
    hash_part(&mut hash, "\u{1f}");
    for blocker in seed.depends_on {
        hash_part(&mut hash, blocker);
    }
    format!("ishoo_new:{hash:016x}")
}

fn hash_part(hash: &mut u64, part: &str) {
    for byte in part.as_bytes() {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
    *hash ^= 0xff;
    *hash = hash.wrapping_mul(0x100000001b3);
}

#[cfg(test)]
fn maybe_delay_new_response_for_tests() {
    if let Ok(raw) = std::env::var("ISHOO_MCP_NEW_RESPONSE_DELAY_MS") {
        if let Ok(millis) = raw.parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(millis));
        }
    }
}

#[cfg(not(test))]
fn maybe_delay_new_response_for_tests() {}

/// `ishoo_resolve` (MCP-04) — assemble the Resolution Contract from required
/// typed fields and write it through the `cli_edit` core fn (DEC-49).
fn handle_resolve(path: &Path, args: &Value) -> ToolResult {
    let id = required_str(args, "id")?;
    let what_changed = required_str(args, "what_changed")?;
    let why = required_str(args, "why")?;
    let verification = required_str(args, "verification")?;
    let handoff = required_str(args, "handoff")?;

    let resolution = format!(
        "**What changed:** {what_changed}\n\n\
         **Why:** {why}\n\n\
         **Verification:** {verification}\n\n\
         **Handoff:** {handoff}"
    );
    let contract = model::validate_resolution_contract(&resolution);
    if !contract.complete {
        return Err(ToolError::invalid_params(format!(
            "assembled Resolution Contract is incomplete (missing: {})",
            contract.missing.join(", ")
        )));
    }

    let mut workspace = load_workspace(path)?;
    let outcome = model::cli_edit(
        &mut workspace,
        &id,
        &EditArgs {
            resolution: Some(resolution),
            ..EditArgs::default()
        },
    )
    .map_err(ToolError::invalid_params)?;

    serde_json::to_value(&outcome)
        .map_err(|e| ToolError::invalid_params(format!("Failed to serialize outcome: {e}")))
}

// --- argument helpers -----------------------------------------------------

/// Load the workspace, mapping a load failure to a tool error (never exits the
/// server process, unlike the CLI's `load_workspace`).
///
/// STOR-25: reads answer from the resident store owner's in-memory workspace when
/// the on-disk store is unchanged, so a run of MCP tool calls re-parses the shard
/// store once, not per call (DEC-84 rule 2). `Workspace::load` is still the source
/// on a cache miss, so behavior is identical to a fresh load — just not repeated.
fn load_workspace(path: &Path) -> Result<Workspace, ToolError> {
    crate::model::store_owner::load_cached(path)
        .map_err(|e| ToolError::invalid_params(format!("could not load workspace: {e}")))
}

/// A required, non-empty string argument.
fn required_str(args: &Value, key: &str) -> Result<String, ToolError> {
    match args.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        Some(_) => Err(ToolError::invalid_params(format!(
            "field '{key}' must not be empty"
        ))),
        None => Err(ToolError::invalid_params(format!(
            "missing required field '{key}' (received keys: {})",
            received_keys(args)
        ))),
    }
}

/// Render the object keys actually present in `args`, sorted for determinism.
///
/// A bare "missing required field" tells the caller *which* field is absent but
/// not what the server *did* receive — so a field dropped during client-side
/// serialization or transport reads identically to one the caller forgot. Listing
/// the received keys makes the difference self-evident: a key the caller knows it
/// sent showing up absent here points at the wire, not the call site. Returns
/// "none" when `args` is not an object or carries no keys.
fn received_keys(args: &Value) -> String {
    match args.as_object() {
        Some(map) if !map.is_empty() => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            keys.join(", ")
        }
        _ => "none".to_string(),
    }
}

/// An optional string argument (absent or empty → None).
fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ToolError::invalid_params(format!("'{key}' must be a positive integer"))
        }),
    }
}

fn reject_unknown_keys(args: &Value, allowed: &[&str]) -> Result<(), ToolError> {
    let Some(map) = args.as_object() else {
        return Err(ToolError::invalid_params("arguments must be an object"));
    };
    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    unknown.sort_unstable();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(ToolError::invalid_params(format!(
            "unsupported field(s): {}",
            unknown.join(", ")
        )))
    }
}

/// A required non-negative integer argument (e.g. a 0-based comment index).
fn required_index(args: &Value, key: &str) -> Result<usize, ToolError> {
    match args.get(key) {
        None => Err(ToolError::invalid_params(format!(
            "missing required field '{key}'"
        ))),
        Some(value) => value.as_u64().map(|n| n as usize).ok_or_else(|| {
            ToolError::invalid_params(format!("'{key}' must be a non-negative integer"))
        }),
    }
}

/// A required array-of-strings argument. An empty array is valid (it asserts the
/// "none" case explicitly); a missing field is an error.
fn required_str_array(args: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match args.get(key) {
        None => Err(ToolError::invalid_params(format!(
            "missing required field '{key}' (use [] to assert none)"
        ))),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::invalid_params(format!("'{key}' must be strings")))
            })
            .collect(),
        Some(_) => Err(ToolError::invalid_params(format!(
            "'{key}' must be an array"
        ))),
    }
}

/// Normalize the brief-documented `none` sentinel for a required ref array. A lone
/// `["none"]` (case-insensitive) means "explicitly no refs" — the array form of
/// the CLI's `--depends-on none` / `--decisions none`. Mirroring that here keeps
/// the MCP surface consistent with the CLI and the agent brief, so a client that
/// passes the documented sentinel gets an empty list instead of a confusing
/// "Invalid ref 'none'". Any other content (incl. "none" mixed with real ids) is
/// returned unchanged, matching the CLI, where such a mix is a real ref error.
fn normalize_none_sentinel(refs: Vec<String>) -> Vec<String> {
    if refs.len() == 1 && refs[0].eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_full_coverage() {
        assert!(
            coverage_gaps().is_empty(),
            "registry is missing tools for: {:?}",
            coverage_gaps()
        );
    }

    #[test]
    fn every_tool_maps_to_an_in_scope_capability() {
        let in_scope: HashSet<&str> = IN_SCOPE_CAPABILITIES.iter().copied().collect();
        for tool in registry() {
            assert!(
                in_scope.contains(tool.capability),
                "tool {} maps to unknown capability {}",
                tool.name,
                tool.capability
            );
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let mut seen = HashSet::new();
        for tool in registry() {
            assert!(seen.insert(tool.name), "duplicate tool name {}", tool.name);
        }
    }

    #[test]
    fn removing_an_entry_reports_the_missing_capability() {
        // Simulate dropping ishoo_show: its capability must surface as a gap.
        let covered: HashSet<&str> = registry()
            .iter()
            .filter(|tool| tool.name != "ishoo_show")
            .map(|tool| tool.capability)
            .collect();
        let gaps = gaps_against(&covered);
        assert_eq!(gaps, vec!["show"]);
    }
}
