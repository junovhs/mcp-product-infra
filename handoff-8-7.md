# Handoff: Mine `davinci-resolve-mcp` for hardening ideas for `mcp-product-infra`

## Goal

Review the supplied `davinci-resolve-mcp` repository as a source of production MCP failure modes and reliability patterns, compare those patterns against the current `mcp-product-infra`, and file a focused set of GitHub issues for generic infrastructure improvements that belong in `mcp-product-infra`.

Do **not** implement the issues in this task.

The objective is not to port Resolve-specific behavior. `mcp-product-infra` should remain the small, boring infrastructure layer that apps can reuse so they do not have to repeatedly solve MCP transport, lifecycle, ownership, recovery, concurrency, failure classification, diagnostics, etc.

We want to mine the DaVinci project because it has clearly encountered a large number of ugly real-world automation/lifecycle failures.

## Important context

`mcp-product-infra` powers my apps' MCP servers. Its job is to make the boring parts absurdly stable.

Some real incidents from apps using this infrastructure:

- A resident Ishoo MCP owner became stale. `ishoo_start` failed four times with `push failed`. Normal shell Git still worked, but the resident owner could not publish an ownership transition. Restarting Codex did not restart the resident owner, so progress halted until the stale owner itself was restarted.
- One Ishoo call took ~226 seconds because it was hashing 5.1 GB of accidentally stored build output. The MCP server was not dead, but from the client side the operation looked hung.
- SEMMAP calls in isolated worktrees frequently encountered a missing index, failed, spent ~17–22 seconds generating an index, and then had to be retried.
- `semmap_summary` once took ~74 seconds but eventually completed.
- Many things logged as MCP “failures” were actually normal typed refusals: invalid plan names, invalid ADR args, missing symbols, guarded workflow decisions, etc.

The useful failure categories are therefore:

1. genuinely unhealthy/stale resident owner;
2. healthy server doing unexpectedly enormous work;
3. missing/stale dependency requiring repair;
4. normal application refusal incorrectly lumped together with infrastructure failure.

Use those incidents as motivation, but do not claim they came from DaVinci.

---

# First: audit what `mcp-product-infra` ALREADY does

Before filing anything, inspect current `mcp-product-infra` and explicitly deduplicate proposals.

In particular, **do not file duplicates** for things already present.

The current repo already has substantial machinery including:

- resident singleton owner;
- crash-safe owner election;
- stale-build retirement using build fingerprint;
- bounded owner idle lifetime;
- liveness path;
- fail-closed writes when a live owner is unreachable;
- mutation FIFO;
- concurrent reads;
- global serial mode;
- `before_tool` readiness/freshness hook;
- mutation hook;
- shutdown drain;
- parent-death handling;
- Windows process-liveness handling;
- response timeout separate from socket framing timeout;
- owner transport failure classification;
- `ResponseLost` / malformed-response handling;
- **ambiguous commit handling already exists**:

  - `OwnerTransportError::ResponseLost`
  - `may_have_committed()`
  - `OwnerProse::ambiguous_commit`
  - “write may have committed; verify before retrying”
  - `tests/owner_shutdown_race.rs`

- panic containment;
- structured MCP results;
- annotations;
- host config install helpers;
- sidecar test-support helpers.

This matters because an earlier conceptual review suggested making ambiguous commit first-class; inspection of the current repo shows that this is **already implemented**. Do not file it again.

Relevant current files include:

- `src/server.rs`
- `src/sidecar.rs`
- `src/types.rs`
- `src/registry.rs`
- `src/manifest.rs`
- `tests/owner_shutdown_race.rs`
- `README.md`
- `docs/language-agnostic-sidecar.md`

---

# DaVinci source areas to mine

At minimum inspect these files and their associated tests:

### Concurrency / long-operation behavior

- `src/utils/page_lock.py`
- `src/utils/resolve_busy.py`
- `src/server.py`
- `tests/test_page_lock.py`
- `tests/test_resolve_busy.py`
- `tests/test_threaded_tool_dispatch.py`

### Lifecycle / stale-process behavior

