use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct OwnerEndpoint {
    addr: SocketAddr,
    token: String,
    pid: u32,
    /// FEAT-34: the build fingerprint of the owner process. A client whose own build
    /// differs retires this owner and elects a fresh one, so a `cargo install` takes effect
    /// without a manual restart. `serde(default)` = empty for a pre-FEAT-34 registration,
    /// which mismatches any real fingerprint and is therefore retired on first contact.
    #[serde(default)]
    fingerprint: String,
}

#[derive(Deserialize, Serialize)]
struct OwnerRequest {
    token: String,
    line: String,
}

#[derive(Deserialize, Serialize)]
struct OwnerResponse {
    response: Option<String>,
}

/// The ishoo binary name, used for a PATH fallback when `current_exe()` is unusable.
#[cfg(windows)]
const OWNER_BIN_NAME: &str = "ishoo.exe";
#[cfg(not(windows))]
const OWNER_BIN_NAME: &str = "ishoo";

/// Strip Linux's unlinked-executable " (deleted)" marker from a `current_exe()` path,
/// yielding the real on-disk path a replacement binary would occupy. `None` when there is
/// no marker to strip. Kernel-formatted paths use exactly this suffix after the inode is
/// unlinked (e.g. `cargo install` overwriting the running binary).
fn strip_deleted_marker(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?
        .strip_suffix(" (deleted)")
        .map(PathBuf::from)
}

/// Search `path_var` (a `PATH`-formatted list) for an executable named `name`.
fn find_in_paths(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Resolve the executable to spawn the resident owner with (FIX-154). `current_exe()` is
/// the correct binary, but after `cargo install` (or any in-place replacement) the running
/// process's original path is unlinked: on Linux `current_exe()` then returns a
/// `<path> (deleted)` path that does not exist, so spawning it fails with ENOENT and wedges
/// every write behind "Failed to spawn resident MCP owner: No such file or directory". The
/// resolution order: the live `current_exe()` if it still exists; else the same path with
/// the " (deleted)" marker stripped (a `cargo install` has usually written the replacement
/// back there); else a `PATH` lookup of the binary name; else a clear, actionable error.
fn resolve_owner_exe() -> Result<PathBuf, String> {
    let current = std::env::current_exe().ok();
    if let Some(exe) = current.as_ref() {
        if exe.exists() {
            return Ok(exe.clone());
        }
        if let Some(replaced) = strip_deleted_marker(exe) {
            if replaced.exists() {
                return Ok(replaced);
            }
        }
    }
    // Derive the binary name (marker stripped) for a PATH lookup, defaulting to `ishoo`.
    let name = current
        .as_ref()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().trim_end_matches(" (deleted)").to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| OWNER_BIN_NAME.to_string());
    if let Some(path_var) = std::env::var_os("PATH") {
        if let Some(found) = find_in_paths(&name, &path_var) {
            return Ok(found);
        }
    }
    Err(format!(
        "Failed to locate an ishoo executable to spawn the resident MCP owner: \
         current_exe is unavailable or deleted and '{name}' was not found on PATH"
    ))
}

/// The running build's owner fingerprint (FEAT-34): the package version plus the resolved
/// owner binary's size and mtime. Computed from `resolve_owner_exe` (not the raw
/// `current_exe`, which may be an unlinked "(deleted)" path after `cargo install`), so a
/// client with a replaced binary and the owner it spawns from the *same* on-disk binary
/// agree on the fingerprint — otherwise the two would disagree forever and retire in a
/// loop. Two different builds (a release bump or a dev rebuild) yield different signatures,
/// so a stale owner is always retired.
fn current_build_fingerprint() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let exe_sig = resolve_owner_exe()
        .ok()
        .and_then(|path| fs::metadata(&path).ok())
        .map(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}-{}", meta.len(), mtime)
        })
        .unwrap_or_default();
    format!("{version}+{exe_sig}")
}

