//! Long-operation leases: what is running, and for how long (BUSY-01).
//!
//! A legitimately slow operation and a wedged one look identical from the
//! client — silence. DEC-05 forbids interrupting a running mutation, so
//! visibility is the only remedy available: a handler registers a labelled
//! lease, the lease is queryable with its elapsed time while the call is still
//! in flight, and it releases on completion *or* panic because release is tied
//! to `Drop`.
//!
//! The registry is deliberately small. It is not a job framework: nothing is
//! persisted, scheduled, cancelled, or coordinated across machines.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A *foreign* registration older than this is ignored by [`snapshot`] and by
/// exclusivity checks. It is a leak backstop for records this process did not
/// create and cannot reason about; a record owned by this process is never
/// age-stale, because `Drop` is its release path and a genuinely slow operation
/// must stay visible no matter how long it runs — going invisible at six hours
/// would reintroduce exactly the silence this module exists to remove.
pub const MAX_ACTIVITY_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// How often an exclusive acquire re-checks a held lane while waiting.
const ACQUIRE_POLL: Duration = Duration::from_millis(10);

/// One running operation, as reported to a caller.
#[derive(Clone, Debug, Serialize)]
pub struct ActivityView {
    pub id: u64,
    /// What is running, in the app's own words (e.g. "ishoo_done FIX-01").
    pub label: String,
    /// The exclusivity lane this lease holds, when it holds one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// True when this lease was taken through [`try_acquire_exclusive`], i.e.
    /// it claimed an unoccupied lane. Mutual exclusion holds among acquirers:
    /// while this lease lives, no other [`try_acquire_exclusive`] on the lane
    /// succeeds. It is not a lock — [`begin_in_lane`] tags a lane without
    /// asking — so a lane is only truly serialized when every entrant acquires.
    pub exclusive: bool,
    pub pid: u32,
    /// How long it has been running — the number that separates "slow" from
    /// "hung" at a glance.
    pub elapsed_ms: u64,
    pub started_unix_ms: u64,
    /// Optional coarse progress the handler reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Milliseconds since the last progress report — a 226-second operation
    /// that is still ticking is not the same as one that stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_progress_ms: Option<u64>,
}

#[derive(Clone, Debug)]
struct Record {
    label: String,
    lane: Option<String>,
    exclusive: bool,
    pid: u32,
    started: Instant,
    started_unix_ms: u64,
    phase: Option<String>,
    completed: Option<u64>,
    total: Option<u64>,
    last_progress: Option<Instant>,
}

impl Record {
    fn view(&self, id: u64) -> ActivityView {
        ActivityView {
            id,
            label: self.label.clone(),
            lane: self.lane.clone(),
            exclusive: self.exclusive,
            pid: self.pid,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            started_unix_ms: self.started_unix_ms,
            phase: self.phase.clone(),
            completed: self.completed,
            total: self.total,
            since_progress_ms: self.last_progress.map(|at| at.elapsed().as_millis() as u64),
        }
    }

    /// A record is stale — and therefore invisible and non-blocking — when it
    /// belongs to another process that is gone, or when it is a foreign record
    /// that has outlived [`MAX_ACTIVITY_AGE`]. Our own records are held by a
    /// live `ActivityLease`, so age alone never retires one.
    fn is_stale(&self) -> bool {
        if self.pid == std::process::id() {
            return false;
        }
        !crate::sidecar::process_is_alive(self.pid) || self.started.elapsed() > MAX_ACTIVITY_AGE
    }
}

#[derive(Default)]
struct State {
    next_id: u64,
    records: HashMap<u64, Record>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// The registry lock, recovered from poisoning rather than propagated.
///
/// Every critical section here is a `HashMap` insert/remove/read, so a panic
/// elsewhere can never leave the map half-updated — the map is always
/// consistent and the poison flag carries no information. Treating poison as a
/// failure is what would be dangerous: `Drop` would skip its removal and leak
/// the very lease this module promises to release.
fn lock_state() -> std::sync::MutexGuard<'static, State> {
    match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            state().clear_poison();
            poisoned.into_inner()
        }
    }
}

