use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[path = "main_cli_query.rs"]
mod main_cli_query;
#[path = "main_help.rs"]
mod main_help;

pub use main_cli_query::{build_bulk_query, build_issue_query, BulkQueryArgs, IssueQueryArgs};
use main_help::{
    APPLY_HELP, BATCH_HELP, DECISION_DRAFT_ISSUES_HELP, DECISION_HELP, DECLINE_HELP, DONE_HELP,
    EDIT_HELP, FILES_HELP, LABELS_HELP, LAND_HELP, LINK_HELP, LINT_HELP, LIST_HELP, MOVE_HELP,
    NEW_HELP, PLAN_HELP, REFS_HELP, RELINK_HELP, RENAME_ID_HELP, ROOT_HELP, SET_HELP, SHELVE_HELP,
    SHOW_HELP, SUPERSEDE_HELP,
};

#[derive(Parser)]
#[command(
    name = "ishoo",
    about = "Issue control plane for AI agents and humans.",
    long_about = ROOT_HELP,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[arg(short, long, default_value = ".", global = true)]
    pub path: PathBuf,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RefKind {
    All,
    Links,
    DependsOn,
}

#[derive(Args)]
pub struct HelpArgs {
    #[arg(long, conflicts_with = "command")]
    pub all: bool,
    pub command: Option<String>,
}

#[derive(Args)]
pub struct PlanArgs {
    #[command(subcommand)]
    pub command: PlanCommand,
}

#[derive(Args)]
pub struct DecisionArgs {
    #[command(subcommand)]
    pub command: DecisionCommand,
}

