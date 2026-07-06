mod handlers;
// AX command-card render primitive (DEC-27). First consumer: `ishoo status`
// (AX-05); also rendered by `ishoo preflight` (AX-06) and core-verb cards (AX-07).
pub(crate) mod card;

// MCP-02/DEC-49: typed orientation core fn, re-exported crate-wide so the `mcp`
// module's ishoo_status seed tool serializes the same facts the CLI card renders.
// FIX-122: the MCP handler consumes the workspace-injected variant so it never
// reaches the CLI's process-exiting loader on a missing store.
pub(crate) use handlers::status_report_with_workspace;

use crate::main_cli::{
    build_bulk_query, build_issue_query, render_all_help, render_help, CharterCommand, Cli,
    Commands, CommentCommand, DecisionCommand, EpicCommand, MilestoneCommand, PeopleCommand,
    PlanCommand, RefKind, RoadmapCommand, VersionCommand,
};
use crate::model;

struct MoveArgs {
    ids: Vec<String>,
    to: String,
    query: model::IssueQuery,
}

struct LinkArgs {
    link_path: Option<std::path::PathBuf>,
    key: Option<String>,
    label: Option<String>,
    list: bool,
    repo: Option<String>,
}

struct NewArgs {
    title: String,
    category: String,
    status: String,
    active: bool,
    labels: Option<String>,
    description: Option<String>,
    description_file: Option<String>,
    file: Option<String>,
    plan: Option<String>,
    decisions: Option<String>,
    depends_on: Option<String>,
}

struct DecomposeArgs {
    parent: String,
    title: String,
    category: String,
    description: Option<String>,
    description_file: Option<String>,
    decisions: Option<String>,
    depends_on: Option<String>,
}

struct SplitLikeArgs {
    to: String,
    query: model::IssueQuery,
    archive_done_by_default: bool,
}

pub fn run(cli: Cli) -> i32 {
    if matches!(cli.command, Some(Commands::Init)) {
        return handlers::run_init(&cli);
    }
    if matches!(cli.command, Some(Commands::Brief)) {
        // Resolve the workspace root so the charter (if any) can be appended,
        // without triggering the gitignore/adapter side effects of the main path.
        let path = model::discover_root(&cli.path);
        return run_brief(&path);
    }
    // ADPT-01 (DEC-88): the ONLY place repo host-adapter files materialize. Works on a
    // repo that has no Ishoo store yet, so it runs before store discovery.
    if let Some(Commands::Enable {
        user,
        remove,
        git_auth,
    }) = cli.command
    {
        return handlers::run_enable(&cli.path, user, remove, git_auth);
    }
    if let Some(Commands::Help(args)) = cli.command {
        return if args.all {
            match render_all_help() {
                Ok(help) => {
                    print!("{help}");
                    0
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    1
                }
            }
        } else {
            match render_help(args.command.as_deref()) {
                Ok(help) => {
                    print!("{help}");
                    0
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    1
                }
            }
        };
    }

    let path = model::discover_root(&cli.path);
    model::ensure_ishoo_gitignore(&path);
    // DEC-88 supersedes the old auto-materialization: repo host-adapter files
    // (including `.mcp.json`) materialize ONLY on explicit `ishoo enable`, never on an
    // arbitrary command — a silent write to a repo the user did not ask to enable is
    // exactly what DEC-88 forbids. `.ishoo/`-gitignore above is store infrastructure,
    // not a host adapter, so it stays automatic.
    // MCP-02: the MCP server is a long-running stdio loop that owns its own
    // per-tool-call store lifecycle, so it bypasses the single-shot autocommit
    // wrapper in dispatch_command.
    if matches!(cli.command, Some(Commands::Mcp)) {
        return crate::mcp::run_server(path);
    }
    if matches!(cli.command, Some(Commands::McpOwner)) {
        return crate::mcp::run_owner_server(path);
    }
    dispatch_command(path, cli.command.unwrap_or(Commands::Dash))
}