- `src/utils/resolve_bridge.py`
- `src/utils/resolve_bridge_client.py`
- `src/utils/resolve_bridge_ops.py`
- `tests/test_resolve_bridge.py`

### Failure truth / behavioral reality

- `src/utils/api_truth.py`
- `tests/test_api_truth.py`
- `tests/test_api_truth_mitigations.py`

### Governance / safety drift

- `src/utils/destructive_hook.py`
- `tests/test_destructive_decorator_coverage.py`
- `tests/test_destructive_registry_drift.py`

### Long-running jobs / progress / recovery

- `src/utils/media_analysis_jobs.py`
- `src/utils/media_analysis.py`
- `tests/test_media_analysis.py`
- `tests/test_background_jobs.py`

### Instrumentation / qualification

- `src/utils/bridge_metrics.py`
- `src/utils/bridge_differential.py`
- associated bridge tests

---

# Candidate issues to evaluate

These are candidates, not orders to blindly file all of them. Inspect the current implementation first and consolidate overlapping ideas.

## 1. Separate mutation semantics from execution concurrency

**High priority.**

DaVinci exposes an important distinction that `MutationKind` alone cannot model.

A tool can be logically read-only while still requiring exclusive access to some thread-unsafe or globally mutable application resource.

Examples from DaVinci:

- Resolve has one globally active page.
- A read operation may temporarily switch to Color/Edit, read something, and restore the page.
- Concurrent “read-only” calls can corrupt one another if one switches pages underneath another.
- `page_lock.py` therefore serializes these operations even though the user-visible operation may not be a mutation.
- `resolve_busy.py` also prevents unrelated calls from entering the single-threaded scripting bridge during long synchronous operations.

`mcp-product-infra` currently strongly associates:

- read → concurrent
- mutation → FIFO serialized

and has `ServerConfig::serial()` as a global escape hatch.

Consider whether infrastructure should instead have a second, orthogonal concept such as:

- mutation classification: `Never | Always | Dynamic`
- execution policy: `Concurrent | Serialized | Lane(name) | Exclusive`

Exact API is open.

The important principle is:

> `readOnlyHint` describes semantic mutation. It should not also have to encode whether a handler/library is safe to execute concurrently.

Keep this generic. Do not introduce Resolve pages into the abstraction.

Potential acceptance tests should demonstrate two logically read-only tools sharing an exclusive execution lane without being advertised as mutations.

---

## 2. Add first-class “owner is reachable but application is unhealthy” readiness semantics

**High priority.**

DaVinci has a very strong real-world example in `api_truth.py`:

`ProjectManager.GetCurrentDatabase()` can return `None` after a bad Resolve startup while all superficial checks still work:

- scripting connection succeeds;
- product/version queries succeed;
- page queries succeed;
- current-project access can succeed;

but meaningful project operations fail or can block indefinitely.

DaVinci explicitly documents that a successful connection is not sufficient liveness/readiness evidence.

This maps closely to the Ishoo stale-owner incident:

- process existed;
- owner infrastructure existed;
- critical application operation was broken;
- restarting the MCP host did not repair the resident owner.

Investigate a generic distinction along the lines of:

- process alive;
- transport reachable;
- authoritative owner elected;
- application ready;
- application degraded/unhealthy.

`before_tool` already provides an application readiness gate, so do not replace it unnecessarily. The issue should investigate whether resident owners need a reusable health/readiness hook and machine-readable health state that clients/diagnostics/recovery can consult.

Important question:

> Can `mcp-product-infra` recognize “live process, working socket, broken resident application state” without treating an ordinary domain error as owner death?

Do not automatically retire an owner because a random handler returned an error.

---

## 3. Explicit owner/session generation identity

Evaluate whether this is already sufficiently represented by PID/token/fingerprint before filing.

DaVinci's bridge returns a `session` identifier from health. Reload waits specifically for the session ID to change rather than merely waiting for the endpoint to answer again.

Why?

Because during reload, the **old listener may still answer health checks**. “Endpoint responds” does not prove that the new runtime is active.

Every object handle is scoped to that session. After reload, old handles produce `stale_handle`.