#[derive(Subcommand)]
pub enum DecisionCommand {
    #[command(about = "Create a new decision record")]
    New {
        title: String,
        #[arg(long)]
        decision: Option<String>,
        #[arg(
            long = "decision-file",
            help = "Read Decision section text from a file"
        )]
        decision_file: Option<PathBuf>,
        #[arg(long)]
        problem: Option<String>,
        #[arg(long = "problem-file", help = "Read Problem section text from a file")]
        problem_file: Option<PathBuf>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long = "scope-file", help = "Read Scope section text from a file")]
        scope_file: Option<PathBuf>,
        #[arg(long)]
        rule: Option<String>,
        #[arg(long = "rule-file", help = "Read Rule section text from a file")]
        rule_file: Option<PathBuf>,
        #[arg(long)]
        consequences: Option<String>,
        #[arg(
            long = "consequences-file",
            help = "Read Consequences section text from a file"
        )]
        consequences_file: Option<PathBuf>,
        #[arg(long = "alternatives-rejected")]
        alternatives_rejected: Option<String>,
        #[arg(
            long = "alternatives-rejected-file",
            help = "Read Alternatives Rejected section text from a file"
        )]
        alternatives_rejected_file: Option<PathBuf>,
        #[arg(long = "operational-impact")]
        operational_impact: Option<String>,
        #[arg(
            long = "operational-impact-file",
            help = "Read Operational Impact section text from a file"
        )]
        operational_impact_file: Option<PathBuf>,
        #[arg(long = "supporting-note")]
        supporting_note: Option<String>,
        #[arg(
            long = "supporting-note-file",
            help = "Read optional supporting note / appendix text from a file"
        )]
        supporting_note_file: Option<PathBuf>,
        #[arg(long, help = "Comma-separated related issue IDs")]
        issues: Option<String>,
        #[arg(long, help = "Comma-separated related file paths")]
        files: Option<String>,
        #[arg(long, help = "Comma-separated tags")]
        tags: Option<String>,
        #[arg(long, default_value = "proposed")]
        status: String,
    },
    #[command(about = "List decision records (optionally filtered by label and/or text)")]
    List {
        /// Only decisions carrying this domain label (DEC-62)
        #[arg(long)]
        label: Option<String>,
        /// Case-insensitive substring over id/title/body/tags
        #[arg(long)]
        text: Option<String>,
    },
    #[command(about = "Show a decision record in full detail")]
    Show { id: String },
    #[command(about = "Mark a decision as accepted")]
    Accept { id: String },
    #[command(about = "Mark a decision as superseded by a newer decision")]
    Supersede {
        superseded: String,
        new: String,
        /// Why the old decision is being replaced (required, DECI-01)
        #[arg(long)]
        reason: String,
    },
    #[command(about = "Set the status of a decision")]
    Set { id: String, status: String },
    #[command(about = "Print a decision as ADR markdown")]
    Adr { id: String },
    // CLI-79: ADR file export is UI-only (mirrors ISS-52 for issues). Agents must
    // have no CLI/MCP path to write file projections — they stay on the MCP rails.
    #[command(
        about = "Generate a batch TOML issue-draft from an accepted decision",
        long_about = DECISION_DRAFT_ISSUES_HELP
    )]
    DraftIssues {
        id: String,
        #[arg(long, default_value = "iss", help = "Issue ID category prefix")]
        category: String,
        #[arg(
            long,
            default_value = "issues-active.md",
            help = "Owning document for generated issues"
        )]
        document: String,
        #[arg(
            long,
            help = "Allow draft generation for proposed or superseded decisions"
        )]
        allow_non_accepted: bool,
    },
    #[command(about = "Permanently delete a decision record from the store")]
    Delete {
        id: String,
        #[arg(long, help = "Confirm permanent deletion (required)")]
        confirm: bool,
    },
    #[command(
        about = "Edit fields of a decision record",
        long_about = "Update one or more fields of a decision record.\n\nAt least one field option is required. List fields (--tags, --issues, --files) replace\nthe entire field. Use `*-file` variants for multi-line text to avoid shell quoting issues.\n\nEXAMPLES\n  ishoo decision edit DEC-001 --decision \"Choose the store-backed model.\"\n  ishoo decision edit DEC-001 --rule-file /tmp/rule.txt\n  ishoo decision edit DEC-001 --tags architecture,model --issues ISS-01,ISS-02\n  ishoo decision edit DEC-001 --consequences \"Smaller surface, stricter constraints.\"\n  ishoo decision edit DEC-001 --title \"Revised title\""
    )]
    Edit {
        id: String,
        #[arg(long, help = "Replace the decision title")]
        title: Option<String>,
        #[arg(long, help = "Decision section text")]
        decision: Option<String>,
        #[arg(
            long = "decision-file",
            help = "Read Decision section text from a file"
        )]
        decision_file: Option<PathBuf>,
        #[arg(long, help = "Problem section text")]
        problem: Option<String>,
        #[arg(long = "problem-file", help = "Read Problem section text from a file")]
        problem_file: Option<PathBuf>,
        #[arg(long, help = "Scope section text")]
        scope: Option<String>,
        #[arg(long = "scope-file", help = "Read Scope section text from a file")]
        scope_file: Option<PathBuf>,
        #[arg(long, help = "Rule section text")]
        rule: Option<String>,
        #[arg(long = "rule-file", help = "Read Rule section text from a file")]
        rule_file: Option<PathBuf>,
        #[arg(long, help = "Consequences section text")]
        consequences: Option<String>,
        #[arg(long = "consequences-file", help = "Read consequences from a file")]
        consequences_file: Option<PathBuf>,
        #[arg(
            long = "alternatives-rejected",
            help = "Alternatives Rejected section text"
        )]
        alternatives_rejected: Option<String>,
        #[arg(
            long = "alternatives-rejected-file",
            help = "Read Alternatives Rejected section text from a file"
        )]
        alternatives_rejected_file: Option<PathBuf>,
        #[arg(long = "operational-impact", help = "Operational Impact section text")]
        operational_impact: Option<String>,
        #[arg(
            long = "operational-impact-file",
            help = "Read Operational Impact section text from a file"
        )]
        operational_impact_file: Option<PathBuf>,
        #[arg(
            long = "supporting-note",
            help = "Optional supporting note / appendix text"
        )]
        supporting_note: Option<String>,
        #[arg(
            long = "supporting-note-file",
            help = "Read optional supporting note / appendix text from a file"
        )]
        supporting_note_file: Option<PathBuf>,
        #[arg(long, help = "Comma-separated tags (replaces existing)")]
        tags: Option<String>,
        #[arg(long, help = "Comma-separated issue ids (replaces existing)")]
        issues: Option<String>,
        #[arg(long, help = "Comma-separated file paths (replaces existing)")]
        files: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PlanCommand {
    #[command(about = "Show the current persisted plan")]
    Show,
    #[command(about = "Add an issue to the plan (appends, or use --after/--before to position)")]
    Add {
        id: String,
        #[arg(
            long,
            conflicts_with = "before",
            help = "Insert immediately after this issue"
        )]
        after: Option<String>,
        #[arg(long, help = "Insert immediately before this issue")]
        before: Option<String>,
    },
    #[command(about = "Reposition an existing plan entry with --after/--before")]
    Move {
        id: String,
        #[arg(
            long,
            conflicts_with = "before",
            help = "Move immediately after this issue"
        )]
        after: Option<String>,
        #[arg(long, help = "Move immediately before this issue")]
        before: Option<String>,
    },
    #[command(about = "Remove an issue from the plan")]
    Remove { id: String },
    #[command(
        about = "Replace the plan with an explicit ordered list (accepts done ids for history)"
    )]
    Set { ids: Vec<String> },
    #[command(about = "Clear the current plan")]
    Clear,
    #[command(about = "Show the next non-done plan item")]
    Next,
    #[command(about = "Populate the plan from local issues, optionally filtered by labels")]
    Populate {
        #[arg(long)]
        labels: Option<String>,
    },
    #[command(about = "Generate a TOML batch plan")]
    Generate { kind: String },
    #[command(name = "new-plan", about = "Create a new named plan and switch to it")]
    NewPlan { name: String },
    #[command(
        name = "list-plans",
        about = "List named plans (archived hidden unless --all)"
    )]
    ListPlans {
        #[arg(long, help = "Include archived plans")]
        all: bool,
    },
    #[command(name = "use", about = "Switch the active named plan")]
    Use { name: String },
    #[command(about = "Set the active plan back to Backlog (the default plan)")]
    Deactivate,
    #[command(about = "Rename the active named plan in place (entries unchanged)")]
    Rename { name: String },
    #[command(about = "Archive a plan: hide it from the list/dropdown but keep it (DEC-26)")]
    Archive { name: String },
    #[command(about = "Delete a plan only if it has zero entries (DEC-26 keeps non-empty plans)")]
    Delete { name: String },
    #[command(name = "drop-plan", about = "Delete a named plan")]
    DropPlan { name: String },
    #[command(
        about = "Link a plan to a release-checkpoint milestone (omit <milestone> to clear) (DEC-73)"
    )]
    Milestone {
        plan: String,
        milestone: Option<String>,
    },
    #[command(hide = true, name = "rebalance")]
    LegacyRebalance,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(
        about = "Initialize a new issue tracker",
        long_about = "Initialize a new issue tracker by creating the canonical sharded store under .ishoo/records/ in the target directory (gitignored on main; its history rides the refs/ishoo/store sidecar ref). Markdown issue files are not created automatically; humans can export markdown projections from the desktop UI when needed."
    )]
    Init,
    #[command(
        about = "Enable Ishoo for agent hosts: user-wide MCP registration, repo adapters, or GitHub publish auth",
        long_about = "By default, materialize this repo's optional host-adapter files (ADPT-01): \
                      .mcp.json, .codex/config.toml, .claude/settings.local.json, .claude/.gitignore, \
                      CLAUDE.md, and AGENTS.md. With --user, register Ishoo + SEMMAP in user/global \
                      host configuration instead (ADPT-03), so Claude Code and Codex can see the MCP \
                      servers from every repo for this user on this machine. Every merge is \
                      idempotent and non-clobbering; existing foreign servers, settings, and user \
                      prose are preserved. Per DEC-88 these files materialize ONLY here, never on a \
                      tool call or startup. With --git-auth, run the one-time GitHub HTTPS auth setup \
                      for this repo: GitHub CLI device-flow login, git credential-helper setup, and \
                      origin URL conversion to https://github.com/owner/repo.git."
    )]
    Enable {
        #[arg(
            long,
            help = "Register/repair user-wide Claude Code and Codex MCP entries instead of repo files"
        )]
        user: bool,
        #[arg(
            long,
            requires = "user",
            help = "Remove Ishoo-owned user-wide MCP entries instead of registering them"
        )]
        remove: bool,
        #[arg(
            long = "git-auth",
            conflicts_with = "remove",
            help = "Configure GitHub HTTPS credential-helper auth for environment-independent publishing"
        )]
        git_auth: bool,
    },
    #[command(about = "List issues matching a query", long_about = LIST_HELP)]
    List {
        #[arg(long = "group-by", default_value = "status", hide = true)]
        group_by: String,
        #[arg(long)]
        compact: bool,
        #[arg(long)]
        linked: bool,
        #[command(flatten)]
        query: IssueQueryArgs,
    },
    #[command(about = "List labels currently in use", long_about = LABELS_HELP)]
    Labels,
    #[command(about = "List source files referenced by issues", long_about = FILES_HELP)]
    Files,
    #[command(about = "List referenced issue refs", long_about = REFS_HELP)]
    Refs {
        #[arg(long, value_enum, default_value = "all")]
        kind: RefKind,
    },
    #[command(about = "Show one issue in full detail", long_about = SHOW_HELP)]
    Show { id: String },
    #[command(about = "Retire an issue as declined", long_about = DECLINE_HELP)]
    Decline {
        id: String,
        #[arg(long)]
        reason: String,
    },
    #[command(about = "Retire an issue as replaced by another issue", long_about = SUPERSEDE_HELP)]
    Supersede {
        id: String,
        #[arg(long = "by")]
        replacement: String,
        #[arg(long)]
        reason: String,
    },
    #[command(about = "Shelve a shelved-labeled issue as retained knowledge", long_about = SHELVE_HELP)]
    Shelve {
        id: String,
        #[arg(long)]
        reason: String,
    },
    #[command(about = "Create a new issue", long_about = NEW_HELP)]
    New {
        title: String,
        #[arg(long, default_value = "iss")]
        category: String,
        #[arg(short, long, default_value = "backlog")]
        status: String,
        /// Create the issue active (default is backlog). Plain `new` never displaces the active issue.
        #[arg(long)]
        active: bool,
        /// Comma-separated labels to assign at creation time; assess urgency with a tier
        #[arg(long)]
        labels: Option<String>,
        /// Issue description (purpose and context). Prefer --description-file for multi-line text.
        #[arg(long)]
        description: Option<String>,
        /// Read description from a file (avoids shell quoting issues)
        #[arg(long = "description-file")]
        description_file: Option<PathBuf>,
        /// Target document partition (e.g. issues-backlog.md)
        #[arg(long = "document", alias = "file")]
        file: Option<String>,
        /// Required named plan home: an existing plan name/id, or new:"<name>" (DEC-55; backlog is a status, not a target)
        #[arg(long)]
        plan: Option<String>,
        /// Governing decision link(s): comma-separated DEC ids, or `none` to
        /// assert no ADR constrains this issue (ADR-02). Required.
        #[arg(long)]
        decisions: Option<String>,
        /// Blocking issue id(s): comma-separated, or `none` to assert no issue
        /// blocks this one (DEC-43). Required — derived plan order needs blockers
        /// declared at filing.
        #[arg(long = "depends-on")]
        depends_on: Option<String>,
    },
    #[command(
        about = "Split a parent into a child issue, recording parent/child lineage (DEC-61)",
        long_about = "Split a parent issue into a focused child, recording the typed \
                      parent-child decomposition lineage (DEC-61). The child takes a full \
                      Scope Contract like `new` and inherits the parent's plan; the parent \
                      stays an umbrella. Run once per child — repeated calls accumulate \
                      children under the same parent. Use this for a scope-BLOCK split so \
                      lineage is durable instead of orphaned sub-issues."
    )]
    Decompose {
        /// The parent issue id to split.
        parent: String,
        /// Title of the child issue to file.
        title: String,
        #[arg(long, default_value = "iss")]
        category: String,
        /// Child Scope Contract text. Prefer --description-file for multi-line text.
        #[arg(long)]
        description: Option<String>,
        /// Read the child's Scope Contract from a file (avoids shell quoting issues).
        #[arg(long = "description-file")]
        description_file: Option<PathBuf>,
        /// Governing decision link(s): comma-separated DEC ids, or `none` (ADR-02). Required.
        #[arg(long)]
        decisions: Option<String>,
        /// Blocking issue id(s): comma-separated, or `none` (DEC-43). Required.
        #[arg(long = "depends-on")]
        depends_on: Option<String>,
    },
    #[command(about = "Edit one issue", long_about = EDIT_HELP)]
    Edit {
        id: String,
        #[arg(long, help = "Replace title (single-line; omit shell metacharacters)")]
        title: Option<String>,
        #[arg(
            long = "title-file",
            help = "Read title from a file (safe for special characters)"
        )]
        title_file: Option<PathBuf>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        labels: Option<String>,
        #[arg(long)]
        files: Option<String>,
        #[arg(long)]
        links: Option<String>,
        #[arg(long = "depends-on")]
        depends_on: Option<String>,
        #[arg(long = "decisions")]
        decisions: Option<String>,
        #[arg(
            long,
            help = "Replace description inline. For multi-line text or text containing \
                    backticks / $() / special characters, prefer --description-file."
        )]
        description: Option<String>,
        #[arg(
            long = "description-file",
            help = "Read description from a file (avoids shell quoting issues)"
        )]
        description_file: Option<PathBuf>,
        #[arg(
            long,
            help = "Replace resolution inline. For multi-line text or text containing \
                    backticks / $() / special characters, prefer --resolution-file."
        )]
        resolution: Option<String>,
        #[arg(
            long = "resolution-file",
            help = "Read resolution from a file (avoids shell quoting issues)"
        )]
        resolution_file: Option<PathBuf>,
        /// Move this issue to a different document partition
        #[arg(long = "document", alias = "file")]
        file: Option<String>,
        /// Assign an owner (user_id from the people registry). Use empty string to clear.
        #[arg(long)]
        owner: Option<String>,
    },
    #[command(about = "Move issues to another document", long_about = MOVE_HELP)]
    Move {
        ids: Vec<String>,
        #[arg(short = 't', long = "to")]
        to: String,
        #[command(flatten)]
        query: BulkQueryArgs,
    },
    #[command(about = "Inspect or mutate the persisted plan", long_about = PLAN_HELP)]
    Plan(PlanArgs),
    #[command(about = "Validate or execute a plan", long_about = APPLY_HELP)]
    Apply {
        plan: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "Apply a batch of mutations atomically", long_about = BATCH_HELP)]
    Batch {
        /// TOML batch file path (use - to read from stdin)
        file: Option<PathBuf>,
        /// Validate and preview without writing
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "Check issue hygiene and consistency", long_about = LINT_HELP)]
    Lint {
        #[arg(long)]
        strict: bool,
    },
    #[command(about = "Register a linked project", long_about = LINK_HELP)]
    Link {
        /// Folder to register (omit when using --list)
        #[arg(value_name = "PATH")]
        target: Option<PathBuf>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long = "list")]
        list: bool,
        /// Git remote URL for syncing when local path is unavailable
        #[arg(long)]
        repo: Option<String>,
    },
    #[command(about = "Update local path for a linked project", long_about = RELINK_HELP)]
    Relink { key: String, path: PathBuf },
    #[command(about = "Set the status of an issue (shorthand for edit --status)", long_about = SET_HELP)]
    Set { id: String, status: String },
    #[command(about = "Rename an issue ID and update all references", long_about = RENAME_ID_HELP)]
    RenameId { old: String, new: String },
    #[command(about = "Print help for a command or the full command surface", long_about = HELP_HELP)]
    Help(HelpArgs),
    #[command(about = "Permanently delete an issue from the store")]
    Delete {
        id: String,
        #[arg(short, long, help = "Skip the done-issue safety check")]
        force: bool,
    },
    #[command(hide = true)]
    Split {
        #[arg(short = 't', long = "to")]
        to: String,
        #[command(flatten)]
        query: BulkQueryArgs,
    },
    #[command(hide = true)]
    Archive {
        #[arg(short = 't', long = "to", default_value = "archive.md")]
        to: String,
        #[command(flatten)]
        query: BulkQueryArgs,
    },
    #[command(
        about = "Claim an issue and prepare an isolated worktree for execution",
        long_about = "Claim an issue and prepare an isolated worktree for execution.\n\n\
            Creates a hidden claim ref (refs/ishoo/claims/<id>) as the execution authority \
            and an isolated git worktree (.ishoo/worktrees/<id>) on the execution branch \
            (ishoo/<id>). If the issue is already claimed by a stale holder the command \
            refuses; use `ishoo reclaim` to force-take a stale claim."
    )]
    Start { id: String },
    #[command(
        about = "Release the claim and remove the worktree for a finished execution",
        long_about = "Release the claim ref and remove the isolated worktree for an issue \
            whose execution is complete. Run this after neti check passes and before landing."
    )]
    Finish { id: String },
    #[command(
        about = "Complete an issue: gates, mark done, tear down its worktree (default path)",
        long_about = DONE_HELP
    )]
    Done { id: String },
    #[command(about = "Mark an issue done and accepted into the main branch", long_about = LAND_HELP)]
    Land { id: String },
    #[command(
        about = "Force-take a stale claim for an issue",
        long_about = "Inspect the current claim for an issue and, if it is stale, overwrite \
            it to become the new claimant. Fails if the claim is fresh (actively held).\n\n\
            Default staleness threshold: 3600 seconds (1 hour)."
    )]
    Reclaim { id: String },
    #[command(
        about = "Sweep orphaned execution substrate (crash backstop)",
        long_about = "Garbage-collect orphaned execution substrate left by a crash (DEC-35): \
            remove worktrees whose claim ref is gone, claim refs with no worktree, and execution \
            branches `ishoo/<id>` that have no live claim and are fully merged into main. \
            Unmerged orphan branches are reported, never deleted. A clean state prints nothing to do."
    )]
    Gc,
    #[command(
        name = "migrate-stores",
        about = "Convert Ishoo stores to the additive-safe v3 wire format (ARCH-04)",
        long_about = "Convert this project's store — or, with --all, every project in your Library — \
            to the additive-safe v3 shard format, so future Ishoo upgrades can add fields without \
            ever bricking the store (the FIX-120 class). This is a ONE-WAY flip: once a store is v3, \
            an OLDER Ishoo binary will refuse to read it, so upgrade Ishoo on every machine that \
            shares the store BEFORE migrating. Runs as a dry-run preview unless you pass --yes."
    )]
    MigrateStores {
        #[arg(
            long,
            help = "Migrate every project in the Library, not just the current one"
        )]
        all: bool,
        #[arg(
            long,
            help = "Actually perform the irreversible conversion (otherwise preview only)"
        )]
        yes: bool,
    },
    #[command(hide = true, name = "claim-refresh")]
    ClaimRefresh { id: String },
    #[command(hide = true)]
    Heatmap,
    #[command(hide = true)]
    Dash,
    #[command(about = "Manage decision records", long_about = DECISION_HELP)]
    Decision(DecisionArgs),
    #[command(about = "Manage milestones")]
    Milestone(MilestoneArgs),
    #[command(about = "Manage version configuration")]
    Version(VersionArgs),
    #[command(about = "Manage the project charter (purpose, audience, success, non-goals)")]
    Charter(CharterArgs),
    #[command(about = "Manage named epics (ID-prefix workstreams)")]
    Epic(EpicArgs),
    #[command(about = "Manage the roadmap (ordered sequence of epics)")]
    Roadmap(RoadmapArgs),
    #[command(about = "Manage the people registry")]
    People(PeopleArgs),
    #[command(about = "Add or list comments/notes on an issue")]
    Comment(CommentArgs),
    #[command(
        about = "Print the live orientation card (workspace, focus, contracts, next command)"
    )]
    Status,
    #[command(about = "Print the mechanical readiness card for an issue before landing")]
    Preflight { id: String },
    #[command(
        name = "search-issues",
        about = "Concept search over issues by meaning (synonyms/concepts), not literal words"
    )]
    SearchIssues {
        /// The concept to search for (a phrase is fine; matched by meaning).
        query: String,
        /// Max results to return.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    #[command(
        name = "candidates",
        about = "Gather the bounded next-work candidate set for a lens (concept recall ∪ safety/breaking anchor, minus blocked)"
    )]
    Candidates {
        /// The lens: a short phrase of high-priority concepts (from the charter / current phase).
        lens: String,
        /// Max concept-search hits to union into the candidate set.
        #[arg(long, default_value_t = 12)]
        top: usize,
    },
    #[command(
        about = "Detect & heal split/legacy/dangling stores (tracked .ishoo, dangling plan refs, unpublished refs)",
        long_about = "Diagnose store durability faults that the normal mutation path cannot reach: \
                      .ishoo/ paths still tracked on main (legacy half-migrated store), plan entries \
                      pointing at absent records (dangling), and local refs ahead of the remote. \
                      Read-only by default; pass --fix to untrack legacy paths (working files are \
                      preserved), re-snapshot the store to refs/ishoo/store, and publish. Lost \
                      record bodies are reported as unrecoverable, never fabricated."
    )]
    Doctor {
        #[arg(long, help = "Apply the bounded heal instead of only reporting")]
        fix: bool,
    },
    #[command(
        name = "resolve-store",
        about = "Resolve a same-record store conflict by keeping one side of every conflicting record (DEC-50)",
        long_about = "When ishoo reports \"the same record changed on the remote\", converge by keeping \
                      every conflicting record from one side: `keep-mine` retains this checkout's \
                      version, `take-remote` takes origin's. Distinct-record edits already auto-merge; \
                      this only decides the records that changed on both sides, then advances and pushes \
                      the merged store ref. Run with no side to just list the conflicting records."
    )]
    ResolveStore {
        #[arg(
            value_parser = ["keep-mine", "take-remote", "newest"],
            help = "How to resolve each conflicting record: keep-mine, take-remote, or newest \
                    (newest-mutation-wins, the diverged-store recovery default). Omit to only \
                    list the conflicts. `newest` backs up both original tips to refs/ishoo/backup/*."
        )]
        side: Option<String>,
    },
    #[command(about = "Print the agent protocol (SEMMAP-first workflow + command ladder)")]
    Brief,
    #[command(
        about = "Run the MCP server (stdio JSON-RPC) for agent tool calls (DEC-49)",
        long_about = "Serve ishoo as an MCP (Model Context Protocol) server over stdio. Reads \
                      newline-delimited JSON-RPC requests on stdin and writes responses on \
                      stdout, answering initialize / tools/list / tools/call. Tool handlers call \
                      ishoo's core functions directly and return structured JSON (DEC-49). \
                      Intended to be launched by an MCP host (e.g. Claude Code via .mcp.json), \
                      not run interactively."
    )]
    Mcp,
    #[command(hide = true, name = "mcp-owner")]
    McpOwner,
}