/// Ask a live owner running a different build to shut down, then wait briefly for it to
/// exit so its singleton lock (FIX-153) is released before we elect the replacement.
fn retire_stale_owner(endpoint: &OwnerEndpoint) {
    let _ = send_line(endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if !crate::ui::process_is_alive(endpoint.pid) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub(super) fn ensure_owner_process(root: &Path) -> Result<OwnerEndpoint, String> {
    let root = crate::model::git_remote::canonical_workspace_root(root);
    if let Some(endpoint) = read_endpoint(&root) {
        if send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            // FEAT-34: a live owner running our build is attached to directly. One running
            // a different build is gracefully retired here so a `cargo install` (or dev
            // rebuild) takes effect on the next session instead of the resident owner
            // silently serving the old binary forever (the recurring deployment gotcha).
            if endpoint.fingerprint == current_build_fingerprint() {
                return Ok(endpoint);
            }
            retire_stale_owner(&endpoint);
            // Fall through to elect a fresh owner from the current binary.
        }
    }

    let endpoint_file = endpoint_path(&root);
    let _ = fs::remove_file(&endpoint_file);
    let exe = resolve_owner_exe()?;
    let root_arg = root.display().to_string();
    Command::new(exe)
        .args(["--path", &root_arg, "mcp-owner"])
        // FIX-154: pin the child's working directory to the (existing) workspace root. A
        // client that inherited a now-deleted cwd would otherwise fail the spawn with
        // ENOENT ("No such file or directory") before the owner ever starts.
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn resident MCP owner: {e}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(endpoint) = read_endpoint(&root).and_then(|endpoint| {
            send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
                .ok()
                .map(|_| endpoint)
        }) {
            return Ok(endpoint);
        }
        if std::time::Instant::now() >= deadline {
            return Err("resident MCP owner did not publish a usable endpoint".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Outcome of trying to recover a writable resident owner after a mutation could not
/// reach the one we were using (FIX-150).
pub(super) enum OwnerRecovery {
    /// A live owner is reachable now — either a fresh one we spawned, or one another
    /// process registered since (e.g. the app was restarted). Retry the write here.
    Reelected(OwnerEndpoint),
    /// The owner we were using is unreachable but its process is still alive: a live
    /// single writer mid-blip. Never spawn a second writer (STOR-22/DEC-83) — fail the
    /// write closed and let the caller retry against the same owner.
    LiveButUnreachable,
    /// No live owner and a fresh one could not be elected; the store is unwritable
    /// until a resident owner comes back. Carries the underlying reason.
    Down(String),
}

/// Recover a writable owner after `tried` became unreachable mid-session (FIX-150,
/// DEC-53 "crashes heal"). PID liveness — not just a failed socket ping — decides
/// whether to spawn: a still-alive owner process is a live writer having a blip and
/// must never be duplicated (the STOR-22 second-writer hole); only a genuinely dead
/// owner is cleared from the registration and replaced.
pub(super) fn recover_owner(root: &Path, tried: &OwnerEndpoint) -> OwnerRecovery {
    let root = crate::model::git_remote::canonical_workspace_root(root);
    // Whoever is registered and answering a ping right now is the live owner — a fresh
    // one, a restarted app's, or ours after a transient blip. Prefer it; no spawn.
    if let Some(current) = read_endpoint(&root) {
        if send_line(&current, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            return OwnerRecovery::Reelected(current);
        }
    }
    // The owner we were using did not answer. If its process is still alive, it is a
    // live writer mid-blip — refuse rather than start a rival writer.
    if crate::ui::process_is_alive(tried.pid) {
        return OwnerRecovery::LiveButUnreachable;
    }
    // The owner process is gone. Drop the stale registration so no client keeps dialing
    // a corpse, then elect a fresh resident writer and hand it back.
    let _ = fs::remove_file(endpoint_path(&root));
    match ensure_owner_process(&root) {
        Ok(endpoint) => OwnerRecovery::Reelected(endpoint),
        Err(e) => OwnerRecovery::Down(e),
    }
}

pub(super) fn run_owner_server(root: PathBuf) -> Result<(), String> {
    let root = crate::model::git_remote::canonical_workspace_root(&root);

    // FIX-153 (DEC-83/DEC-77): singleton by construction. Hold an exclusive OS advisory
    // lock for this process's whole lifetime BEFORE doing any owner work. The kernel
    // releases it the instant the process dies, so election is crash-safe: a dead owner's
    // lock vanishes with it (no stale-lock wedge), and a live owner's lock cannot be taken
    // by a second `mcp-owner`. If the lock is already held, a resident owner is live —
    // exit cleanly (the client that spawned us finds and uses the existing endpoint)
    // rather than become a rival writer (the 2026-07-04 split-brain: two owner lineages
    // each pushing over the other on one .ishoo/).
    let _owner_lock = match OwnerLock::try_acquire(&root)? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    super::run_startup_store_sync(&root);
    crate::model::publisher::spawn(root.clone());
    // FIX-148: the inbound analogue of the auto-publisher. A resident owner runs for
    // hours; without a cadence it never sees store commits another machine pushes
    // between boundaries and serves an arbitrarily stale store. This periodically
    // fetches + fast-forwards + re-materializes so readers converge without a restart.
    crate::model::reconciler::spawn(root.clone());
    let _ = crate::model::store_owner::start(&root)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("Failed to bind resident MCP owner socket: {e}"))?;
    let endpoint = OwnerEndpoint {
        addr: listener
            .local_addr()
            .map_err(|e| format!("Failed to read resident MCP owner address: {e}"))?,
        token: new_token(),
        pid: std::process::id(),
        fingerprint: current_build_fingerprint(),
    };
    write_endpoint(&root, &endpoint)?;
    // Only the lock holder ever reaches here, so it is the sole author of the endpoint
    // registration. Keep it authoritative: re-assert it if a racing client removed it,
    // and exit if the store it serves disappears.
    spawn_owner_watchdog(root.clone(), endpoint.clone());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let root = root.clone();
                let token = endpoint.token.clone();
                thread::spawn(move || {
                    let _ = handle_owner_stream(&root, &token, stream);
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    Ok(())
}

pub(super) fn send_line(endpoint: &OwnerEndpoint, line: &str) -> Result<Option<String>, String> {
    let mut stream = TcpStream::connect_timeout(&endpoint.addr, Duration::from_secs(1))
        .map_err(|e| format!("Failed to connect to resident MCP owner: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Failed to set MCP owner read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("Failed to set MCP owner write timeout: {e}"))?;
    let request = OwnerRequest {
        token: endpoint.token.clone(),
        line: line.to_string(),
    };
    serde_json::to_writer(&mut stream, &request)
        .map_err(|e| format!("Failed to encode MCP owner request: {e}"))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("Failed to write MCP owner request: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("Failed to flush MCP owner request: {e}"))?;

    let mut raw = String::new();
    BufReader::new(stream)
        .read_line(&mut raw)
        .map_err(|e| format!("Failed to read MCP owner response: {e}"))?;
    let response: OwnerResponse = serde_json::from_str(raw.trim_end())
        .map_err(|e| format!("Malformed MCP owner response: {e}"))?;
    Ok(response.response)
}

fn handle_owner_stream(root: &Path, token: &str, stream: TcpStream) -> Result<(), String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("Failed to clone MCP owner stream: {e}"))?,
    );
    let mut raw = String::new();
    reader
        .read_line(&mut raw)
        .map_err(|e| format!("Failed to read MCP owner request: {e}"))?;
    let request: OwnerRequest = serde_json::from_str(raw.trim_end())
        .map_err(|e| format!("Malformed MCP owner request: {e}"))?;

    // FEAT-34: graceful upgrade handoff. A token-authenticated `owner/shutdown` retires
    // this owner so a newer-build client can elect a replacement. Ack first (so the client
    // sees a clean handoff), then drain any in-flight mutation by taking the serial write
    // queue, and exit — the OS releases the singleton lock (FIX-153) on exit. An interrupted
    // mutation would be crash-safe anyway (DEC-53), but draining avoids a client-visible
    // error during a routine upgrade.
    if request.token == token && line_method(&request.line).as_deref() == Some("owner/shutdown") {
        let ack =
            Some(r#"{"jsonrpc":"2.0","result":{"status":"shutting_down"}}"#.to_string());
        let mut writer = stream;
        serde_json::to_writer(&mut writer, &OwnerResponse { response: ack })
            .map_err(|e| format!("Failed to encode MCP owner response: {e}"))?;
        writer
            .write_all(b"\n")
            .map_err(|e| format!("Failed to write MCP owner response: {e}"))?;
        let _ = writer.flush();
        // Drain: block until any in-flight serialized mutation completes, then exit.
        let _ = crate::model::store_owner::mutate(root, || Ok::<(), String>(()));
        std::process::exit(0);
    }

    let response = if request.token == token {
        super::handle_line(root, &request.line)
    } else {
        Some(super::error_frame(
            serde_json::Value::Null,
            super::INVALID_REQUEST,
            "Invalid resident MCP owner token",
        ))
    };
    let mut writer = stream;
    serde_json::to_writer(&mut writer, &OwnerResponse { response })
        .map_err(|e| format!("Failed to encode MCP owner response: {e}"))?;
    writer
        .write_all(b"\n")
        .map_err(|e| format!("Failed to write MCP owner response: {e}"))?;
    Ok(())
}

fn endpoint_path(root: &Path) -> PathBuf {
    root.join(".ishoo").join("cache").join("mcp-owner.json")
}

/// Path of the resident-owner singleton lock — distinct from the endpoint registration
/// (`mcp-owner.json`). The lock is a live/dead signal held by the OS; the registration is
/// the mutable "how to reach the current owner" fact the lock holder maintains.
fn owner_lock_path(root: &Path) -> PathBuf {
    root.join(".ishoo").join("cache").join("mcp-owner.lock")
}

/// An exclusive, OS-advisory lock on the workspace's owner-lock file, held for the whole
/// lifetime of the resident owner (FIX-153, DEC-83/DEC-77). The kernel releases it the
/// instant the holding process dies, so exactly one `mcp-owner` per workspace can hold it:
/// a second one fails to acquire and exits instead of becoming a rival writer, and a
/// crashed owner leaves no stale lock to wedge the next election. Dropping the value (or
/// the process exiting) releases the lock.
struct OwnerLock {
    // The open, locked file. Kept alive for the process lifetime; the advisory lock is
    // bound to this open file description and releases when it is closed.
    _file: fs::File,
}

impl OwnerLock {
    /// Try to take the singleton lock without blocking. `Ok(Some)` = acquired (we are the
    /// one owner); `Ok(None)` = another live owner already holds it; `Err` = the lock file
    /// could not even be opened (a real filesystem fault, not contention).
    fn try_acquire(root: &Path) -> Result<Option<Self>, String> {
        let path = owner_lock_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create owner lock dir: {e}"))?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("Failed to open owner lock file {}: {e}", path.display()))?;
        match try_lock_exclusive_nonblocking(&file) {
            Ok(true) => Ok(Some(OwnerLock { _file: file })),
            Ok(false) => Ok(None),
            Err(e) => Err(format!("Failed to acquire owner lock: {e}")),
        }
    }
}

/// Non-blocking exclusive whole-file advisory lock via `flock(2)`. On Linux the lock is
/// bound to the open file description and the kernel drops it on fd close / process death,
/// which is precisely the crash-safe singleton primitive we want. `Ok(false)` means
/// another open description already holds it (contention, not an error).
#[cfg(unix)]
fn try_lock_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    // EWOULDBLOCK (== EAGAIN) is the "already locked" signal for a LOCK_NB request.
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK => Ok(false),
        _ => Err(err),
    }
}

/// Windows analogue via `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY` (non-blocking) and
/// `LOCKFILE_EXCLUSIVE_LOCK`. The lock is released when the handle closes / the process
/// exits, the same crash-safe property `flock` gives on Unix. A lock-violation error means
/// another owner holds it (contention); any other failure is treated conservatively as
/// "not acquired" so we never start a rival writer on an ambiguous result.
#[cfg(windows)]
fn try_lock_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == ERROR_LOCK_VIOLATION as i32 => Ok(false),
        _ => Ok(false),
    }
}

/// The lock holder's registration watchdog (FIX-153). Because only the lock holder runs
/// it, re-asserting is always correct — no rival owner can exist to fight over the file:
///  - Re-write our endpoint if it goes missing or is overwritten (a racing client removes
///    `mcp-owner.json` before spawning; the doomed second owner exits without writing, so
///    without this the live owner's registration would stay gone).
///  - Exit the process when the `.ishoo/` store it exists to serve is gone — a resident
///    owner for a deleted / temp-dir workspace is a zombie (six such were found running on
///    2026-07-04); its lock and socket should die with the store.
fn spawn_owner_watchdog(root: PathBuf, endpoint: OwnerEndpoint) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(1));
        if !root.join(".ishoo").exists() {
            std::process::exit(0);
        }
        let ours = read_endpoint(&root).is_some_and(|cur| cur.pid == endpoint.pid);
        if !ours {
            let _ = write_endpoint(&root, &endpoint);
        }
    });
}

