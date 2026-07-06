use super::{load_workspace, MoveArgs};
use crate::main_cli::{
    CharterArgs, Cli, CommentArgs, CommentCommand, DecisionArgs, EpicArgs, MilestoneArgs,
    PeopleArgs, PlanCommand, RoadmapArgs, VersionArgs,
};
use crate::{model, ui};

#[path = "handlers_charter.rs"]
mod handlers_charter;
#[path = "handlers_decisions.rs"]
mod handlers_decisions;
#[path = "handlers_linked.rs"]
mod handlers_linked;
#[path = "handlers_new.rs"]
mod handlers_new;
#[path = "handlers_people.rs"]
mod handlers_people;
#[path = "handlers_preflight.rs"]
mod handlers_preflight;
#[path = "handlers_release.rs"]
mod handlers_release;
#[path = "handlers_status.rs"]
mod handlers_status;
#[path = "handlers_structure.rs"]
mod handlers_structure;

// MCP-02/DEC-49: the `ishoo mcp` seed tool serializes the same typed report the
// `status` card renders. Re-export the core fn so the mcp module can call it
// directly without reimplementing orientation logic.
pub(crate) use handlers_status::status_report_with_workspace;

pub(super) fn run_status(path: std::path::PathBuf) -> i32 {
    handlers_status::run_status(path)
}

pub(super) fn run_preflight(path: std::path::PathBuf, id: String) -> i32 {
    handlers_preflight::run_preflight(path, id)
}

/// ISS-234: `ishoo search-issues <query>` — concept search over the issue store by
/// meaning, not literal words. Read-only; prints a ranked list. Requires the
/// `semantic-search` build feature, enabled by default for installed binaries
/// (custom `--no-default-features` builds fall back to keyword search).
pub(super) fn run_search_issues(path: std::path::PathBuf, query: String, top: usize) -> i32 {
    match model::issue_search::search_issues_with_status(&path, &query, top) {
        Ok(result) if result.matches.is_empty() => {
            if let model::issue_search::SearchMode::LexicalFallback { reason } = &result.mode {
                eprintln!(
                    "note: semantic issue search fell back to simple keyword matching: {reason}"
                );
            }
            match result.cache_state {
                model::issue_search::SearchCacheState::Ready => {
                    if matches!(
                        &result.mode,
                        model::issue_search::SearchMode::LexicalFallback { .. }
                    ) {
                        println!("No keyword fallback matches.");
                    } else {
                        println!("No issues to search.");
                    }
                }
                model::issue_search::SearchCacheState::Warming {
                    cached,
                    missing,
                    total,
                } => {
                    if matches!(
                        &result.mode,
                        model::issue_search::SearchMode::LexicalFallback { .. }
                    ) {
                        println!(
                            "No keyword fallback matches. Embeddings are warming in the background (cached {cached}/{total}, missing {missing})."
                        );
                    } else {
                        println!(
                            "No cached semantic matches yet. Embeddings are warming in the background (cached {cached}/{total}, missing {missing})."
                        );
                    }
                }
                model::issue_search::SearchCacheState::WarmStalled {
                    cached,
                    missing,
                    total,
                    error,
                } => {
                    println!(
                        "No keyword fallback matches. The background embedding warm has stalled (last error: {error}); cached {cached}/{total} ({missing} missing). Concept search is degraded until this is resolved."
                    );
                }
            }
            0
        }
        Ok(result) => {
            if let model::issue_search::SearchMode::LexicalFallback { reason } = &result.mode {
                eprintln!(
                    "note: semantic issue search fell back to simple keyword matching: {reason}"
                );
            }
            if let model::issue_search::SearchCacheState::Warming {
                cached,
                missing,
                total,
            } = &result.cache_state
            {
                eprintln!(
                    "note: semantic issue embeddings are warming in the background (cached {cached}/{total}, missing {missing}); results are partial"
                );
            }
            if let model::issue_search::SearchCacheState::WarmStalled {
                cached,
                missing,
                total,
                error,
            } = &result.cache_state
            {
                eprintln!(
                    "note: the background embedding warm has stalled (last error: {error}); cached {cached}/{total} ({missing} missing) — results use the keyword fallback and are degraded"
                );
            }
            for hit in result.matches {
                println!("{:>5.1}%  {}  {}", hit.score * 100.0, hit.id, hit.title);
            }
            0
        }
        Err(e) => {
            eprintln!("search-issues: {e}");
            1
        }
    }
}

/// ISS-235: `ishoo candidates <lens>` — the candidate-gathering step of the
/// charter→lens→sequence prioritization workflow. Composes concept recall with
/// the always-on safety/breaking anchor, drops blocked issues, and prints the
/// bounded set the agent then deep-reads and sequences. Read-only; persists
/// nothing. Degrades to the anchor backstop when concept search is unavailable.
pub(super) fn run_candidates(path: std::path::PathBuf, lens: String, top: usize) -> i32 {
    match model::candidate_gather::gather_candidates(&path, &lens, top) {
        Ok(set) => {
            if let Some(note) = &set.note {
                eprintln!("note: {note}");
            }
            println!(
                "Lens: {}\nCandidates: {} of {} live issues (concept hits: {}{})",
                set.lens,
                set.candidates.len(),
                set.total_live,
                set.concept_hits,
                if set.concept_search_ran {
                    String::new()
                } else {
                    ", concept search OFF".to_string()
                },
            );
            for c in &set.candidates {
                let score = format!("{:>5.1}%", c.rank_score * 100.0);
                let mut why = Vec::new();
                if let Some(concept_score) = c.concept_score {
                    why.push(format!("concept {:.1}%", concept_score * 100.0));
                }
                if c.by_anchor {
                    why.push("safety/breaking".to_string());
                }
                println!("{:>6}  {}  {}  [{}]", score, c.id, c.title, why.join("+"));
            }
            if !set.dropped_blocked.is_empty() {
                println!("Dropped (blocked): {}", set.dropped_blocked.join(", "));
            }
            0
        }
        Err(e) => {
            eprintln!("candidates: {e}");
            1
        }
    }
}

/// DURA-05 (DEC-54/DEC-51): `ishoo doctor` — diagnose store durability faults and,
/// with `--fix`, apply the bounded never-destroy heal. Read-only by default.
pub(super) fn run_resolve_store(path: std::path::PathBuf, side: Option<String>) -> i32 {
    use model::git_remote::{resolve_store_conflict, store_conflict_paths, ConflictSide};
    match side {
        // No side: list the conflicting records so the human can decide (FEAT-23).
        None => match store_conflict_paths(&path) {
            Ok(paths) if paths.is_empty() => {
                println!("No same-record store conflicts to resolve.");
                0
            }
            Ok(paths) => {
                println!("Same-record store conflicts ({}):", paths.len());
                for p in &paths {
                    println!("  {p}");
                }
                println!(
                    "\nResolve with:\n  \
                     ishoo resolve-store keep-mine    # keep this checkout's version of each\n  \
                     ishoo resolve-store take-remote  # take origin's version of each\n\n\
                     Inspect a record manually (opaque store blobs):\n  \
                     git show {store_ref}:<path>                      # mine\n  \
                     git show refs/ishoo/remotes/sync/store:<path>    # remote (after a fetch)",
                    store_ref = model::git_remote::STORE_REF,
                );
                0
            }
            Err(error) => {
                eprintln!("Error: {error}");
                1
            }
        },
        Some(side) => {
            let choice = match side.as_str() {
                "keep-mine" => ConflictSide::KeepMine,
                "take-remote" => ConflictSide::TakeRemote,
                "newest" => ConflictSide::Newest,
                other => {
                    eprintln!(
                        "Error: unknown side '{other}' (use keep-mine, take-remote, or newest)"
                    );
                    return 1;
                }
            };
            match resolve_store_conflict(&path, choice) {
                Ok(report) => {
                    println!("Store reconciled ({side}): {:?}", report.outcome);
                    for r in &report.resolved {
                        println!("  conflict {} -> kept {}", r.path, r.kept);
                    }
                    for b in &report.backups {
                        println!("  backup: {b}");
                    }
                    0
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    1
                }
            }
        }
    }
}