fn dispatch_command(path: std::path::PathBuf, command: Commands) -> i32 {
    let autocommit_store = command_autocommits_store(&command);
    let commit_path = path.clone();
    let code = match command {
        Commands::Dash => handlers::run_dash(path),
        Commands::Decision(args) => handlers::run_decision(path, args),
        Commands::Milestone(args) => handlers::run_milestone(path, args),
        Commands::Version(args) => handlers::run_version(path, args),
        Commands::Charter(args) => handlers::run_charter(path, args),
        Commands::Epic(args) => handlers::run_epic(path, args),
        Commands::Roadmap(args) => handlers::run_roadmap(path, args),
        Commands::People(args) => handlers::run_people(path, args),
        Commands::Comment(args) => handlers::run_comment(path, args),
        Commands::Status => handlers::run_status(path),
        Commands::Preflight { id } => handlers::run_preflight(path, id),
        Commands::SearchIssues { query, top } => handlers::run_search_issues(path, query, top),
        Commands::Candidates { lens, top } => handlers::run_candidates(path, lens, top),
        Commands::Doctor { fix } => handlers::run_doctor(path, fix),
        Commands::ResolveStore { side } => handlers::run_resolve_store(path, side),
        Commands::Plan(args) => handlers::run_plan(path, args.command),
        Commands::Apply { plan, dry_run } => handlers::run_apply(path, plan, dry_run),
        Commands::Batch { file, dry_run } => handlers::run_batch(path, file, dry_run),
        Commands::Lint { strict } => handlers::run_lint(path, strict),
        Commands::Set { id, status } => handlers::run_set(path, id, status),
        Commands::Decline { id, reason } => handlers::run_decline(path, id, reason),
        Commands::Shelve { id, reason } => handlers::run_shelve(path, id, reason),
        Commands::Supersede {
            id,
            replacement,
            reason,
        } => handlers::run_supersede(path, id, replacement, reason),
        Commands::Labels => handlers::run_labels(path),
        Commands::Files => handlers::run_files(path),
        Commands::Refs { kind } => handlers::run_refs(path, to_ref_list_kind(kind)),
        Commands::Delete { id, force } => handlers::run_delete(path, id, force),
        Commands::Start { id } => handlers::run_start(path, id),
        Commands::Finish { id } => handlers::run_finish(path, id),
        Commands::Done { id } => handlers::run_done(path, id),
        Commands::Land { id } => handlers::run_land(path, id),
        Commands::Reclaim { id } => handlers::run_reclaim(path, id),
        Commands::Gc => handlers::run_gc(path),
        Commands::MigrateStores { all, yes } => handlers::run_migrate_stores(path, all, yes),
        Commands::ClaimRefresh { id } => handlers::run_claim_refresh(path, id),
        Commands::Heatmap => handlers::run_heatmap(path),
        Commands::Link {
            target: link_path,
            key,
            label,
            list,
            repo,
        } => handlers::run_link(
            path,
            LinkArgs {
                link_path,
                key,
                label,
                list,
                repo,
            },
        ),
        Commands::Relink {
            key,
            path: new_path,
        } => handlers::run_relink(path, key, new_path),
        Commands::Help(..) => unreachable!(),
        Commands::Init => unreachable!(),
        Commands::Enable { .. } => unreachable!("Enable is handled before dispatch_command"),
        Commands::Mcp => unreachable!("Mcp is handled before dispatch_command"),
        Commands::McpOwner => unreachable!("McpOwner is handled before dispatch_command"),
        other => dispatch_workspace_command(path, other),
    };

    if code == 0 && autocommit_store {
        if let Err(error) =
            handlers::commit_store_mutation(&commit_path, "chore(ishoo): commit store mutation")
        {
            eprintln!("Error: {error}");
            return 1;
        }
    }

    code
}

