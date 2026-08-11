# Handoff: MCP hardening after the 8-7 mining pass

Date: 2026-08-11

This is the continuation point for the reliability work that began with
`handoff-8-7.md` and the incident inventory in `mcp-crap-summary.md`. It records
what those documents turned into, what is now production behavior, and what is
still genuinely open. Use this file as the current handoff; keep the earlier
documents as historical evidence rather than treating their candidate lists as
an unfiltered backlog.

## How the documents fit together

- [`handoff-8-7.md`](./handoff-8-7.md) is the original DaVinci mining brief. It
  names production failure patterns and twelve candidates to investigate. Its
  instructions to audit and file issues describe that earlier phase; they are
  not current work orders.
- [`mcp-crap-summary.md`](./mcp-crap-summary.md) is the pre-hardening incident
  inventory: stale owners, unexpectedly huge work, missing worktree indexes,
  and normal typed refusals misreported as infrastructure failures.
- [`NOTES.md`](./NOTES.md) records the older Ishoo extraction/parity work. Its
  extraction checklist is complete: Ishoo consumes this crate and its runtime
  parity suite passed after the local duplicate runtime was removed.
- [`README.md`](./README.md) is the current product contract and integration
  guide. Prefer it over the handoffs for supported API behavior.
- [`docs/behavioral-truth-tables.md`](./docs/behavioral-truth-tables.md) is the
  production-scar recording method added by DOCS-01. It defines the five fields
  (nominal operation, observed failure, detection, mitigation, regression test)
  and maps them to the real owner-shutdown race.
- This file is the current process/state handoff.

## Current state

The `mcp-hardening` plan is effectively complete. Before this handoff it had 13
landed issues and one open issue, DOCS-02, whose sole deliverable is this file.
Landing DOCS-02 makes the plan 14/14.

The mining candidates became bounded, tested infrastructure rather than a copy
of DaVinci-specific machinery:

| Landed issue | Result |
| --- | --- |
| CONC-01 | Mutation classification and execution scheduling are independent; logically read-only work can still require serialization. |
| OWN-01 | Owner reachability and application readiness are distinct, with application health supplied by the app rather than inferred from arbitrary handler errors. |
| OWN-02 | An unhealthy owner can retire automatically through bounded, draining recovery instead of wedging work indefinitely. |
| ERR-01 | Tool failures carry stable machine-readable kinds/details separately from JSON-RPC codes and messages. |
| VAL-01 | Registry/manifest policy drift is mechanically validated against the real registered surface. |
| PROC-01 | Timed subprocesses terminate the process tree and bound post-kill output draining. |
| CANC-01 | Client cancellation, response timeout, and stdin disconnect do not abort an in-flight mutation at an arbitrary point. |
| BUSY-01 | Long operations expose a small lease/activity record so slow-but-working is distinguishable from hung. |
| DIAG-01 | Per-dispatch timing attributes queue, handler, owner round-trip, and total latency without requiring app-specific instrumentation. |
| DOCS-01 | Observed contract gaps become findable truth tables tied to executable regression tests, not a speculative runtime registry. |
| PROT-01 | Stdio accepts both the established stateful flow and MCP 2026-07-28 stateless requests, including `server/discover`, per-request metadata, modern result shapes, and unsupported-version errors. |
| GUAR-01 | Shell-guard installation no longer turns a missing tool into a permanent exit-127 PATH shim. |
| EOL-01 | The remaining CRLF working-tree files were normalized. |

The original mining brief was deliberately broader than this result. Existing
machinery such as `before_tool`, owner PID/token/fingerprint identity, ambiguous
commit handling, and shutdown draining was retained rather than re-filed under
new names. MCP infrastructure still does not own app-specific repair logic,
workflow/job orchestration, or a giant universal failure enum.

## Protocol boundary after PROT-01

The current protocol work is intentionally stdio-first because the consuming
servers are stdio servers. PROT-01 supplies dual-era stateless compatibility
without breaking established stateful clients.