pub(super) fn run_doctor(path: std::path::PathBuf, fix: bool) -> i32 {
    let report = match model::doctor::diagnose(&path) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };

    if report.is_healthy() {
        println!("doctor: store healthy — no tracked .ishoo paths, no dangling plan refs, nothing unpublished.");
        return 0;
    }

    println!("doctor: found store durability faults:");
    if let Some(error) = &report.store_unreadable {
        println!("  store unreadable — dangling-ref scan skipped: {error}");
    }
    if !report.tracked_store_paths.is_empty() {
        println!(
            "  legacy in-tree store: {} .ishoo path(s) tracked on main (should be sidecar-only, DEC-51)",
            report.tracked_store_paths.len()
        );
        for p in &report.tracked_store_paths {
            println!("    - {p}");
        }
    }
    for d in &report.dangling_plan_refs {
        println!(
            "  dangling plan ref: plan '{}' lists '{}' but no record exists",
            d.plan, d.issue_id
        );
    }
    for a in &report.ahead {
        println!(
            "  unpublished: {} is AHEAD of {} by {} commit(s)",
            a.local_ref, a.remote_ref, a.ahead_by
        );
    }
    match report.store_drift {
        model::git_remote::StoreDrift::Behind => println!(
            "  store drift: working .ishoo is BEHIND refs/ishoo/store (a bare fetch advanced the ref past it) — reads are stale; `--fix` reconciles from the ref (SAFE-08/DURA-09)"
        ),
        model::git_remote::StoreDrift::Ahead => println!(
            "  store drift: working .ishoo holds un-snapshotted/diverged content not in refs/ishoo/store — `--fix` snapshots it to the ref"
        ),
        model::git_remote::StoreDrift::InSync | model::git_remote::StoreDrift::NoRef => {}
    }

    if !fix {
        println!("\nRun `ishoo doctor --fix` to untrack legacy paths, reconcile/re-snapshot the store, and publish.");
        return 1;
    }

    println!("\ndoctor --fix: applying bounded heal…");
    let outcome = match model::doctor::heal(&path, &report) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("Error: heal failed: {error}");
            return 1;
        }
    };
    if !outcome.untracked_paths.is_empty() {
        println!(
            "  untracked {} legacy .ishoo path(s) (working files preserved)",
            outcome.untracked_paths.len()
        );
    }
    if outcome.reconciled_from_ref {
        println!("  reconciled working store FROM refs/ishoo/store (it was behind — never re-snapshotted a stale tree)");
    }
    if outcome.resnapshotted {
        println!("  re-snapshotted store to refs/ishoo/store");
    }
    println!("  publish: {}", outcome.publish);
    if !outcome.unrecoverable_danglers.is_empty() {
        println!(
            "  unrecoverable: {} dangling plan ref(s) — lost record bodies cannot be rebuilt; resolve manually:",
            outcome.unrecoverable_danglers.len()
        );
        for d in &outcome.unrecoverable_danglers {
            println!("    - plan '{}' -> '{}'", d.plan, d.issue_id);
        }
        return 1;
    }
    println!("doctor: heal complete.");
    0
}

pub(super) fn commit_store_mutation(path: &std::path::Path, message: &str) -> Result<(), String> {
    model::git_remote::commit_store_mutation(path, message)
}

pub(super) fn run_decision(path: std::path::PathBuf, args: DecisionArgs) -> i32 {
    handlers_decisions::run_decision(path, args)
}

pub(super) fn run_milestone(path: std::path::PathBuf, args: MilestoneArgs) -> i32 {
    handlers_release::run_milestone(path, args)
}

pub(super) fn run_version(path: std::path::PathBuf, args: VersionArgs) -> i32 {
    handlers_release::run_version(path, args)
}

pub(super) fn run_charter(path: std::path::PathBuf, args: CharterArgs) -> i32 {
    handlers_charter::run_charter(path, args)
}

pub(super) fn run_epic(path: std::path::PathBuf, args: EpicArgs) -> i32 {
    handlers_structure::run_epic(path, args)
}

pub(super) fn run_roadmap(path: std::path::PathBuf, args: RoadmapArgs) -> i32 {
    handlers_structure::run_roadmap(path, args)
}

pub(super) fn run_people(path: std::path::PathBuf, args: PeopleArgs) -> i32 {
    handlers_people::run_people(path, args)
}

pub(super) fn run_comment(path: std::path::PathBuf, args: CommentArgs) -> i32 {
    match args.command {
        CommentCommand::Add {
            id,
            text,
            text_file,
            author,
        } => {
            let body = match (text, text_file) {
                (_, Some(file)) => match std::fs::read_to_string(&file) {
                    Ok(contents) => contents,
                    Err(e) => {
                        eprintln!("Error: failed to read --text-file {file}: {e}");
                        return 1;
                    }
                },
                (Some(text), None) => text,
                (None, None) => {
                    eprintln!("Error: provide comment text or --text-file");
                    return 1;
                }
            };
            let mut workspace = load_workspace(&path);
            model::cli_comment_add(&mut workspace, id, author, body);
            0
        }
        CommentCommand::List { id } => {
            let workspace = load_workspace(&path);
            model::cli_comment_list(&workspace, &id);
            0
        }
        CommentCommand::Edit {
            id,
            index,
            text,
            text_file,
        } => {
            let body = match (text, text_file) {
                (_, Some(file)) => match std::fs::read_to_string(&file) {
                    Ok(contents) => contents,
                    Err(e) => {
                        eprintln!("Error: failed to read --text-file {file}: {e}");
                        return 1;
                    }
                },
                (Some(text), None) => text,
                (None, None) => {
                    eprintln!("Error: provide comment text or --text-file");
                    return 1;
                }
            };
            let mut workspace = load_workspace(&path);
            model::cli_comment_edit(&mut workspace, id, index, body);
            0
        }
        CommentCommand::Remove { id, index } => {
            let mut workspace = load_workspace(&path);
            model::cli_comment_remove(&mut workspace, id, index);
            0
        }
    }
}

pub(super) fn run_link(path: std::path::PathBuf, args: super::LinkArgs) -> i32 {
    handlers_linked::run_link(path, args)
}

pub(super) fn run_relink(
    path: std::path::PathBuf,
    key: String,
    new_path: std::path::PathBuf,
) -> i32 {
    handlers_linked::run_relink(path, key, new_path)
}

#[cfg(test)]
pub(super) fn run_plan_queue(
    path: std::path::PathBuf,
    add: Option<String>,
    remove: Option<String>,
    next: bool,
    clear: bool,
) -> i32 {
    handlers_linked::run_plan_queue(path, add, remove, next, clear)
}