fn read_endpoint(root: &Path) -> Option<OwnerEndpoint> {
    let raw = fs::read_to_string(endpoint_path(root)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_endpoint(root: &Path, endpoint: &OwnerEndpoint) -> Result<(), String> {
    let path = endpoint_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create MCP owner cache dir: {e}"))?;
    }
    let text = serde_json::to_string(endpoint)
        .map_err(|e| format!("Failed to encode MCP owner endpoint: {e}"))?;
    fs::write(&path, text).map_err(|e| format!("Failed to write MCP owner endpoint: {e}"))
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// The JSON-RPC `method` of a raw owner-request line, or `None` if absent/unparseable —
/// used to route control-plane methods (FEAT-34 `owner/shutdown`) before the MCP dispatch.
fn line_method(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
}

/// An endpoint whose address has no listener, so a connect is refused — the
/// "resident owner unreachable" path (STOR-22). Bind an ephemeral port to reserve a
/// real address, then drop the listener so nothing is accepting on it.
#[cfg(test)]
pub(super) fn unreachable_endpoint_for_tests() -> OwnerEndpoint {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read ephemeral addr");
    drop(listener);
    OwnerEndpoint {
        addr,
        token: new_token(),
        pid: std::process::id(),
        fingerprint: current_build_fingerprint(),
    }
}

/// Persist `endpoint` as the resident-owner registration, so a recovery path can find
/// and ping it (FIX-150 tests).
#[cfg(test)]
pub(super) fn write_endpoint_for_tests(root: &Path, endpoint: &OwnerEndpoint) {
    write_endpoint(root, endpoint).expect("write test owner registration");
}

#[cfg(test)]
pub(super) fn start_owner_thread_for_tests(root: PathBuf) -> Result<OwnerEndpoint, String> {
    let root = crate::model::git_remote::canonical_workspace_root(&root);
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("Failed to bind test MCP owner socket: {e}"))?;
    let endpoint = OwnerEndpoint {
        addr: listener
            .local_addr()
            .map_err(|e| format!("Failed to read test MCP owner address: {e}"))?,
        token: new_token(),
        pid: std::process::id(),
        fingerprint: current_build_fingerprint(),
    };
    let token = endpoint.token.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let _ = handle_owner_stream(&root, &token, stream);
            }
        }
    });
    Ok(endpoint)
}