Relevant code:

- `resolve_bridge_ops.py::op_health`
- `resolve_bridge_client.py::bridge_reload`

Potential generic lesson:

> distinguish build identity from process/instance generation, and make generation visible enough for diagnostics and stale-client-state detection.

`mcp-product-infra` already has PID, token, and build fingerprint. Determine whether those already solve this completely. Only file an issue if an explicit owner instance/generation ID would materially improve correctness or diagnostics.

---

## 4. Add generic long-operation visibility / exclusive-operation lease

**High priority.**

Inspect `src/utils/resolve_busy.py`.

DaVinci encountered synchronous operations that legitimately take seconds or minutes. A second call entering the underlying Resolve scripting bridge simply hangs with no feedback.

The project created a cross-process record containing roughly:

- operation label;
- PID;
- thread;
- start time.

Other calls wait briefly and then return structured `RESOLVE_BUSY` state naming the operation instead of hanging.

Stale records are ignored if:

- the PID is dead; or
- the operation exceeds a maximum age.

This maps directly to the 226-second Ishoo hashing incident.

Consider a very small generic primitive, not a job framework:

- register an exclusive/long operation;
- expose what is currently running;
- include elapsed time;
- reliably release on completion;
- ignore/recover stale registrations;
- optionally cause conflicting calls to return structured Busy state.

Possible names:

- operation lease;
- busy lease;
- exclusive operation guard;
- activity registry.

Non-goal: do not turn `mcp-product-infra` into a workflow engine.

---

## 5. Add lightweight progress/activity reporting for legitimately long handlers

Related to #4, but consider whether it deserves a separate issue.

DaVinci's media-analysis jobs persist:

- status;
- clip counts;
- percentage;
- `progress.json`;
- append-only events;
- terminal states;
- runner state.

The generic lesson is not “build a background job system.”

The generic lesson is:

> a 226-second active operation should be distinguishable from a 226-second dead operation.

Investigate an optional operation context that can update something like:

- phase;
- completed units;
- total units;
- last-progress timestamp;
- arbitrary small structured status.

This could later feed diagnostics or MCP progress notifications where appropriate.

Keep it optional and tiny.

---

## 6. Make structured failure KIND separate from JSON-RPC error code/message

**High priority.**

`mcp-product-infra::ToolError` currently appears to carry:

- numeric JSON-RPC code;
- message.

DaVinci's bridge has `OperationError` with:

- stable string code;
- human message;
- structured details.

Examples include:

- `invalid_arguments`
- `invalid_path`
- `input_not_found`
- `path_not_allowed`
- `resolve_unavailable`
- `resolve_not_ready`
- `capability_unavailable`
- `operation_failed`
- `no_project`
- `no_timeline`
- `not_found`
- `ambiguous_locator`
- `stale_handle`

This is directly relevant to our observed logs where normal refusals and real infrastructure failures are all described as “failed.”

Consider adding an optional stable machine-readable kind/data to `ToolError` while preserving proper JSON-RPC codes.

Potential generic categories might eventually include things like:

- invalid input;
- precondition not met;
- policy refusal;
- dependency unavailable/stale;
- busy;
- timeout;
- owner unhealthy;
- transport failure;
- internal failure;
- outcome unknown.

Do not prematurely freeze a giant universal enum if a small extensible string kind + structured data is better.

The goal is that agents, logs, and recovery logic can tell:

> “invalid plan name”

from:

> “resident owner is unhealthy.”

---

## 7. Add registry/policy drift validation helpers

**High priority because this is exactly the kind of boring bug infrastructure should prevent.**

Inspect:

- `src/utils/destructive_hook.py`
- `tests/test_destructive_registry_drift.py`
- `tests/test_destructive_decorator_coverage.py`

DaVinci had a serious safety regression where destructive action names existed in a registry under the wrong dispatcher/tool names. The safety registry therefore looked populated but never matched real calls, silently bypassing version-before-mutate behavior.

They added static tests that compare policy registries against the actual dispatched action strings.

Generalize the lesson:

> configuration/policy metadata that governs safety must be mechanically checked against the real registered surface.

