# Behavioral truth tables

Production APIs sometimes behave differently from their nominal contract. A
call may report success before work is durable, silently do nothing in one
state, or answer a health probe while being unable to serve real requests.
Once investigated, record that difference as a small truth table and pin the
mitigation with a regression test. This turns an expensive incident into
knowledge the next maintainer can find and executable evidence the code cannot
quietly forget.

## The five fields

Record one row per distinct behavior:

| Field | What to record |
| --- | --- |
| Nominal operation | The API call and the behavior its contract appears to promise. |
| Observed failure | What actually happened, including the relevant state or race. |
| Detection | A deterministic signal that distinguishes the failure from normal behavior. |
| Mitigation | The smallest behavior verified to prevent or safely handle the failure. |
| Regression test | The test that recreates the state, exercises the mitigation, and fails if the scar returns. |

Prefer observable facts over theories. Include exact response shapes, process
states, timing boundaries, or filesystem conditions when they matter. A
mitigation without a reproducing test is still a hypothesis; a prose note
without either is easy to lose.

## Worked example: owner shutdown with a mutation in flight

The regression in
[`tests/owner_shutdown_race.rs`](../tests/owner_shutdown_race.rs) maps onto all
five fields:

| Field | Recorded behavior |
| --- | --- |
| Nominal operation | `owner/shutdown` retires the resident owner cleanly while ordinary owner requests receive their responses. |
| Observed failure | The old shutdown path exited the process immediately. When shutdown raced a slow mutation, the mutation could commit but its connection closed before the reply arrived. The caller saw a lost response and could not know whether retrying would double-apply the write. |
| Detection | Start a real child owner, send a deliberately slow request, request shutdown while that handler is running, and observe whether the original connection receives the completed response rather than a reset or `ResponseLost`. |
| Mitigation | Acknowledge shutdown, stop accepting new work, drain in-flight handlers, and exit only after their responses have been written. |
| Regression test | `committed_mutation_reply_survives_owner_shutdown` repeats the race three times, requires the shutdown acknowledgement and slow reply, and then requires the owner to exit. |

The test uses a real child process because an in-process double would not cover
the failure boundary: process exit tearing down a live socket. Its timing is
bounded and its assertions name the safety property, so a failure points back
to the production scar rather than merely reporting a sleep-related mismatch.

## From finding to durable knowledge

1. Reduce the incident to the smallest repeatable state and operation.
2. Write the five fields before changing the code. This separates observed
   behavior from the proposed explanation.
3. Add a regression test at the boundary where the failure occurred. Use a real
   process, transport, filesystem, or dependency when that boundary caused the
   bug; otherwise prefer a smaller deterministic test.
4. Implement the mitigation and demonstrate that the test fails before it and
   passes after it.
5. Keep the table near the owning code or project documentation and link the
   regression test by its stable path and test name.

Use an ADR when the mitigation establishes a durable architectural rule or
chooses among meaningful alternatives. Use ordinary code comments when a local
invariant would otherwise be surprising. Change the public contract when the
observed behavior is intentional and callers should rely on it. The truth table
does not replace any of those artifacts; it connects the original observation
to the mitigation and the permanent proof.

Do not turn this practice into a speculative registry. Record behaviors that
were observed or can be reproduced, keep each row narrow, and let the regression
test remain the authority on whether the mitigation still works.
