# HANDOFF 2026-07-16 — Two JSON-RPC spec-conformance fixes to file and land in THIS repo

**Who wrote this:** a Claude session working in `~/semmap`, which audited this crate's
transport while closing semmap's AUD-09 ("JSON-RPC framing correctness"). The defects
live in THIS repo's code, and per the per-repo decomposition rule (semmap/ishoo DEC-107)
their issues must be filed in THIS repo's own Ishoo store — that session could not do it
cross-repo. Everything needed is below; no other context required.

**What to do:** file two issues via the `ishoo_*` MCP tools (call `ishoo_brief` first,
per this repo's protocol), then implement them — both are small, test-covered changes
in `src/server.rs`. Both consumers of this runtime (semmap `semmap mcp`, ishoo
`ishoo mcp`) pick the fixes up on their next rebuild; no consumer-side changes needed.

---

## Defect 1 — known methods reply to notification-shaped frames

**Evidence:** `src/server.rs` `handle_line()` (as of 2026-07-16 the dispatch is at
:438-479). `is_request = message.get("id").is_some()` (:450) is only consulted in the
missing-method branch (:454-457) and the method-not-found fallthrough (:476-477).
The known-method arms (:460-474 — `initialize`, `tools/list`, `tools/call`,
`resources/list`, `resources/read`, `ping`) respond unconditionally. So a frame like
`{"jsonrpc":"2.0","method":"ping"}` — **no id, i.e. a notification** — gets a response
with `"id":null`. JSON-RPC 2.0 §4.1: "The Server MUST NOT reply to a Notification."

**Reachability:** conforming MCP hosts only send `notifications/*`-prefixed
notifications, which correctly return `None` at :475 — so this needs a nonconforming
client. Low severity, real deviation.

**Fix shape:** gate every known-method arm on `is_request` (return `None` when the
frame has no id). Watch the side-effect question for `tools/call`: a notification-shaped
tools/call today *executes the tool* and replies; after the fix decide (and record in
the issue) whether it should execute-without-reply or be dropped entirely — dropping is
simpler and safer (a caller who wants execution wants the result).

**Test:** `handle_line(r#"{"jsonrpc":"2.0","method":"ping"}"#)` must return `None`;
same for `tools/list` and a `tools/call`. Existing tests around :1259-1342 show the
harness pattern.

## Defect 2 — initialize echoes arbitrary client protocolVersion strings

**Evidence:** `src/server.rs` `initialize_result()` (:602-625). A missing or non-string
`params.protocolVersion` falls back to `DEFAULT_PROTOCOL_VERSION` (:603-607) — good —
but any *string* value (empty `""`, garbage `"lol"`, unsupported future versions) is
echoed verbatim into the result (:614). The MCP spec expects the server to answer with
a version **it supports** (its own latest, or the client's if supported).

**Fix shape:** answer with the client's requested version only if it is in a supported
set (start with just `DEFAULT_PROTOCOL_VERSION`); otherwise answer
`DEFAULT_PROTOCOL_VERSION`. Keep it a const list so adding versions is one line.

**Test:** initialize with `"protocolVersion":""` and `"protocolVersion":"garbage"`
must both come back as `DEFAULT_PROTOCOL_VERSION`; a supported version echoes.
Existing test `initialize_echoes_protocol_and_lists_tools` (:1342) will need its
expectation updated to match the new clamping behavior.

---

## Filing notes (for the Ishoo store in THIS repo)

- Two separate issues (independent, both `fix`-category, non-urgent; suggested labels:
  `fix` + `api`, urgency `mid` or unlabeled). Shape facts for both: mechanical, closed,
  decisive, ordinary → routine tier.
- Proof-of-done for each: the unit tests above passing + full `cargo test` green.
- Provenance line to include: "Found by semmap AUD-09 (semmap repo store, resolved
  2026-07-16), which audited the framing after it moved here in semmap's MCP-01
  migration."

## Context that is NOT a defect (don't re-litigate)

- No pre-initialize guard (tools callable before `initialize`): deliberate — the
  runtime is stateless by design (`handle_line` is a pure function of the line; the
  dispatch/FIFO model and tests rely on it), and no server state depends on initialize.
- `id: null` handling is correct: `get("id").is_some()` is true for `Value::Null`,
  so `{"id":null}` is treated as a request, not swallowed.
- Windows stdout capture (`src/capture.rs:158-218`): audited safe 2026-07-16 (semmap
  AUD-06) — the unchecked `GetStdHandle` is a faithful value round-trip; the restore
  cannot make state worse than it was. The unix `dup()` guard asymmetry is justified
  (dup is fallible and unrecoverable; the raw-value save is not).
- `resources.rs` read dispatch: audited safe (semmap AUD-08) — closed enumerated set,
  exact URI match, no client-derived paths.