#[derive(clap::Args)]
pub struct CharterArgs {
    #[command(subcommand)]
    pub command: CharterCommand,
}

#[derive(Subcommand)]
pub enum CharterCommand {
    #[command(
        about = "Create the project charter (errors if one already exists — use `edit` to change it)"
    )]
    Set {
        #[arg(long, help = "What this project is and why it exists (required)")]
        purpose: Option<String>,
        #[arg(long = "purpose-file", help = "Read purpose text from a file")]
        purpose_file: Option<PathBuf>,
        #[arg(long, help = "Who the project is for")]
        audience: Option<String>,
        #[arg(long = "audience-file", help = "Read audience text from a file")]
        audience_file: Option<PathBuf>,
        #[arg(long, help = "What success looks like")]
        success: Option<String>,
        #[arg(long = "success-file", help = "Read success text from a file")]
        success_file: Option<PathBuf>,
        #[arg(long = "non-goals", help = "Explicit non-goals (what this project will NOT do)")]
        non_goals: Option<String>,
        #[arg(long = "non-goals-file", help = "Read non-goals text from a file")]
        non_goals_file: Option<PathBuf>,
    },
    #[command(about = "Print the project charter")]
    Show,
    #[command(about = "Amend the existing charter (only provided fields change)")]
    Edit {
        #[arg(long)]
        purpose: Option<String>,
        #[arg(long = "purpose-file", help = "Read purpose text from a file")]
        purpose_file: Option<PathBuf>,
        #[arg(long)]
        audience: Option<String>,
        #[arg(long = "audience-file", help = "Read audience text from a file")]
        audience_file: Option<PathBuf>,
        #[arg(long)]
        success: Option<String>,
        #[arg(long = "success-file", help = "Read success text from a file")]
        success_file: Option<PathBuf>,
        #[arg(long = "non-goals")]
        non_goals: Option<String>,
        #[arg(long = "non-goals-file", help = "Read non-goals text from a file")]
        non_goals_file: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
pub struct MilestoneArgs {
    #[command(subcommand)]
    pub command: MilestoneCommand,
}

#[derive(Subcommand)]
pub enum MilestoneCommand {
    #[command(about = "Create a new milestone")]
    New {
        title: String,
        #[arg(long)]
        target_version: Option<String>,
    },
    #[command(about = "List all milestones")]
    List,
    #[command(about = "Show milestone details and issue completion")]
    Show { id: String },
    #[command(about = "Close a milestone")]
    Close { id: String },
    #[command(about = "Link an issue to a milestone")]
    Link {
        issue_id: String,
        milestone_id: String,
    },
    #[command(about = "Check if milestone is release-ready (all issues done)")]
    Check { id: String },
}

#[derive(clap::Args)]
pub struct VersionArgs {
    #[command(subcommand)]
    pub command: VersionCommand,
}

#[derive(Subcommand)]
pub enum VersionCommand {
    #[command(about = "Read the current version from the configured source")]
    Get,
    #[command(about = "Update the configured version source to a target version")]
    Bump {
        target_version: String,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "Configure the version source file")]
    SetSource {
        source_file: String,
        #[arg(long, default_value = "cargo")]
        kind: String,
    },
}