pub(super) fn run_enable(path: &std::path::Path, user: bool, remove: bool, git_auth: bool) -> i32 {
    if user {
        let code = run_user_enable(remove);
        if code != 0 || !git_auth {
            return code;
        }
        return run_git_auth(path);
    }

    let adapter_code = match model::adapters::enable_repo_adapters(path) {
        Ok(report) => {
            println!(
                "Enabled Ishoo host adapters in {}:",
                report.repo_root.display()
            );
            for (file, action) in &report.files {
                match action {
                    model::adapters::AdapterAction::Skipped(reason) => {
                        println!("  {file}: skipped ({reason})")
                    }
                    other => println!("  {file}: {}", other.tag()),
                }
            }
            println!(
                "\nRestart your agent host (or reload the repo) so it picks up the \
                 Ishoo + SEMMAP MCP servers."
            );
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    };
    if adapter_code != 0 || !git_auth {
        return adapter_code;
    }
    run_git_auth(path)
}

fn run_git_auth(path: &std::path::Path) -> i32 {
    match model::git_remote::setup_github_https_auth(path) {
        Ok(report) => {
            println!(
                "\nConfigured GitHub HTTPS publish auth in {}:",
                report.repo_root.display()
            );
            println!("  origin: {}", report.https_remote);
            println!(
                "  GitHub login: {}",
                if report.login_ran {
                    "completed"
                } else {
                    "already authenticated"
                }
            );
            println!("  git credential helper: configured by gh auth setup-git");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn run_user_enable(remove: bool) -> i32 {
    let result = if remove {
        model::adapters::remove_user_adapters()
    } else {
        model::adapters::enable_user_adapters()
    };
    match result {
        Ok(report) => {
            let action = if remove { "Removed" } else { "Enabled" };
            println!("{action} Ishoo user-scope host adapters:");
            for (file, action) in &report.files {
                match action {
                    model::adapters::AdapterAction::Skipped(reason) => {
                        println!("  {file}: skipped ({reason})")
                    }
                    other => println!("  {file}: {}", other.tag()),
                }
            }
            if !remove {
                println!(
                    "\nRestart your agent host so it picks up the user-wide \
                     Ishoo + SEMMAP MCP servers."
                );
            }
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

pub(super) fn run_init(cli: &Cli) -> i32 {
    match model::init_workspace(&cli.path) {
        Ok(created_path) => {
            println!(
                "Initialized ishoo project store in {}",
                created_path.display()
            );
            println!("Store: {}/.ishoo/ (sharded)", created_path.display());
            println!("Use the desktop UI to export markdown projections for humans.");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

pub(super) fn run_dash(path: std::path::PathBuf) -> i32 {
    ui::launch_dashboard(path);
    0
}

pub(super) fn run_list(
    path: std::path::PathBuf,
    group_by: String,
    compact: bool,
    linked: bool,
    query: model::IssueQuery,
) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_list_with_mode(
        &workspace,
        &query,
        model::SectionMode::from_str(&group_by),
        compact,
    );
    if linked {
        model::cli_linked_summary(&path);
    }
    0
}

pub(super) fn run_show(path: std::path::PathBuf, id: &str) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_show(&workspace, id);
    0
}

pub(super) fn run_labels(path: std::path::PathBuf) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_labels(&workspace);
    0
}

pub(super) fn run_files(path: std::path::PathBuf) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_files(&workspace);
    0
}

pub(super) fn run_refs(path: std::path::PathBuf, kind: model::RefListKind) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_refs(&workspace, kind);
    0
}

pub(super) fn run_new(path: std::path::PathBuf, args: super::NewArgs) -> i32 {
    handlers_new::run_new(path, args)
}

pub(super) fn run_decompose(path: std::path::PathBuf, args: super::DecomposeArgs) -> i32 {
    handlers_new::run_decompose(path, args)
}

pub(super) fn run_edit(path: std::path::PathBuf, id: String, args: model::EditArgs) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_edit(&mut workspace, &id, &args) {
        Ok(outcome) => {
            println!("Edited [{}]", outcome.id);
            // FIX-144: dropped-accepted-ADR advisories go to stderr, never silent.
            for warning in &outcome.warnings {
                eprintln!("Warning: {warning}");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_decline(path: std::path::PathBuf, id: String, reason: String) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_decline(&mut workspace, &id, &reason) {
        Ok(()) => {
            println!("Declined [{id}]");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_shelve(path: std::path::PathBuf, id: String, reason: String) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_shelve(&mut workspace, &id, &reason) {
        Ok(()) => {
            println!("Shelved [{id}]");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_supersede(
    path: std::path::PathBuf,
    id: String,
    replacement: String,
    reason: String,
) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_supersede(&mut workspace, &id, &replacement, &reason) {
        Ok(()) => {
            println!("Superseded [{id}] by [{replacement}]");
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_move(path: std::path::PathBuf, args: MoveArgs) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_move(&mut workspace, &args.query, &args.ids, &args.to) {
        Ok(moved) => {
            println!("Moved {moved} issue(s) to {}", args.to);
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_plan(path: std::path::PathBuf, command: PlanCommand) -> i32 {
    match command {
        PlanCommand::Show => {
            model::cli_plan_show(&path);
            0
        }
        PlanCommand::Add { id, after, before } => {
            let result = match (after, before) {
                (Some(anchor), _) => model::cli_plan_add_at(&path, &id, &anchor, true),
                (_, Some(anchor)) => model::cli_plan_add_at(&path, &id, &anchor, false),
                (None, None) => model::cli_plan_add(&path, &id),
            };
            match result {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        PlanCommand::Move { id, after, before } => {
            let result = match (after, before) {
                (Some(anchor), _) => model::cli_plan_move(&path, &id, &anchor, true),
                (_, Some(anchor)) => model::cli_plan_move(&path, &id, &anchor, false),
                (None, None) => Err("specify --after <id> or --before <id>".to_string()),
            };
            match result {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        PlanCommand::Remove { id } => match model::cli_plan_remove(&path, &id) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Set { ids } => match model::cli_plan_set(&path, &ids) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Clear => {
            model::cli_plan_clear(&path);
            0
        }
        PlanCommand::Next => {
            model::cli_plan_next(&path);
            0
        }
        PlanCommand::Populate { labels } => {
            match model::cli_plan_populate(&path, labels.as_deref()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        PlanCommand::Generate { kind } => run_generate_plan(path, &kind),
        PlanCommand::LegacyRebalance => run_generate_plan(path, "rebalance"),
        PlanCommand::NewPlan { name } => match model::cli_plan_new_plan(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::ListPlans { all } => {
            model::cli_plan_list_plans(&path, all);
            0
        }
        PlanCommand::Milestone { plan, milestone } => {
            match model::cli_plan_milestone(&path, &plan, milestone.as_deref()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            }
        }
        PlanCommand::Archive { name } => match model::cli_plan_archive(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Delete { name } => match model::cli_plan_delete_empty(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Use { name } => match model::cli_plan_use(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Deactivate => match model::cli_plan_deactivate(&path) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::Rename { name } => match model::cli_plan_rename(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
        PlanCommand::DropPlan { name } => match model::cli_plan_drop_plan(&path, &name) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        },
    }
}

fn run_generate_plan(path: std::path::PathBuf, kind: &str) -> i32 {
    let workspace = load_workspace(&path);
    let kind = match model::PlanKind::from_str(kind) {
        Ok(kind) => kind,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };

    match model::cli_generate_plan(&workspace, kind) {
        Ok(plan) => {
            print!("{plan}");
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

pub(super) fn run_apply(path: std::path::PathBuf, plan: std::path::PathBuf, dry_run: bool) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_apply(&mut workspace, &plan, dry_run) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_batch(
    path: std::path::PathBuf,
    file: Option<std::path::PathBuf>,
    dry_run: bool,
) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_batch(&mut workspace, file.as_deref(), dry_run) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_lint(path: std::path::PathBuf, strict: bool) -> i32 {
    match model::cli_lint(&path, strict) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

pub(super) fn run_rename_id(path: std::path::PathBuf, old_id: String, new_id: String) -> i32 {
    let mut workspace = load_workspace(&path);
    if let Err(e) = model::rename_issue_id(&mut workspace.issues, &old_id, &new_id) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = workspace.save() {
        eprintln!("Error: {e}");
        return 1;
    }
    // Update Plan entries that reference old_id under the local project key.
    let mut plan = model::Plan::load(&path);
    let mut plan_changed = false;
    for entry in plan.entries.iter_mut() {
        if entry.project_key == "local" && entry.issue_id == old_id {
            entry.issue_id.clone_from(&new_id);
            plan_changed = true;
        }
    }
    if plan_changed {
        if let Err(e) = plan.save(&path) {
            eprintln!("Warning: issues renamed but plan save failed: {e}");
        }
    }
    println!("Renamed {old_id} → {new_id}");
    0
}

/// CLI-47: print a notice naming any change to which issue is active, including
/// the single-active demotion of others. Prints nothing when the active set is
/// unchanged.
fn print_active_transition(prev_active: &[String], issues: &[model::Issue]) {
    let now_active: Vec<String> = issues
        .iter()
        .filter(|i| i.status == model::Status::Active)
        .map(|i| i.id.clone())
        .collect();
    let demoted: Vec<String> = prev_active
        .iter()
        .filter(|p| !now_active.contains(p))
        .cloned()
        .collect();
    let gained = now_active.iter().any(|n| !prev_active.contains(n));
    if !gained && demoted.is_empty() {
        return;
    }
    let active_str = if now_active.is_empty() {
        "none".to_string()
    } else {
        now_active.join(", ")
    };
    if demoted.is_empty() {
        println!("Active issue is now: {active_str}.");
    } else {
        println!(
            "Active issue is now: {active_str} (demoted from active: {}).",
            demoted.join(", ")
        );
    }
}

pub(super) fn run_set(path: std::path::PathBuf, id: String, status: String) -> i32 {
    let mut workspace = load_workspace(&path);
    let is_active = status.trim().eq_ignore_ascii_case("active");

    let parsed_status = match model::parse_cli_status(status.trim()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if parsed_status == model::Status::Done {
        eprintln!("Error: Use `ishoo land {id}` to mark an issue done once it is accepted into the main branch");
        return 1;
    }

    // Begin gates (only when transitioning to active): the Scope Contract must be
    // complete (AUD-04), and a planned issue must target its plan's ready front
    // (DEC-40), the same as `start`.
    if parsed_status == model::Status::Active {
        if !scope_contract_allows_begin(&workspace.issues, &id) {
            return 1;
        }
        if !plan_order_allows_begin(&path, &id) {
            return 1;
        }
    }

    let base_commit = if is_active { get_head_sha(&path) } else { None };

    // CLI-47: capture the active set before the change so we can report any
    // transition (including the single-active demotion of other issues).
    let prev_active: Vec<String> = workspace
        .issues
        .iter()
        .filter(|i| i.status == model::Status::Active)
        .map(|i| i.id.clone())
        .collect();

    if let Err(e) = model::update_issue_by_id(
        &mut workspace.issues,
        &id,
        &model::IssuePatch {
            status: Some(parsed_status),
            base_commit,
            ..model::IssuePatch::default()
        },
    ) {
        eprintln!("Error: {e}");
        return 1;
    }

    match workspace.save() {
        Ok(()) => {
            println!("Edited [{id}]");
            print_active_transition(&prev_active, &workspace.issues);
            if is_active {
                if let Err(e) = model::save_explicit_current_focus_issue_id(&path, Some(&id)) {
                    eprintln!("Warning: set active succeeded but could not set current focus: {e}");
                }
                if let Some(issue) = workspace.issues.iter().find(|issue| issue.id == id) {
                    if let Some(card) =
                        model::governing_decisions_card_for_issue(&workspace.decisions, issue)
                    {
                        println!("\n{card}");
                    }
                } else if let Some(card) = model::governing_decisions_card(&workspace.decisions) {
                    println!("\n{card}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn get_head_sha(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Count of changed files that are not tests, for the land trust surface
/// (HOME-01). Language-general heuristic (DEC-37), not a Rust-only check.
fn non_test_file_count(files: &[String]) -> usize {
    files
        .iter()
        .filter(|path| {
            let p = path.replace('\\', "/");
            let stem = p.rsplit('/').next().unwrap_or(&p);
            let is_test = p.contains("/tests/")
                || p.contains("/test/")
                || stem.starts_with("test_")
                || stem.ends_with("_test.rs")
                || stem.ends_with("_tests.rs")
                || p.ends_with(".test.js")
                || p.ends_with(".test.ts")
                || p.ends_with(".spec.js")
                || p.ends_with(".spec.ts");
            !is_test
        })
        .count()
}

/// Returns `true` if any tracked, non-store file is modified or staged.
///
/// Ishoo's own canonical store (`.ishoo/`) is excluded: ishoo mutates it as part
/// of normal operation (`set active`, `edit --resolution`, and the land-time
/// done mutation itself), so it is always dirty at land time. The DEC-22 block is
/// about unreviewed developer *code*, not ishoo's bookkeeping.
fn has_dirty_tracked_files(repo_root: &std::path::Path) -> bool {
    // MCP-06: the dirty-tree check is now a shared core fn so the CLI land path
    // and the MCP transition tools agree byte-for-byte on what "dirty" means.
    model::git_remote::has_dirty_tracked_tree(repo_root)
}

/// CLI-63 / DEC-33 / DEC-37: untracked (non-ignored) source files in any
/// supported language. These would make a committed tree differ from the
/// validated working tree — e.g. `git add -u` / `commit -am` drops a new file a
/// committed file needs, landing a non-compiling HEAD. `ls-files --others`
/// expands new directories to individual files, so a whole new module dir is
/// caught too; source-vs-other classification is the shared, language-general
/// `lang::is_source_file` (never a Rust-only `.rs` filter).
fn untracked_source_files(repo_root: &std::path::Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "ls-files",
            "--others",
            "--exclude-standard",
        ])
        .output();
    let Ok(out) = output else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|p| model::is_source_file(p))
        .map(String::from)
        .collect()
}

pub(super) fn run_delete(path: std::path::PathBuf, id: String, force: bool) -> i32 {
    let mut workspace = load_workspace(&path);
    match model::cli_delete(&mut workspace, &id, force) {
        Ok(()) => {
            // CLI-66: if the issue was actually deleted (cli_delete returns Ok
            // even on a cancelled confirmation), prune it from every plan so no
            // plan entry dangles as a phantom card referencing a gone issue.
            if !workspace.issues.iter().any(|issue| issue.id == id) {
                match model::AllPlans::prune_issue_everywhere(&path, "local", &id) {
                    Ok(n) if n > 0 => {
                        let plural = if n == 1 { "entry" } else { "entries" };
                        println!("Pruned {n} plan {plural} referencing {id}.");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Warning: deleted {id} but could not prune it from plan(s): {e}")
                    }
                }
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub(super) fn run_split_or_archive(
    path: std::path::PathBuf,
    args: MoveArgs,
    archive_done_by_default: bool,
) -> i32 {
    let mut workspace = load_workspace(&path);
    let mut query = args.query.clone();
    if archive_done_by_default && query.statuses.is_empty() {
        query.statuses = vec![model::Status::Done];
    }
    match model::cli_move(&mut workspace, &query, &args.ids, &args.to) {
        Ok(moved) => {
            if archive_done_by_default {
                println!("Archived {moved} issue(s) to {}", args.to);
            } else {
                println!("Moved {moved} issue(s) to {}", args.to);
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// Staleness threshold used by both `run_start` and `run_reclaim`: 1 hour.
const STALE_THRESHOLD_SECS: u64 = model::git_remote::CLAIM_STALE_THRESHOLD_SECS;

/// Surface a claim-propagation outcome (DEC-21 push-on-claim made offline-safe,
/// DEC-32). Silent on success and on the solo/no-remote path — the solo dev pays
/// nothing and sees no noise; only a deferred push (a remote exists but the push
/// did not land) prints a notice so the user knows the claim has not yet synced.
fn report_claim_push(outcome: model::git_remote::PushOutcome, id: &str) {
    if let model::git_remote::PushOutcome::Deferred(err) = outcome {
        eprintln!(
            "Notice: {id} recorded locally but not yet synced to the remote ({err}). \
             It will publish on the next successful push."
        );
    }
}

/// Begin gate (DEC-40/DEC-44, relaxed): an issue may be begun (started or set
/// active) unless one of its declared dependencies is still unfinished. Plan
/// order is advisory — `plan next` recommends the derived front, but any
/// unblocked issue may begin out of order. (DEC-42 enrollment now happens at
/// filing time, so this gate no longer needs to enroll orphans.) Returns true if
/// `id` may begin; prints the rejection naming the blockers otherwise. An unknown
/// id passes here — existence is the caller's concern.
fn plan_order_allows_begin(path: &std::path::Path, id: &str) -> bool {
    let workspace = load_workspace(path);
    let Some(issue) = workspace.issues.iter().find(|i| i.id == id) else {
        return true;
    };
    let blockers = model::plan::unresolved_plan_dependencies("local", issue, |_key, dep_id| {
        workspace
            .issues
            .iter()
            .find(|i| i.id == dep_id)
            .map(|i| i.status)
    });
    if !blockers.is_empty() {
        eprintln!(
            "Cannot begin {id}: blocked by unfinished {}: {}.\n\
             Finish the blocker(s) first, or drop the edge with `ishoo edit {id} --depends-on <ids|none>`.",
            if blockers.len() == 1 {
                "dependency"
            } else {
                "dependencies"
            },
            blockers.join(", "),
        );
        return false;
    }

    // DEC-42 self-heal: an issue in no plan is enrolled into Backlog on begin, so
    // no orphan stays a free agent. (Filing enrolls too; this catches legacy data.)
    let mut all = model::AllPlans::load(path);
    let in_a_plan = all
        .named
        .iter()
        .any(|p| p.plan.entries.iter().any(|e| e.issue_id == id))
        || all.default_plan.entries.iter().any(|e| e.issue_id == id);
    if !in_a_plan {
        all.default_plan.add("local".to_string(), id.to_string());
        if let Err(e) = all.save(path) {
            eprintln!("Warning: could not enroll {id} into Backlog: {e}");
        }
    }
    true
}

/// Scope Contract gate (AUD-04, DEC-48): an issue may be begun (started or set
/// active) only when its description carries a complete Scope Contract — the same
/// four fields `ishoo land` enforces as the backstop. The brief requires the
/// contract before work begins ("Before code changes… ensure Scope Contract");
/// set-active previously only *warned*, so a contract-less issue could be
/// activated (the UI-123 class of "rendered surface implies a gate that isn't
/// there") and only hit the wall at land. Returns true if `id` may begin; prints
/// the rejection naming the missing sections and returns false otherwise. An
/// unknown id passes here — existence is the caller's concern.
fn scope_contract_allows_begin(issues: &[model::Issue], id: &str) -> bool {
    let Some(issue) = issues.iter().find(|i| i.id == id) else {
        return true;
    };
    let scope = model::validate_scope_contract(&issue.description);
    if scope.complete {
        return true;
    }
    let missing_list = model::missing_fields_report(&issue.description, &scope.missing);
    eprintln!(
        "Cannot begin {id}: incomplete Scope Contract.\n\
         Missing:\n{missing_list}\n\n\
         Fill it before starting work:\n  ishoo edit {id} --description-file <file>"
    );
    false
}

pub(super) fn run_start(path: std::path::PathBuf, id: String) -> i32 {
    // 1. Validate the issue exists in the canonical store.
    let workspace = load_workspace(&path);
    if !workspace.issues.iter().any(|i| i.id == id) {
        eprintln!("Error: issue {id} not found");
        return 1;
    }

    // 2. Locate the git repository root (execution substrate lives here).
    let repo_root = match model::git_remote::find_repo_root(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // 3. Inspect any existing claim and decide: new run, attach, or reject.
    let existing = match model::git_remote::inspect_claim(&repo_root, &id) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let is_attach = existing.is_some();
    let local_host = model::git_remote::local_hostname();

    match &existing {
        Some(info) if info.hostname == local_host => {
            // Our own claim — refresh the lease and attach to the existing run.
            match model::git_remote::refresh_claim(&repo_root, &id) {
                Ok(outcome) => report_claim_push(outcome, &id),
                Err(e) => {
                    eprintln!("Error refreshing claim: {e}");
                    return 1;
                }
            }
        }
        Some(info) if model::git_remote::is_claim_stale(info, STALE_THRESHOLD_SECS) => {
            eprintln!(
                "Error: {id} has a stale claim from {}. \
                 Run `ishoo reclaim {id}` to take it over.",
                info.hostname
            );
            return 1;
        }
        Some(info) => {
            eprintln!(
                "Error: {id} is actively claimed by {}. \
                 Wait for them to finish, or run `ishoo reclaim {id}` once the claim is stale.",
                info.hostname
            );
            return 1;
        }
        None => {
            // Begin gates (DEC-40 plan order + AUD-04 Scope Contract): beginning
            // fresh work must target the plan's ready front and carry a complete
            // Scope Contract. Attach/refresh of an existing claim above is never
            // gated — resuming in-progress work is always allowed.
            if !scope_contract_allows_begin(&workspace.issues, &id) {
                return 1;
            }
            if !plan_order_allows_begin(&path, &id) {
                return 1;
            }
            // No existing claim — create an exclusive one.
            match model::git_remote::create_claim(&repo_root, &id) {
                Ok(outcome) => report_claim_push(outcome, &id),
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            }
        }
    }

    // 4. Create or attach to the isolated worktree.
    let wt = match model::git_remote::create_worktree(&repo_root, &id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            // Only release the claim if we just created it (not on attach).
            if !is_attach {
                let _ = model::git_remote::release_claim(&repo_root, &id);
            }
            return 1;
        }
    };

    // 5. Persist start-time issue state in one local store write, no push (DEC-21).
    let mut assigned_owner: Option<String> = None;
    {
        let mut start_ws = load_workspace(&path);
        // CLI-39: on a fresh claim only, record base_commit = HEAD so the scope
        // gate and preflight operate after `start` (mirrors `set active`;
        // DEC-15/DEC-38). Durable status is unchanged; attach keeps its base.
        if !is_attach {
            if let Err(e) = model::update_issue_by_id(
                &mut start_ws.issues,
                &id,
                &model::IssuePatch {
                    base_commit: get_head_sha(&path),
                    // Record the begin time so the Home feed can show how long the
                    // run took (HOME-05). First-begin only; never overwritten.
                    started_at: Some(model::current_recorded_at()),
                    ..model::IssuePatch::default()
                },
            ) {
                eprintln!("Warning: claim created but could not record base commit: {e}");
            }
        }
        // EXEC-08 / DEC-14: auto-assign the machine-local current person as owner
        // when the issue is unowned. Only treat it as assigned if it actually
        // sticks — a current person that is no longer registered fails validation,
        // and we must not report an owner we did not set.
        let unowned = start_ws
            .issues
            .iter()
            .find(|i| i.id == id)
            .is_some_and(|i| i.owner_id.is_none());
        let owner = match model::load_current_person(&path).filter(|_| unowned) {
            Some(person) if model::set_issue_owner(&mut start_ws, &id, Some(&person)).is_ok() => {
                Some(person)
            }
            _ => None,
        };
        // Skip the write only when there is nothing to persist (attach with no
        // owner change); otherwise commit base_commit and/or the owner together.
        if !is_attach || owner.is_some() {
            match start_ws.save() {
                Ok(()) => assigned_owner = owner,
                Err(e) => {
                    eprintln!("Warning: claim created but could not persist start state: {e}")
                }
            }
        }
    }

    // 6. Set canonical current focus so the UI has one deterministic "now" slot.
    if let Err(e) = model::save_explicit_current_focus_issue_id(&path, Some(&id)) {
        eprintln!("Warning: claim created but could not set current focus: {e}");
    }

    // 7. Agent-friendly structured output.
    if is_attach {
        println!("Attached {id}");
    } else {
        println!("Started {id}");
    }
    println!("  claim_ref={}", model::git_remote::claim_ref_name(&id));
    println!("  branch={}", model::git_remote::execution_branch_name(&id));
    println!("  worktree={}", wt.display());
    if let Some(owner) = &assigned_owner {
        println!("  owner={owner}");
    }
    if let Some(issue) = workspace.issues.iter().find(|issue| issue.id == id) {
        if let Some(card) = model::governing_decisions_card_for_issue(&workspace.decisions, issue) {
            println!("\n{card}");
        }
    } else if let Some(card) = model::governing_decisions_card(&workspace.decisions) {
        println!("\n{card}");
    }
    0
}

pub(super) fn run_finish(path: std::path::PathBuf, id: String) -> i32 {
    // The teardown sequence lives in the shared core (model::gates::finish) so the
    // CLI and the ishoo_finish MCP tool never drift; this wrapper renders it.
    match model::gates::finish(&path, &id) {
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
        Ok(verdict) if verdict.blocked => {
            eprintln!("Error: {}", verdict.reasons.join("; "));
            1
        }
        Ok(verdict) => {
            if verdict.claim_push == "deferred" {
                eprintln!(
                    "Notice: {id} recorded locally but not yet synced to the remote. \
                     It will publish on the next successful push."
                );
            }
            println!("Finished {id}. Claim released, worktree removed.");
            0
        }
    }
}

/// `ishoo gc` — the DEC-35 crash-only backstop. Sweeps orphan worktrees, stale
/// claim refs, and merged orphan execution branches; reports unmerged orphans
/// without deleting them.
/// `ishoo migrate-stores` (ARCH-04, DEC-62/DEC-77): convert this project's store —
/// or, with `--all`, every project in the Library — to the additive-safe v3 wire
/// format. The flip is irreversible (older binaries refuse a v3 store), so it is
/// human-gated: a bare invocation only PREVIEWS the targets and the warning; the
/// conversion runs only with `--yes`.
pub(super) fn run_migrate_stores(path: std::path::PathBuf, all: bool, yes: bool) -> i32 {
    let targets: Vec<std::path::PathBuf> = if all {
        match model::project_registry::registry_path() {
            Some(file) => model::project_registry::ProjectRegistry::load(&file)
                .list()
                .into_iter()
                .map(|entry| entry.path)
                .collect(),
            None => {
                eprintln!("Error: could not resolve the Library registry path.");
                return 1;
            }
        }
    } else {
        vec![path]
    };

    if targets.is_empty() {
        println!("migrate-stores: no projects to migrate.");
        return 0;
    }

    if !yes {
        println!(
            "migrate-stores: PREVIEW — {} project(s) would be converted to the v3 format:",
            targets.len()
        );
        for t in &targets {
            println!("  - {}", t.display());
        }
        println!(
            "\nThis is a ONE-WAY flip. Once a store is v3, an OLDER Ishoo binary will refuse to"
        );
        println!("read it — so upgrade Ishoo on EVERY machine that shares these stores first.");
        println!("\nNothing was changed. Re-run with `--yes` to perform the conversion.");
        return 0;
    }

    let (mut migrated, mut already, mut failed) = (0usize, 0usize, 0usize);
    for t in &targets {
        match model::project_store::ProjectStore::migrate_to_v3(t) {
            Ok(true) => {
                migrated += 1;
                println!("  migrated → v3: {}", t.display());
            }
            Ok(false) => {
                already += 1;
                println!("  already v3:    {}", t.display());
            }
            Err(error) => {
                failed += 1;
                eprintln!("  FAILED:        {} — {error}", t.display());
            }
        }
    }
    println!("\nmigrate-stores: {migrated} migrated, {already} already v3, {failed} failed.");
    i32::from(failed > 0)
}

pub(super) fn run_gc(path: std::path::PathBuf) -> i32 {
    let repo_root = match model::git_remote::find_repo_root(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let report = match model::git_remote::gc(&repo_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if report.is_empty() {
        println!("Nothing to clean up.");
        return 0;
    }
    for id in &report.removed_worktrees {
        println!("Removed orphan worktree: {id}");
    }
    for id in &report.removed_claims {
        println!("Removed stale claim ref: {id}");
    }
    for branch in &report.removed_branches {
        println!("Deleted merged orphan branch: {branch}");
    }
    for branch in &report.kept_unmerged_branches {
        println!("Kept unmerged orphan branch (not deleted; remove by hand if intended): {branch}");
    }
    0
}

/// The two completion paths share one core (gates -> mark done -> teardown) and
/// differ only in surface today (DEC-13/38). `Done` is the default worktree path
/// (`start` -> `done`); `Land` is the shared-tree self-commit path
/// (`set active` -> `land`). GIT-02 makes `Done` auto-commit the worktree where
/// `Land` keeps the DEC-22 dirty-tree block — this enum is that seam.
#[derive(Clone, Copy)]
pub(super) enum CompletionVerb {
    Done,
    Land,
}

impl CompletionVerb {
    /// The command name to use in user-facing retry hints.
    fn cmd(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Land => "land",
        }
    }

    /// Past-tense word for the success line.
    fn past(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Land => "Landed",
        }
    }
}

/// `ishoo done <id>` — the default completion verb (DEC-13). Worktree path:
/// runs the gates, marks the issue done, tears down the execution substrate.
/// (GIT-02 adds the auto-commit; GIT-03/04 the integrate+push.)
pub(super) fn run_done(path: std::path::PathBuf, id: String) -> i32 {
    run_completion(path, id, CompletionVerb::Done)
}

/// `ishoo land <id>` — the shared-tree self-commit completion (DEC-38). Keeps
/// the DEC-22 dirty-tree block; the developer commits by hand first.
pub(super) fn run_land(path: std::path::PathBuf, id: String) -> i32 {
    run_completion(path, id, CompletionVerb::Land)
}

/// Print the CLI presentation card (DEC-16) for a blocked completion. The shared
/// gate core ([`model::gates::run_completion_gates`]) measured the cause; the CLI
/// renders it. Mirrors the MCP surface, which returns the same block as a typed
/// `LandVerdict` instead of printing.
fn print_completion_block(id: &str, cmd: &str, block: &model::gates::CompletionBlock) {
    use model::gates::CompletionBlockCause;
    match &block.cause {
        CompletionBlockCause::ScopeTooLarge => {
            if let Some(scope) = &block.scope {
                eprintln!(
                    "Scope too large to land [{id}]: {n_files} files across {n_sub} subsystems \
                    (limit: 6 subsystems or 12 files).\n\
                    \n\
                    Split it: decompose into focused sub-issues, each touching 1-3 subsystems.\n\
                    `decompose` records parent->child lineage (DEC-61) so the split stays traceable \
                    and the children inherit this issue's plan:\n\
                    \n\
                      ishoo decompose {id} \"<sub-issue title>\" --category {cat} --description-file <file> --decisions <DEC-ids|none> --depends-on <ids|none>\n\
                      ishoo start <NEW-ID>\n\
                      # make the focused change, resolve, then:\n\
                      ishoo {cmd} <NEW-ID>\n\
                    \n\
                    Subsystems touched ({n_sub}): {}\n\
                    Files changed ({n_files}): {}",
                    scope.subsystems.join(", "),
                    scope.files.join(", "),
                    cat = id.split('-').next().unwrap_or("CAT"),
                    n_sub = scope.subsystem_count,
                    n_files = scope.file_count,
                );
            }
        }
        CompletionBlockCause::Diverged => {
            // The gate passed before integration diverged; announce it, then the block.
            if let Some(label) = &block.correctness_gate {
                print_gate_outcome(id, label);
            }
            let branch = model::git_remote::execution_branch_name(id);
            eprintln!(
                "Blocked [{id}]: the default branch advanced past this issue's base, so a \
                 clean fast-forward is impossible. The verified commit is on {branch}; \
                 rebase it onto the default branch and {cmd} again.\n\
                 The issue stays ACTIVE and its worktree is preserved — nothing was marked \
                 done or torn down."
            );
        }
        CompletionBlockCause::IntegrateError(e) => {
            if let Some(label) = &block.correctness_gate {
                print_gate_outcome(id, label);
            }
            eprintln!(
                "Blocked [{id}]: could not integrate into the default branch: {e}\n\
                 The issue stays ACTIVE and its worktree/commit are preserved — fix the \
                 cause and {cmd} again. Nothing was marked done or torn down."
            );
        }
        CompletionBlockCause::Structural => {
            // The CLI hard-blocks dirty/untracked trees before the core and does not
            // gate contracts, so it never reaches here; render the reasons defensively.
            for reason in &block.reasons {
                eprintln!("Blocked [{id}]: {reason}");
            }
        }
    }
}

/// DEC-70: the control surface runs no build/test gate, so the only thing to report
/// is the advisory note (carried in `label`) pointing at the issue's Verification
/// contract. No "passed"/"skipped" language — Ishoo measured nothing to pass.
fn print_gate_outcome(id: &str, label: &str) {
    eprintln!("[{id}] verification: {label}");
}

fn run_completion(path: std::path::PathBuf, id: String, verb: CompletionVerb) -> i32 {
    let cmd = verb.cmd();
    let mut workspace = load_workspace(&path);
    // Whether the GIT-02 done-commit actually created a commit (vs a clean no-code
    // tree). Used by the shared core to roll the commit back on a gate failure.
    let mut did_commit = false;

    let repo_root = model::git_remote::find_repo_root(&path).ok();
    // The worktree `done` path: a live execution worktree (created by `start`)
    // exists and the verb is `done`. `land`, and a `done` with no worktree, keep the
    // DEC-22 shared-tree semantics (commit by hand first), so they pass no worktree
    // to the core and skip the auto-commit + integrate steps.
    let worktree = repo_root.as_ref().and_then(|root| {
        let wt = model::git_remote::worktree_path(root, &id);
        (matches!(verb, CompletionVerb::Done) && wt.exists()).then_some(wt)
    });
    let work_dir = match (worktree.as_ref(), repo_root.as_ref()) {
        (Some(wt), _) => Some(wt.clone()),
        (None, Some(root)) => Some(root.clone()),
        (None, None) => None,
    };

    if let Some(root) = repo_root.as_ref() {
        if let Some(wt) = worktree.as_ref() {
            // GIT-02 / DEC-38: `done` stages and commits the worktree's source tree
            // itself — replacing the interim DEC-22 dirty-tree block — BEFORE the
            // gates, so the scope check measures `base..HEAD` exactly; the core rolls
            // it back on any gate failure. A clean worktree commits nothing and still
            // lands. `.ishoo/` is gitignored, so the store is never swept in.
            let message = match workspace.issues.iter().find(|i| i.id == id) {
                Some(issue) => model::synthesize_commit_message(issue, None),
                None => {
                    eprintln!("Error: issue {id} not found");
                    return 1;
                }
            };
            match model::git_remote::commit_worktree_all(wt, &message) {
                Ok(committed) => did_commit = committed,
                Err(e) => {
                    eprintln!("Error [{id}]: could not create the done-commit: {e}");
                    return 1;
                }
            }
        } else {
            // DEC-22 / ADR-014: land must not auto-commit. Block on a dirty tracked
            // tree so no unreviewed code is swept into a commit and the scope check
            // measures exactly the committed work. The developer commits or stashes
            // first. (ishoo's own .ishoo/ store is excluded — see has_dirty_tracked_files.)
            if has_dirty_tracked_files(root) {
                eprintln!(
                    "Uncommitted changes detected. Commit or stash before landing.\n\n\
                     ishoo {cmd} does not commit your work — commit it yourself with a real\n\
                     message, then {cmd}:\n\
                     \n  git add -A && git commit -m \"<what changed>\"\n  ishoo {cmd} {id}"
                );
                return 1;
            }

            // CLI-63 / DEC-33: block on untracked source files so the committed tree
            // equals the tree the correctness gate validates. (Amends the DEC-22
            // check, which only sees tracked changes.)
            let untracked = untracked_source_files(root);
            if !untracked.is_empty() {
                eprintln!(
                    "Untracked source files detected — the committed tree would not match the\n\
                     validated tree, so land could mark this done on a non-compiling HEAD.\n\
                     Stage them, then commit, so the landed commit equals what the gate checks:\n\
                     \n  git add -A && git commit -m \"<what changed>\"\n  ishoo {cmd} {id}\n\
                     \nUntracked source files:\n  {}",
                    untracked.join("\n  ")
                );
                return 1;
            }
        }
    }

    let base_commit = workspace
        .issues
        .iter()
        .find(|i| i.id == id)
        .and_then(|issue| issue.base_commit.clone());

    // REFA-01: the scope-size / correctness / integration sequence runs through the
    // shared core in `model::gates` — the same code the MCP `ishoo_done` uses — so a
    // gate fix lands on both surfaces at once (this duplicated sequence is why
    // TEST-02's divergence gap could exist). The CLI hard-blocks dirty/untracked trees
    // above and does not gate contract completeness, so it passes `dirty_tree=false`
    // and no structural reasons; it keeps only its own presentation + bookkeeping.
    let measured = match model::gates::run_completion_gates(
        repo_root.as_deref(),
        worktree.as_deref(),
        work_dir.as_deref(),
        base_commit.as_deref(),
        &id,
        did_commit,
        false,
        Vec::new(),
        Some(path.as_path()),
    ) {
        model::gates::CompletionGateOutcome::Blocked(block) => {
            print_completion_block(&id, cmd, &block);
            return 1;
        }
        model::gates::CompletionGateOutcome::Ready(measured) => measured,
    };

    // CLI presentation of the cleared gates (DEC-16): the core measured, the CLI
    // renders. A WARN scope is advisory (proceeds); the gate outcome and a
    // fast-forward integration are announced.
    if let Some(scope) = &measured.scope {
        if scope.verdict == "WARN" {
            eprintln!(
                "Warning [{id}]: diff spans {n_sub} subsystems and {n_files} files — consider splitting before landing.\n\
                \n\
                Subsystems touched ({n_sub}): {}\n\
                Files changed ({n_files}): {}",
                scope.subsystems.join(", "),
                scope.files.join(", "),
                n_sub = scope.subsystem_count,
                n_files = scope.file_count,
            );
        }
    }
    if let Some(label) = &measured.correctness_gate {
        print_gate_outcome(&id, label);
    }
    if let Some(integration) = &measured.integration {
        if integration.starts_with("fast-forwarded") {
            println!("Integrated {id}: {integration}.");
        }
    }

    // Record what this completion measured, so the Home trust surface can render
    // provably-true facts (HOME-01, DEC-36). Unmeasured = not written.
    if let Some(issue) = workspace.issues.iter_mut().find(|i| i.id == id) {
        if let Some(scope) = &measured.scope {
            issue.set_land_fact("scope", scope.verdict.to_string());
            issue.set_land_fact("files", non_test_file_count(&scope.files).to_string());
        }
        // DEC-70: Ishoo no longer measures build/test verification, so no "gate" or
        // "tests" land-fact is recorded — the Home trust surface shows only measured
        // facts (DEC-36), and correctness now lives in the Verification contract.
    }

    if let Err(e) = model::land_issue_by_id(&mut workspace.issues, &id) {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = workspace.save() {
        eprintln!("Error: {e}");
        return 1;
    }
    if let Err(e) = model::clear_explicit_current_focus_issue_id_if_matches(&path, &id) {
        eprintln!("Warning: done issue but could not clear current focus: {e}");
    }

    // Surface the recorded trust-surface facts (HOME-01).
    if let Some(issue) = workspace.issues.iter().find(|i| i.id == id) {
        let facts: Vec<String> = ["files", "scope", "gate", "tests"]
            .iter()
            .filter_map(|key| issue.land_fact(key).map(|val| format!("{key}={val}")))
            .collect();
        if !facts.is_empty() {
            println!("Recorded land facts: {}", facts.join(" "));
        }
    }

    let mut plan = model::Plan::load(&path);
    let plan_changed = plan.append_completed_entry("local".to_string(), id.clone(), |entry| {
        workspace
            .issues
            .iter()
            .find(|issue| entry.project_key == "local" && issue.id == entry.issue_id)
            .is_some_and(|issue| issue.status == model::Status::Done)
    });
    if plan_changed {
        if let Err(e) = plan.save(&path) {
            eprintln!("Error: {e}");
            return 1;
        }
    }

    // DEC-44: when the active named plan is now complete, auto-deactivate it so the
    // active plan falls back to Backlog and the next item comes from there.
    let mut all = model::AllPlans::load(&path);
    let completed_active = all.active_plan_id.clone().and_then(|active_id| {
        all.named
            .iter()
            .find(|p| p.plan_id == active_id)
            .filter(|active| {
                model::plan_is_complete(&active.plan.entries, |key, iid| {
                    if !key.eq_ignore_ascii_case("local") {
                        return None;
                    }
                    workspace
                        .issues
                        .iter()
                        .find(|i| i.id == iid)
                        .map(|i| i.status)
                })
            })
            .map(|active| active.name.clone())
    });
    if let Some(name) = completed_active {
        all.active_plan_id = None;
        match all.save(&path) {
            Ok(()) => println!("Active plan '{name}' is complete — active plan is now Backlog."),
            Err(e) => {
                eprintln!("Warning: landed but could not auto-deactivate completed plan: {e}")
            }
        }
    }

    if let Ok(repo_root) = model::git_remote::find_repo_root(&path) {
        // FIX-79: integration into the default branch already happened above,
        // BEFORE the issue was marked done — a failed/diverged integration returns
        // early and never reaches here. By this point the verified commit is on the
        // default branch, so it is safe to tear the worktree down.
        let worktree_path = model::git_remote::worktree_path(&repo_root, &id);
        if worktree_path.exists() {
            if let Err(e) = model::git_remote::remove_worktree(&repo_root, &id) {
                eprintln!("Error: {e}");
                return 1;
            }
        }
        // DEC-35: the verified commit is on the default branch, so tear down the
        // execution branch too (FIX-99) — keep this CLI path in step with the MCP
        // gates::land teardown. Merged-only and best-effort; an unmerged branch is
        // kept for gc / the operator.
        model::git_remote::delete_execution_branch_if_merged(&repo_root, &id);
        match model::git_remote::release_claim(&repo_root, &id) {
            Ok(outcome) => report_claim_push(outcome, &id),
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    }

    println!("{} {id}. Canonical status updated to done.", verb.past());
    0
}

pub(super) fn run_reclaim(path: std::path::PathBuf, id: String) -> i32 {
    let repo_root = match model::git_remote::find_repo_root(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let info = match model::git_remote::inspect_claim(&repo_root, &id) {
        Ok(Some(i)) => i,
        Ok(None) => {
            eprintln!("Error: {id} is not claimed; use `ishoo start {id}` instead");
            return 1;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    if !model::git_remote::is_claim_stale(&info, STALE_THRESHOLD_SECS) {
        eprintln!(
            "Error: claim for {id} is fresh (held by {}). Reclaim only stale claims.",
            info.hostname
        );
        return 1;
    }
    match model::git_remote::reclaim(&repo_root, &id) {
        Ok(outcome) => report_claim_push(outcome, &id),
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    }
    println!(
        "Reclaimed {} from {} (was stale)",
        info.issue_id, info.hostname
    );
    0
}

pub(super) fn run_claim_refresh(path: std::path::PathBuf, id: String) -> i32 {
    let repo_root = match model::git_remote::find_repo_root(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    match model::git_remote::refresh_claim(&repo_root, &id) {
        Ok(outcome) => report_claim_push(outcome, &id),
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    }
    println!("Claim refreshed for {id}");
    0
}

pub(super) fn run_heatmap(path: std::path::PathBuf) -> i32 {
    let workspace = load_workspace(&path);
    model::cli_heatmap(&workspace);
    0
}

#[cfg(test)]
#[path = "handlers_link_tests.rs"]
mod link_tests;
#[cfg(test)]
#[path = "handlers_link_validation_tests.rs"]
mod link_validation_tests;
#[cfg(test)]
#[path = "handlers_plan_tests.rs"]
mod plan_tests;
#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