Do not read PROT-01 as implicit scope for HTTP routing, OAuth, Tasks, or an SDK
v2 migration. Those may become separate product work when a real consumer or
transport requires them; they are not missing acceptance criteria for the
current runtime.

## Direct dogfood result on 2026-08-11

The SEMMAP outage that blocked Ishoo SAFE-12 was diagnosed in place rather than
worked around by repeatedly restarting the host.

The failure was not a dead MCP transport. SEMMAP indexed its own generated
`SEMMAP.md`. Generation created or rewrote that file, the MCP freshness gate
immediately classified the index as stale, and every navigation request ran
another roughly 30-second full regeneration. The host's 10-second owner
response boundary then reported `Transport closed`, which made healthy ongoing
work look like a dead server.

Semmap FIX-21 excludes canonical generated `SEMMAP.md` from both generation
collection and freshness discovery while preserving authored Markdown. The fix
is pushed as Semmap commit `a58f84c`; focused regressions pass, and the normal
Codex `semmap_context` call against Ishoo's SAFE-12 worktree returned in 0.6
seconds after regeneration.

This incident is a useful application of the behavioral-truth-table discipline:
the nominal freshness guarantee, observed self-invalidation loop, deterministic
file-count/progress signal, narrow exclusion, and regressions are all explicit.
It also reinforces the ownership boundary: `mcp-product-infra` should make the
timeout and activity facts legible, while the application owns correctness of
its freshness predicate.

## Cross-repository continuation point

At this handoff:

- `mcp-product-infra` main and `refs/ishoo/store` were synchronized before
  DOCS-02 began; strict Ishoo lint reported zero findings. The hardening plan's
  only open item was this handoff.
- Semmap main is clean and synchronized at `a58f84c`; FIX-21 is Done. The
  tracking-only FIX-20 was Declined and explicitly superseded by FIX-21. Strict
  Ishoo lint reported zero findings.
- Ishoo main and its store are clean and synchronized. SAFE-12 remains the
  intentionally active, owned issue with a clean worktree; no implementation
  was discarded. Required SEMMAP navigation now works in that worktree. Strict
  Ishoo lint reported zero findings.

## Work that remains

After DOCS-02 lands, there is no unfinished issue in `mcp-hardening`. The
remaining `mcp-product-infra` backlog belongs to the older
`extraction-parity` plan and should be evaluated on its own merits:

- TEST-02 — port remaining protocol/dispatch regression tests from Ishoo.
- CHOR-01 — expand CI to an OS matrix plus fmt and clippy.
- CHOR-02 — fix publish metadata and decide the long-term fate of `origin/`.
- FIX-03 — negotiate only supported MCP protocol versions during initialize.
- FEAT-01 — implement the manifest runner for non-Rust apps (`later`).

Do not silently fold these into the completed hardening plan. Use Ishoo's
candidate/plan workflow to choose the next item, re-check whether its old scope
is still correct after PROT-01, and supersede or re-scope stale work rather than
implementing it blindly.

## Next-session checklist

1. In the target repository, call `ishoo_brief`, `semmap_brief`,
   `ishoo_status`, `semmap_summary`, and `semmap_context` before source work.
2. For the interrupted product-safety thread, resume Ishoo SAFE-12 in its
   preserved worktree and follow its governing ADRs. Do not regenerate a new
   worktree or bypass SEMMAP now that navigation is healthy.
3. For new `mcp-product-infra` work, confirm `mcp-hardening` is fully landed,
   then evaluate the `extraction-parity` backlog rather than reopening the 8-7
   candidate list.
4. Classify incidents before changing infrastructure: dead owner, unhealthy
   app, legitimately long work, stale dependency, normal typed refusal, or
   response lost after possibly committed work.
5. When a nominal API contract differs from observation, add the five-field
   truth table and a boundary-accurate regression test. Do not leave the scar
   only in a handoff.
6. Treat host restart as recovery, not diagnosis. Capture the process,
   transport, activity, timing, and application-readiness facts first.

The durable direction remains the README's boundary: keep this crate the small,
boring operational layer for app-owned MCP servers, and keep product behavior
inside the consuming application.