#[derive(clap::Args)]
pub struct EpicArgs {
    #[command(subcommand)]
    pub command: EpicCommand,
}

#[derive(Subcommand)]
pub enum EpicCommand {
    #[command(about = "Declare a named epic (ID prefix as workstream)")]
    New {
        prefix: String,
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        milestone_id: Option<String>,
    },
    #[command(about = "List all declared epics")]
    List,
    #[command(about = "Show epic details and associated issues")]
    Show { prefix: String },
}

#[derive(clap::Args)]
pub struct RoadmapArgs {
    #[command(subcommand)]
    pub command: RoadmapCommand,
}

#[derive(Subcommand)]
pub enum RoadmapCommand {
    #[command(about = "Show the current roadmap")]
    Show,
    #[command(about = "Set the roadmap as an ordered list of epic prefixes")]
    Set { prefixes: Vec<String> },
}

#[derive(Args)]
pub struct PeopleArgs {
    #[command(subcommand)]
    pub command: PeopleCommand,
}

#[derive(Args)]
pub struct CommentArgs {
    #[command(subcommand)]
    pub command: CommentCommand,
}

#[derive(Subcommand)]
pub enum CommentCommand {
    #[command(about = "Append a comment/note to an issue")]
    Add {
        /// Issue id (e.g. FEAT-05)
        id: String,
        /// Comment text. Omit when using --text-file.
        text: Option<String>,
        /// Read comment text from a file instead of the positional argument.
        #[arg(long)]
        text_file: Option<String>,
        /// Author person user_id. Defaults to the issue owner.
        #[arg(long)]
        author: Option<String>,
    },
    #[command(about = "List the comment thread on an issue")]
    List {
        /// Issue id (e.g. FEAT-05)
        id: String,
    },
    #[command(about = "Edit a comment by its index (see `comment list`)")]
    Edit {
        /// Issue id (e.g. FEAT-05)
        id: String,
        /// Zero-based comment index, oldest first (from `comment list`).
        index: usize,
        /// New comment text. Omit when using --text-file.
        text: Option<String>,
        /// Read new comment text from a file instead of the positional argument.
        #[arg(long)]
        text_file: Option<String>,
    },
    #[command(about = "Remove a comment by its index (see `comment list`)")]
    Remove {
        /// Issue id (e.g. FEAT-05)
        id: String,
        /// Zero-based comment index, oldest first (from `comment list`).
        index: usize,
    },
}