/// A held registration. Dropping it releases the lease, which is the whole
/// reliability argument: a handler that returns early, returns an error, or
/// panics cannot leave the operation registered.
#[derive(Debug)]
pub struct ActivityLease {
    id: u64,
}

impl ActivityLease {
    /// This lease's registry id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Report coarse progress. Purely informational: it moves the phase, the
    /// unit counts, and the last-progress clock a reader uses to tell a ticking
    /// operation from a stopped one.
    pub fn progress(&self, phase: impl Into<String>, completed: Option<u64>, total: Option<u64>) {
        // Convert before locking: a caller's `Into<String>` is arbitrary code,
        // and it must not be able to panic while holding the registry lock.
        let phase = phase.into();
        let mut state = lock_state();
        if let Some(record) = state.records.get_mut(&self.id) {
            record.phase = Some(phase);
            record.completed = completed;
            record.total = total;
            record.last_progress = Some(Instant::now());
        }
    }

    /// This lease's current view, or `None` if it was already released.
    pub fn view(&self) -> Option<ActivityView> {
        let state = lock_state();
        state
            .records
            .get(&self.id)
            .map(|record| record.view(self.id))
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        lock_state().records.remove(&self.id);
    }
}

/// Insert `record` into an already-held registry guard.
fn insert_locked(state: &mut State, record: Record) -> ActivityLease {
    state.next_id += 1;
    let id = state.next_id;
    state.records.insert(id, record);
    ActivityLease { id }
}

fn insert(record: Record) -> ActivityLease {
    insert_locked(&mut lock_state(), record)
}

fn new_record(label: String, lane: Option<String>, exclusive: bool) -> Record {
    Record {
        label,
        lane,
        exclusive,
        pid: std::process::id(),
        started: Instant::now(),
        started_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        phase: None,
        completed: None,
        total: None,
        last_progress: None,
    }
}

/// Register a running operation. Always succeeds — a shared lease never
/// contends with anything, it only makes the work visible.
pub fn begin(label: impl Into<String>) -> ActivityLease {
    insert(new_record(label.into(), None, false))
}

/// [`begin`] with a lane label. Infallible by design: it announces occupancy
/// (blocking later exclusive acquires of the lane) but never waits and never
/// refuses, so it is for work that is *allowed* to run — not a way into a lane
/// someone else holds exclusively. Serialize a lane by having every entrant
/// call [`try_acquire_exclusive`].
pub fn begin_in_lane(label: impl Into<String>, lane: impl Into<String>) -> ActivityLease {
    insert(new_record(label.into(), Some(lane.into()), false))
}