fn command_autocommits_store(command: &Commands) -> bool {
    match command {
        Commands::Decision(args) => matches!(
            args.command,
            DecisionCommand::New { .. }
                | DecisionCommand::Accept { .. }
                | DecisionCommand::Supersede { .. }
                | DecisionCommand::Set { .. }
                | DecisionCommand::Delete { .. }
                | DecisionCommand::Edit { .. }
        ),
        Commands::Milestone(args) => matches!(
            args.command,
            MilestoneCommand::New { .. }
                | MilestoneCommand::Close { .. }
                | MilestoneCommand::Link { .. }
        ),
        Commands::Version(args) => matches!(
            args.command,
            VersionCommand::SetSource { .. } | VersionCommand::Bump { dry_run: false, .. }
        ),
        Commands::Charter(args) => matches!(
            args.command,
            CharterCommand::Set { .. } | CharterCommand::Edit { .. }
        ),
        Commands::Epic(args) => matches!(args.command, EpicCommand::New { .. }),
        Commands::Roadmap(args) => matches!(args.command, RoadmapCommand::Set { .. }),
        Commands::People(args) => matches!(
            args.command,
            PeopleCommand::Add { .. } | PeopleCommand::Use { .. }
        ),
        Commands::Comment(args) => matches!(
            args.command,
            CommentCommand::Add { .. }
                | CommentCommand::Edit { .. }
                | CommentCommand::Remove { .. }
        ),
        Commands::Plan(args) => matches!(
            args.command,
            PlanCommand::Add { .. }
                | PlanCommand::Move { .. }
                | PlanCommand::Remove { .. }
                | PlanCommand::Set { .. }
                | PlanCommand::Clear
                | PlanCommand::Populate { .. }
                | PlanCommand::NewPlan { .. }
                | PlanCommand::Use { .. }
                | PlanCommand::Deactivate
                | PlanCommand::Rename { .. }
                | PlanCommand::Archive { .. }
                | PlanCommand::Delete { .. }
                | PlanCommand::DropPlan { .. }
        ),
        Commands::Apply { dry_run, .. } | Commands::Batch { dry_run, .. } => !dry_run,
        Commands::Set { .. }
        | Commands::Decline { .. }
        | Commands::Shelve { .. }
        | Commands::Supersede { .. }
        | Commands::New { .. }
        | Commands::Decompose { .. }
        | Commands::Edit { .. }
        | Commands::RenameId { .. }
        | Commands::Move { .. }
        | Commands::Split { .. }
        | Commands::Archive { .. }
        | Commands::Delete { .. }
        | Commands::Start { .. }
        | Commands::Finish { .. }
        | Commands::Done { .. }
        | Commands::Land { .. }
        | Commands::Reclaim { .. }
        | Commands::Link { .. }
        | Commands::Relink { .. } => true,
        Commands::Dash
        | Commands::List { .. }
        | Commands::Labels
        | Commands::Files
        | Commands::Refs { .. }
        | Commands::Show { .. }
        | Commands::Status
        | Commands::Preflight { .. }
        | Commands::SearchIssues { .. }
        | Commands::Candidates { .. }
        | Commands::Doctor { .. }
        | Commands::ResolveStore { .. }
        | Commands::Lint { .. }
        | Commands::Gc
        | Commands::MigrateStores { .. }
        | Commands::ClaimRefresh { .. }
        | Commands::Heatmap
        | Commands::Brief
        | Commands::Help(..)
        | Commands::Mcp
        | Commands::McpOwner
        | Commands::Enable { .. }
        | Commands::Init => false,
    }
}