Investigate generic validation/test helpers for things such as:

- duplicate tool names;
- invalid schemas;
- manifest entries with no corresponding handler;
- policy entries referencing nonexistent tools/ops;
- inconsistent annotations;
- mutation classifier fixtures;
- registered destructive policy with no reachable dispatch target;
- accidentally unclassified operations.

Do not build Python-AST-specific machinery into the Rust library. The abstraction should work for applications using `ToolRegistry`/manifest APIs.

---

## 8. Harden subprocess timeout semantics: kill the PROCESS TREE, not just the wrapper

This may become especially important for the language-agnostic manifest runner.

Inspect `media_analysis.py::_kill_process_tree` and the timeout helper around it.

DaVinci documents a measured failure:

- a command on Windows resolved to a wrapper/shim;
- killing the immediate child did not kill the real grandchild;
- the grandchild retained stdout/stderr handles;
- a nominal 5-second timeout still blocked for ~82 seconds until the real work naturally exited.

Their helper therefore uses `Popen` and kills the process tree, with explicit Windows and POSIX handling, then bounds the post-kill pipe drain.

This is exactly the kind of ugly cross-platform subprocess behavior a language-agnostic MCP handler runner should not make every app rediscover.

Evaluate a generic process-runner primitive with:

- timeout;
- descendant/process-group termination;
- bounded stdout/stderr drain;
- no orphaned child tree;
- clear timeout result;
- Windows wrapper/shim tests where feasible.

If the manifest runner is going to execute application handlers, this deserves particular attention.

---

## 9. Define cancellation semantics explicitly for mutations

Inspect `src/server.py::_install_threaded_tool_dispatch`.

DaVinci deliberately makes a synchronous Resolve operation that outlives client cancellation continue to completion while holding the serialization lock:

> the bridge is never left half-mutated

This is a useful policy lesson.

`mcp-product-infra` already has strong ambiguous-commit handling and owner-response timeout behavior, so inspect existing semantics carefully before filing.

Questions to answer:

- What happens to the actual handler when the MCP client disappears or times out?
- Can an in-flight mutation keep executing?
- Can lifecycle retirement interrupt it?
- Is the behavior tested?
- Is it different for reads vs mutations?
- Does MCP cancellation notification have defined semantics?

Potential desired invariant:

> loss of client interest must never implicitly mean “kill a mutation at an arbitrary point” unless the application explicitly opts into cooperative cancellation with safe semantics.

This may be a documentation/test-hardening issue rather than a new API.

---

## 10. Readiness/capability dependencies as structured state

Medium priority; avoid overengineering.

DaVinci has extensive capability detection:

- required external tools;
- availability;
- health endpoints;
- install guidance;
- capability gaps;
- stale/incomplete cached analysis;
- conditional execution based on whether fresh work is actually needed.

This resembles the SEMMAP missing-index problem.

`mcp-product-infra` already has `before_tool`, and its README even uses index freshness as the example, so do not file “add pre-dispatch readiness hooks”; that exists.

Instead investigate whether a small structured readiness/dependency vocabulary would help apps expose:

- ready;
- missing;
- stale;
- rebuilding;
- unavailable;
- repairable.

Potential benefit:

an app could implement:

`missing dependency → repair → retry once`

without every app inventing its own diagnostics format.

Keep auto-repair application-owned. Infrastructure should not know what an index is.

---

## 11. Add cheap per-operation timing/diagnostic telemetry

Medium priority.

DaVinci contains `bridge_metrics.py` specifically because performance bottlenecks in automation cannot safely be guessed; it counts real bridge round trips.

Our incidents similarly show that latency itself is diagnostic:

- normal SEMMAP operation versus 74 seconds;
- normal Ishoo operation versus 226 seconds.

Consider exposing generic timing facts for each dispatch:

- queue wait;
- handler execution time;
- owner round-trip time where applicable;
- total request time;
- outcome kind;
- owner generation/instance;
- whether request was handled locally or by resident owner.

Do not embed an observability platform.

A hook/structured diagnostic record/log line is enough.