#[cfg(test)]
mod owner_lock_tests {
    use super::*;

    // FIX-153: the singleton invariant is CROSS-PROCESS exclusion (two `mcp-owner`
    // processes), which flock guarantees; flock's *same-process* multi-fd behavior is only
    // "may be denied" per flock(2), so we assert the real contract against the external
    // flock(1) tool: while we hold the lock, another process cannot take it; once we
    // release, it can. Linux-only (util-linux flock); the guarantee is kernel-provided.
    #[cfg(target_os = "linux")]
    #[test]
    fn owner_lock_excludes_another_process_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = owner_lock_path(root);

        let held = OwnerLock::try_acquire(root).unwrap().expect("first acquire must win");
        // An external process trying the same lock non-blocking must fail while we hold it.
        let busy = Command::new("flock")
            .args(["-n", path.to_str().unwrap(), "-c", "true"])
            .status()
            .expect("run flock(1)");
        assert!(
            !busy.success(),
            "another process must be refused the lock while we hold it — no rival writer"
        );

        drop(held);
        // After we release (the crash/exit analogue), another process can take it.
        let free = Command::new("flock")
            .args(["-n", path.to_str().unwrap(), "-c", "true"])
            .status()
            .expect("run flock(1)");
        assert!(
            free.success(),
            "after the holder releases, the lock is available again (crash-safe re-election)"
        );
    }

    // The lock file lives under .ishoo/cache/ and acquiring it creates that dir when
    // absent, so a brand-new workspace elects an owner without a pre-created cache.
    #[test]
    fn acquire_creates_the_cache_dir_and_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _lock = OwnerLock::try_acquire(root).unwrap().expect("acquire");
        assert!(
            owner_lock_path(root).exists(),
            "the lock file must be created under .ishoo/cache/"
        );
    }
}