#[derive(Subcommand)]
pub enum PeopleCommand {
    #[command(about = "Register a person in the people registry")]
    Add {
        user_id: String,
        name: String,
        #[arg(long)]
        avatar: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        bio: Option<String>,
    },
    #[command(about = "List registered people")]
    List,
    #[command(about = "Set the machine-local current person (not shared in the store)")]
    Use {
        /// Registered person user_id to make current on this machine.
        user_id: String,
    },
    #[command(about = "Print the machine-local current person")]
    Whoami,
}

const HELP_HELP: &str = "Print built-in help.\n\nUSAGE\n  ishoo help\n  ishoo help <command>\n  ishoo help --all\n\nBEHAVIOR\n  Without arguments, prints the root help text.\n  With a command name, prints the long help for that command.\n  With `--all`, prints the root help plus every visible command help section.\n\nEXAMPLES\n  ishoo help\n  ishoo help list\n  ishoo help --all";

pub fn render_help(command: Option<&str>) -> Result<String, String> {
    let mut cli = Cli::command();
    let mut output = Vec::new();
    match command {
        None => cli
            .write_long_help(&mut output)
            .map_err(|error| format!("Failed to render help: {error}"))?,
        Some(command_name) => {
            let subcommand = cli
                .find_subcommand_mut(command_name)
                .ok_or_else(|| format!("Unknown help topic: {command_name}"))?;
            subcommand
                .write_long_help(&mut output)
                .map_err(|error| format!("Failed to render help for {command_name}: {error}"))?;
        }
    }
    String::from_utf8(output).map_err(|error| format!("Help output was not valid UTF-8: {error}"))
}