The goal is to make “where did these 226 seconds go?” answerable without custom instrumentation in every app.

---

## 12. Consider an `api_truth`-style pattern as documentation/testing guidance, not necessarily core API

DaVinci's `src/utils/api_truth.py` is unusually valuable.

It records cases where the nominal API contract and observed real behavior differ, including:

- calls returning success/failure unreliably;
- silent no-op behavior;
- requirements that are not obvious from the vendor API;
- application states that look healthy but are unusable;
- mitigations that were empirically verified.

This probably does **not** belong as an `mcp-product-infra` runtime feature.

But consider whether the project should recommend or provide a tiny testing pattern for an application's “behavioral truth table”:

- nominal operation;
- observed failure;
- detection;
- mitigation;
- regression test.

The broader principle is excellent for infrastructure:

> encode ugly production knowledge into tests/data rather than leaving it in bug reports and tribal memory.

If this does not warrant a code issue, consider a docs issue.

---

# Things NOT to blindly port

Do not turn `mcp-product-infra` into DaVinci's domain framework.

Do not generically add:

- timeline versioning;
- source-media rules;
- grading safety;
- Resolve page concepts;
- confirm tokens simply because Resolve has them;
- media analysis;
- project/database semantics;
- giant background-job orchestration;
- a generic agent framework.

Likewise, do not replace working pieces of `mcp-product-infra` just because DaVinci implements an analogous mechanism differently.

The value of this exercise is extracting **failure classes and invariants**, not copying implementation.

---

# Suggested prioritization

My current expected priority, subject to your code audit:

### P0 / strongest candidates

1. Separate execution concurrency policy from mutation classification.
2. Model reachable-owner vs application-ready/application-unhealthy state.
3. Structured machine-readable tool failure kinds/details.
4. Long-operation/busy visibility.
5. Registry/policy drift validation helpers.
6. Process-tree-safe timeout helper, particularly before shipping the manifest runner.

### P1

7. Explicit/tested cancellation semantics for mutations.
8. Lightweight progress/activity reporting.
9. Per-operation diagnostic/timing telemetry.
10. Structured readiness/dependency states.

### Investigate before filing

11. Explicit owner/session generation identity — may already be adequately represented by token/PID/fingerprint.
12. `api_truth`-style behavioral truth tables — may be better as documentation/testing guidance than runtime code.

---

# Required issue format

For every issue filed, include:

## Problem

Describe the reusable MCP/application-infrastructure failure mode without Resolve-specific terminology.

## Evidence

Reference the DaVinci files and behavior that demonstrate the failure mode.

If relevant, separately mention the Ishoo/SEMMAP production incident as supporting motivation, clearly labeled as our own evidence rather than DaVinci evidence.

## Why this belongs in `mcp-product-infra`

Explain why multiple applications could encounter it and why leaving it to each application would recreate boring infrastructure.

## Existing infrastructure

Name the closest current primitive in `mcp-product-infra` and explain why it does or does not already solve the problem.

This section is mandatory to prevent duplicate issues.

## Possible shape

Sketch a minimal API/design only enough to make the issue concrete. Do not overdesign the implementation.

## Non-goals

Explicitly bound scope so the library stays small.

## Acceptance/regression tests

Describe the failure shape that should become mechanically tested.

---

# Deliverable

1. Audit the current `mcp-product-infra` implementation against the candidate list.
2. Produce a short dedupe table:

   - already solved;
   - partially solved;
   - genuinely missing;
   - probably application-specific.

3. File the worthwhile issues.
4. Prefer fewer, stronger issues over fragmenting one design into several overlapping tickets.
5. At the end, report:

   - issue number/title;
   - which DaVinci mechanism motivated it;
   - whether one of our Ishoo/SEMMAP incidents also supports it;
   - anything you deliberately chose not to file and why.

Do not implement the issues during this task.

The architectural standard is:

> `mcp-product-infra` should make MCP lifecycle, ownership, concurrency, failure semantics, subprocess behavior, and diagnostics so boring and difficult to get wrong that application authors rarely have to think about them.

Use DaVinci as a mine of production scars in service of that goal.