#[cfg(test)]
mod resolve_exe_tests {
    use super::*;

    // FIX-154: the crux — Linux marks an unlinked (cargo-install-replaced) exe path with a
    // trailing " (deleted)"; stripping it yields the real path the replacement now occupies.
    #[test]
    fn strip_deleted_marker_recovers_the_real_path() {
        assert_eq!(
            strip_deleted_marker(Path::new("/home/u/.cargo/bin/ishoo (deleted)")),
            Some(PathBuf::from("/home/u/.cargo/bin/ishoo")),
        );
        // A live path has no marker to strip.
        assert_eq!(strip_deleted_marker(Path::new("/home/u/.cargo/bin/ishoo")), None);
    }

    // The PATH fallback finds a real executable by name in a PATH-formatted search list.
    #[test]
    fn find_in_paths_locates_a_named_executable() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let tool = b.path().join("ishoo");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        let path_var =
            std::env::join_paths([a.path().to_path_buf(), b.path().to_path_buf()]).unwrap();
        assert_eq!(find_in_paths("ishoo", &path_var), Some(tool));
        assert_eq!(find_in_paths("not-present", &path_var), None);
    }

    // In the common case current_exe() exists, so resolution returns a real, spawnable path.
    #[test]
    fn resolve_owner_exe_returns_an_existing_path() {
        let exe = resolve_owner_exe().expect("must resolve an owner exe");
        assert!(exe.exists(), "resolved owner exe must exist on disk: {}", exe.display());
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    // FEAT-34: the fingerprint is deterministic for one build, so two clients of the same
    // installed binary agree and do not retire each other in a loop.
    #[test]
    fn build_fingerprint_is_stable_within_a_build() {
        assert_eq!(current_build_fingerprint(), current_build_fingerprint());
        assert!(
            current_build_fingerprint().starts_with(env!("CARGO_PKG_VERSION")),
            "fingerprint carries the package version"
        );
    }

    // Control-plane routing: the shutdown method is recognized; noise is not.
    #[test]
    fn line_method_extracts_the_control_method() {
        assert_eq!(
            line_method(r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#).as_deref(),
            Some("owner/shutdown")
        );
        assert_eq!(line_method("not json").as_deref(), None);
        assert_eq!(line_method(r#"{"id":0}"#).as_deref(), None);
    }

    // The upgrade path must also retire a pre-FEAT-34 owner: its registration has no
    // fingerprint field, which deserializes to empty and therefore mismatches any real
    // build fingerprint — so the first newer client retires it and elects a fresh owner.
    #[test]
    fn legacy_registration_without_fingerprint_mismatches_and_is_retired() {
        let json = r#"{"addr":"127.0.0.1:5000","token":"t","pid":42}"#;
        let ep: OwnerEndpoint = serde_json::from_str(json).expect("legacy json deserializes");
        assert!(
            ep.fingerprint.is_empty(),
            "a pre-FEAT-34 registration carries no fingerprint"
        );
        assert_ne!(
            ep.fingerprint,
            current_build_fingerprint(),
            "empty fingerprint mismatches the current build -> the stale owner is retired"
        );
    }

    // A current registration round-trips the fingerprint, so a same-build client attaches
    // without a spurious retire.
    #[test]
    fn current_registration_round_trips_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let ep = OwnerEndpoint {
            addr: "127.0.0.1:5001".parse().unwrap(),
            token: "t".to_string(),
            pid: std::process::id(),
            fingerprint: current_build_fingerprint(),
        };
        write_endpoint(dir.path(), &ep).unwrap();
        let read = read_endpoint(dir.path()).expect("registration read back");
        assert_eq!(read.fingerprint, current_build_fingerprint());
    }
}