pub fn render_all_help() -> Result<String, String> {
    let cli = Cli::command();
    let visible_command_names = cli
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| subcommand.get_name().to_string())
        .collect::<Vec<_>>();

    let mut sections = vec![render_help(None)?];
    for name in visible_command_names {
        sections.push(format!("\n=== {name} ===\n\n{}", render_help(Some(&name))?));
    }
    Ok(sections.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_accepts_global_path_after_subcommand() {
        let cli = Cli::try_parse_from(["ishoo", "init", "--path", "workspace"]).unwrap();
        assert_eq!(cli.path, PathBuf::from("workspace"));
        assert!(matches!(cli.command, Some(Commands::Init)));
    }

    #[test]
    fn enable_accepts_git_auth_onboarding_flag() {
        let cli = Cli::try_parse_from(["ishoo", "enable", "--git-auth"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Enable {
                git_auth: true,
                remove: false,
                ..
            })
        ));
    }

    #[test]
    fn help_all_parses() {
        let cli = Cli::try_parse_from(["ishoo", "help", "--all"]).unwrap();
        match cli.command {
            Some(Commands::Help(args)) => {
                assert!(args.all);
                assert!(args.command.is_none());
            }
            _ => panic!("expected help command"),
        }
    }

    #[test]
    fn retirement_commands_require_their_named_arguments() {
        assert!(Cli::try_parse_from(["ishoo", "decline", "ISS-01"]).is_err());
        assert!(
            Cli::try_parse_from(["ishoo", "supersede", "ISS-01", "--reason", "replaced"]).is_err()
        );
        assert!(Cli::try_parse_from(["ishoo", "supersede", "ISS-01", "--by", "ISS-02"]).is_err());
    }

    #[test]
    fn render_all_help_includes_visible_commands() {
        let help = render_all_help().unwrap();
        assert!(help.contains("=== list ==="));
        assert!(help.contains("=== help ==="));
        assert!(help.contains("ishoo help --all"));
    }

    #[test]
    fn plan_show_parses() {
        let cli = Cli::try_parse_from(["ishoo", "plan", "show"]).unwrap();
        match cli.command {
            Some(Commands::Plan(args)) => match args.command {
                PlanCommand::Show => {}
                _ => panic!("expected plan show"),
            },
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn legacy_plan_shortcut_still_parses() {
        let cli = Cli::try_parse_from(["ishoo", "plan", "rebalance"]).unwrap();
        match cli.command {
            Some(Commands::Plan(args)) => match args.command {
                PlanCommand::LegacyRebalance => {}
                _ => panic!("expected legacy rebalance shortcut"),
            },
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn edit_resolution_file_parses() {
        let cli = Cli::try_parse_from([
            "ishoo",
            "edit",
            "ISS-01",
            "--resolution-file",
            "/tmp/resolution.txt",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Edit {
                resolution_file, ..
            }) => {
                assert_eq!(resolution_file, Some(PathBuf::from("/tmp/resolution.txt")));
            }
            _ => panic!("expected edit command"),
        }
    }

    #[test]
    fn export_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["ishoo", "export"]).is_err());
    }

    // CLI-79: ADR file export is UI-only too — agents must have no CLI path to
    // write file projections (issues or ADRs). `ishoo decision export` is removed.
    #[test]
    fn decision_export_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["ishoo", "decision", "export"]).is_err());
        assert!(
            Cli::try_parse_from(["ishoo", "decision", "export", "--out-dir", "docs/x"]).is_err()
        );
    }

    #[test]
    fn link_keeps_workspace_root_separate_from_link_target() {
        let cli =
            Cli::try_parse_from(["ishoo", "--path", "/tmp/source", "link", "/tmp/target"]).unwrap();
        assert_eq!(cli.path, PathBuf::from("/tmp/source"));
        match cli.command {
            Some(Commands::Link { target, .. }) => {
                assert_eq!(target, Some(PathBuf::from("/tmp/target")));
            }
            _ => panic!("expected link command"),
        }
    }
}