fn dispatch_workspace_command(path: std::path::PathBuf, command: Commands) -> i32 {
    match command {
        Commands::List {
            group_by,
            compact,
            linked,
            query,
        } => handlers::run_list(path, group_by, compact, linked, build_issue_query(&query)),
        Commands::Show { id } => handlers::run_show(path, &id),
        Commands::New {
            title,
            category,
            status,
            active,
            labels,
            description,
            description_file,
            file,
            plan,
            decisions,
            depends_on,
        } => handlers::run_new(
            path,
            NewArgs {
                title,
                category,
                status,
                active,
                labels,
                description,
                description_file: description_file.map(|p| p.to_string_lossy().into_owned()),
                file,
                plan,
                decisions,
                depends_on,
            },
        ),
        Commands::Decompose {
            parent,
            title,
            category,
            description,
            description_file,
            decisions,
            depends_on,
        } => handlers::run_decompose(
            path,
            DecomposeArgs {
                parent,
                title,
                category,
                description,
                description_file: description_file.map(|p| p.to_string_lossy().into_owned()),
                decisions,
                depends_on,
            },
        ),
        Commands::Edit {
            id,
            title,
            title_file,
            status,
            labels,
            files,
            links,
            depends_on,
            decisions,
            description,
            description_file,
            resolution,
            resolution_file,
            file,
            owner,
        } => handlers::run_edit(
            path,
            id,
            model::EditArgs {
                title,
                title_file,
                status,
                labels,
                files,
                links,
                depends_on,
                decisions,
                description,
                description_file,
                resolution,
                resolution_file,
                file,
                owner,
            },
        ),
        Commands::RenameId { old, new } => handlers::run_rename_id(path, old, new),
        Commands::Move { ids, to, query } => handlers::run_move(
            path,
            MoveArgs {
                ids,
                to,
                query: build_bulk_query(&query),
            },
        ),
        Commands::Split { to, query } => run_split_like(
            path,
            SplitLikeArgs {
                to,
                query: build_bulk_query(&query),
                archive_done_by_default: false,
            },
        ),
        Commands::Archive { to, query } => run_split_like(
            path,
            SplitLikeArgs {
                to,
                query: build_bulk_query(&query),
                archive_done_by_default: true,
            },
        ),
        Commands::Dash
        | Commands::Decision(..)
        | Commands::Milestone(..)
        | Commands::Version(..)
        | Commands::Charter(..)
        | Commands::Plan(..)
        | Commands::Apply { .. }
        | Commands::Batch { .. }
        | Commands::Lint { .. }
        | Commands::Set { .. }
        | Commands::Decline { .. }
        | Commands::Shelve { .. }
        | Commands::Supersede { .. }
        | Commands::Help(..)
        | Commands::Labels
        | Commands::Files
        | Commands::Refs { .. }
        | Commands::Delete { .. }
        | Commands::Start { .. }
        | Commands::Finish { .. }
        | Commands::Done { .. }
        | Commands::Land { .. }
        | Commands::Reclaim { .. }
        | Commands::Gc
        | Commands::ClaimRefresh { .. }
        | Commands::Heatmap
        | Commands::Link { .. }
        | Commands::Relink { .. }
        | Commands::Epic(..)
        | Commands::Roadmap(..)
        | Commands::People(..)
        | Commands::Comment(..)
        | Commands::Status
        | Commands::Preflight { .. }
        | Commands::SearchIssues { .. }
        | Commands::Candidates { .. }
        | Commands::Doctor { .. }
        | Commands::ResolveStore { .. }
        | Commands::MigrateStores { .. }
        | Commands::Brief
        | Commands::Mcp
        | Commands::McpOwner
        | Commands::Enable { .. }
        | Commands::Init => unreachable!(),
    }
}

fn run_split_like(path: std::path::PathBuf, args: SplitLikeArgs) -> i32 {
    handlers::run_split_or_archive(
        path,
        MoveArgs {
            ids: vec![],
            to: args.to,
            query: args.query,
        },
        args.archive_done_by_default,
    )
}

fn load_workspace(path: &std::path::Path) -> model::Workspace {
    match model::Workspace::load_with_recorded_timestamps(path) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn to_ref_list_kind(kind: RefKind) -> model::RefListKind {
    match kind {
        RefKind::All => model::RefListKind::All,
        RefKind::Links => model::RefListKind::Links,
        RefKind::DependsOn => model::RefListKind::DependsOn,
    }
}

const BRIEF_DEFAULT: &str = include_str!("agent/brief.md");

fn agent_doc_content(filename: &str, default: &str) -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() {
        let override_path = std::path::Path::new(&home).join(".ishoo").join(filename);
        if let Ok(content) = std::fs::read_to_string(&override_path) {
            return content;
        }
    }
    default.to_string()
}

/// The full agent protocol text (the same source as `ishoo brief`), honoring a
/// `~/.ishoo/brief.md` override. Shared so the `ishoo_brief` MCP tool (MCP-32)
/// returns exactly what the CLI prints. CORE-04: when the project has a charter,
/// it is appended so every agent sees the project's purpose and non-goals at
/// session start. A best-effort read — a missing/unreadable store just yields the
/// base brief, never an error.
pub(crate) fn agent_brief(path: &std::path::Path) -> String {
    let mut brief = agent_doc_content("brief.md", BRIEF_DEFAULT);
    if let Ok(Some(charter)) = model::load_charter(path) {
        if !brief.ends_with('\n') {
            brief.push('\n');
        }
        brief.push_str("\n---\n\n");
        brief.push_str(&model::render_charter(&charter));
    }
    brief
}

fn run_brief(path: &std::path::Path) -> i32 {
    print!("{}", agent_brief(path));
    0
}

#[cfg(test)]
#[path = "main_dispatch_agent_tests.rs"]
mod agent_tests;
#[cfg(test)]
#[path = "main_dispatch_linked_tests.rs"]
mod linked_tests;
#[cfg(test)]
#[path = "main_dispatch_workflow_tests.rs"]
mod workflow_tests;
