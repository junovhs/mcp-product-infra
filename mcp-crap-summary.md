here is a summary of some mcp bs i have dealt with: Yes. I can inspect other Codex tasks visible in this app and filter them by the Ishoo working directory. I searched the 50 most recently surfaced tasks and found 13 other Ishoo tasks.

The clearest genuine MCP-related halt was:

- “Review issue before assignment”: `ishoo_start` failed four times with `push failed`. Shell Git worked, but the resident Ishoo MCP owner could not publish the ownership transition. Restarting Codex did not restart that resident process, so the task explicitly stopped and asked for another restart. Later, restarting the stale Ishoo owner fixed it and work continued. This is a strong example of a stale resident MCP process halting progress.

Other significant incidents:

- “Fix Ishoo UI infinite spinner”: an Ishoo operation took about 226 seconds. This was not a dropped server—the owner was hashing 5.1 GB of accidentally stored build output. Both Ishoo and SEMMAP still answered, but operations appeared hung.
- “Fix Ishoo UI not opening”: the UI was blocked by a stale UI-instance lock, not by MCP. SEMMAP was unusually slow here—`semmap_summary` took about 74 seconds—but completed.
- Repeated SEMMAP failures occurred in several tasks because isolated Ishoo worktrees had no SEMMAP index. Calls such as `semmap_context`, `semmap_region`, and `semmap_diff_impact` failed immediately, forcing a 17–22 second `semmap_generate` followed by retry. These delayed work but generally did not halt it permanently.
- Several other “failed” Ishoo calls were ordinary validation or workflow refusals—invalid plan names, ADR arguments, unavailable symbols, or guarded lifecycle decisions—not server drops.

So the main categories are:

1. Actual stale Ishoo resident owner causing lifecycle calls to fail and progress to stop.
2. Extreme server-side work that looked like an MCP hang.
3. Missing/stale SEMMAP worktree indexes causing repeated failures and regeneration.
4. Normal typed refusals that logs label “failed” but are not infrastructure failures.

I did not find evidence in these tasks of the SEMMAP MCP transport completely disappearing mid-task. The strongest transport/process-health incident is the stale Ishoo resident owner.