/// Take exclusive hold of `lane`, waiting up to `wait` for a conflicting lease
/// to finish. On timeout this returns a structured `busy` failure (ERR-01)
/// naming what is running and how long it has been — never an indefinite block,
/// and never silence.
pub fn try_acquire_exclusive(
    label: impl Into<String>,
    lane: impl Into<String>,
    wait: Duration,
) -> Result<ActivityLease, crate::types::ToolError> {
    let label = label.into();
    let lane = lane.into();
    let deadline = Instant::now() + wait;
    loop {
        // Check and claim under one guard. Two threads racing an empty lane
        // must not both observe it free and then both insert — that would hand
        // out two "exclusive" leases for the same lane.
        match claim_locked(&label, &lane) {
            Ok(lease) => return Ok(lease),
            Err(holder) => {
                if Instant::now() >= deadline {
                    return Err(busy_error(&lane, &holder));
                }
                std::thread::sleep(
                    ACQUIRE_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

/// One atomic attempt: take the lane if no live lease occupies it, otherwise
/// report the holder. An exclusive acquire is blocked by any lease in the lane,
/// exclusive or not — the point of a lane is that its work does not overlap.
// The holder is returned boxed: `ActivityView` is a wide struct and this is the
// hot retry path, so an unboxed `Err` would make every poll iteration move it by
// value on the stack for a case the caller usually discards.
fn claim_locked(label: &str, lane: &str) -> Result<ActivityLease, Box<ActivityView>> {
    let mut state = lock_state();
    if let Some((id, record)) = state
        .records
        .iter()
        .find(|(_, record)| record.lane.as_deref() == Some(lane) && !record.is_stale())
    {
        return Err(Box::new(record.view(*id)));
    }
    let record = new_record(label.to_string(), Some(lane.to_string()), true);
    Ok(insert_locked(&mut state, record))
}

fn busy_error(lane: &str, holder: &ActivityView) -> crate::types::ToolError {
    crate::types::ToolError::new(
        crate::types::SERVER_ERROR,
        format!(
            "busy: '{}' is already running in lane '{lane}' ({}ms elapsed). \
             Nothing was started; retry when it finishes.",
            holder.label, holder.elapsed_ms
        ),
    )
    .with_kind(crate::types::kinds::BUSY)
    .with_data(serde_json::json!({
        "lane": lane,
        "running": holder.label,
        "running_elapsed_ms": holder.elapsed_ms,
        "retry_safe": true,
    }))
}

/// Every operation currently running, newest last. Stale registrations — a dead
/// owning process, or an age past [`MAX_ACTIVITY_AGE`] — are omitted, so a leak
/// can never masquerade as live work.
pub fn snapshot() -> Vec<ActivityView> {
    let state = lock_state();
    let mut views: Vec<ActivityView> = state
        .records
        .iter()
        .filter(|(_, record)| !record.is_stale())
        .map(|(id, record)| record.view(*id))
        .collect();
    views.sort_by_key(|view| view.id);
    views
}

/// Test-only registration of records this process could not otherwise create.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Register a record owned by `pid` and started `age` ago, so the staleness
    /// rules can be exercised without a real foreign process or a six-hour wait.
    pub fn register_foreign(
        label: &str,
        lane: Option<&str>,
        pid: u32,
        age: Duration,
    ) -> ActivityLease {
        let mut record = new_record(label.to_string(), lane.map(str::to_string), false);
        record.pid = pid;
        record.started = Instant::now().checked_sub(age).expect("representable age");
        insert(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lease_is_visible_with_its_label_and_elapsed_time() {
        let lease = begin("slow_tool");
        let first = snapshot()
            .into_iter()
            .find(|view| view.id == lease.id())
            .expect("a held lease must be visible");
        assert_eq!(first.label, "slow_tool");
        assert!(!first.exclusive);

        std::thread::sleep(Duration::from_millis(25));
        let later = lease.view().expect("still held");
        assert!(
            later.elapsed_ms > first.elapsed_ms,
            "elapsed must grow while the call runs: {} -> {}",
            first.elapsed_ms,
            later.elapsed_ms
        );

        let id = lease.id();
        drop(lease);
        assert!(
            !snapshot().iter().any(|view| view.id == id),
            "the lease must be gone once the operation returns"
        );
    }

    #[test]
    fn a_panicking_operation_leaves_no_lease() {
        let outcome = std::panic::catch_unwind(|| {
            let lease = begin("panicking_tool");
            let id = lease.id();
            // Prove it was registered before the unwind.
            assert!(snapshot().iter().any(|view| view.id == id));
            panic!("boom with a lease held");
        });
        assert!(outcome.is_err());
        assert!(
            !snapshot().iter().any(|view| view.label == "panicking_tool"),
            "Drop must release the lease even on panic"
        );
    }

    #[test]
    fn a_conflicting_exclusive_acquire_returns_busy_naming_the_running_operation() {
        let held = begin_in_lane("long_index_rebuild", "index");
        let started = Instant::now();
        let error = try_acquire_exclusive("second_rebuild", "index", Duration::from_millis(50))
            .expect_err("a held lane must refuse, not block");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the refusal must come back within the bounded wait"
        );
        assert_eq!(error.kind.as_deref(), Some(crate::types::kinds::BUSY));
        assert!(
            error.message.contains("long_index_rebuild"),
            "the refusal must name what is running: {}",
            error.message
        );
        let data = error.error_data().unwrap();
        assert_eq!(data["running"], "long_index_rebuild");
        assert_eq!(data["lane"], "index");
        assert_eq!(data["retry_safe"], true);
        drop(held);
    }

    #[test]
    fn an_exclusive_lane_frees_up_once_the_holder_finishes() {
        let held = begin_in_lane("first", "lane_a");
        drop(held);
        let lease = try_acquire_exclusive("second", "lane_a", Duration::from_millis(0))
            .expect("a free lane must be acquirable");
        assert_eq!(lease.view().unwrap().label, "second");
        assert!(lease.view().unwrap().exclusive);
    }

    /// Only one of many threads racing the same empty lane may win. A
    /// check-then-insert acquire passes every serial test and still hands out
    /// two "exclusive" leases here.
    #[test]
    fn only_one_of_many_racing_acquirers_takes_an_empty_lane() {
        use std::sync::{Arc, Barrier};

        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let racers: Vec<_> = (0..threads)
            .map(|n| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    try_acquire_exclusive(format!("racer_{n}"), "raced_lane", Duration::ZERO).ok()
                })
            })
            .collect();
        // Collect, do not count: every winning lease must stay held until the
        // assertion, or joining would release the lane mid-race and a straggler
        // could legitimately acquire it after the fact.
        let winners: Vec<ActivityLease> = racers
            .into_iter()
            .filter_map(|handle| handle.join().expect("racer must not panic"))
            .collect();
        assert_eq!(winners.len(), 1, "exactly one acquirer may hold the lane");
    }

    /// A slow operation this process is still running stays visible past the
    /// leak backstop — going quiet at six hours is the failure this module
    /// exists to prevent. Age only retires records we cannot vouch for.
    #[test]
    fn our_own_long_running_registration_never_ages_out() {
        let ours = test_support::register_foreign(
            "twelve_hour_hash",
            Some("lane_d"),
            std::process::id(),
            MAX_ACTIVITY_AGE * 2,
        );
        assert!(
            snapshot().iter().any(|view| view.id == ours.id()),
            "a live in-process operation must stay visible however long it runs"
        );
        try_acquire_exclusive("interloper", "lane_d", Duration::ZERO)
            .expect_err("and it must still hold its lane");
    }

    #[test]
    fn a_dead_process_or_overage_registration_is_ignored() {
        let dead =
            test_support::register_foreign("ghost", Some("lane_b"), reaped_pid(), Duration::ZERO);
        let ancient = test_support::register_foreign(
            "ancient",
            Some("lane_c"),
            live_foreign_pid(),
            MAX_ACTIVITY_AGE + Duration::from_secs(60),
        );

        let ids: Vec<u64> = snapshot().into_iter().map(|view| view.id).collect();
        assert!(!ids.contains(&dead.id()), "a dead pid's record is ignored");
        assert!(
            !ids.contains(&ancient.id()),
            "an over-age record is ignored"
        );

        // And neither one may wedge its lane.
        try_acquire_exclusive("after_ghost", "lane_b", Duration::ZERO)
            .expect("a dead holder must not block the lane");
        try_acquire_exclusive("after_ancient", "lane_c", Duration::ZERO)
            .expect("an over-age holder must not block the lane");
    }

    /// A pid that definitely no longer exists: spawn a child, wait for it (so it
    /// is reaped, not a zombie), and reuse its pid. `kill(0, 0)` would signal the
    /// process group and read as alive, so a literal 0 is not a dead pid.
    #[cfg(unix)]
    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a trivial child");
        let pid = child.id();
        child.wait().expect("reap the child");
        pid
    }

    #[cfg(not(unix))]
    fn reaped_pid() -> u32 {
        u32::MAX
    }

    /// A pid that is alive but is not us, so the age rule (which only applies
    /// to foreign records) is the thing under test. On unix, pid 1 always
    /// exists; `kill(1, 0)` returning EPERM still reads as alive.
    #[cfg(unix)]
    fn live_foreign_pid() -> u32 {
        1
    }

    #[cfg(not(unix))]
    fn live_foreign_pid() -> u32 {
        std::process::id().wrapping_add(1)
    }

    #[test]
    fn progress_reports_move_the_phase_and_the_last_progress_clock() {
        let lease = begin("hashing");
        assert!(lease.view().unwrap().since_progress_ms.is_none());

        lease.progress("hashing objects", Some(3), Some(10));
        let view = lease.view().unwrap();
        assert_eq!(view.phase.as_deref(), Some("hashing objects"));
        assert_eq!(view.completed, Some(3));
        assert_eq!(view.total, Some(10));
        assert!(view.since_progress_ms.is_some());
    }
}
