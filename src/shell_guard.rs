//! Universal shell-guard: a PATH-shim delivery channel for [`crate::agent_guard`].
//!
//! Host hooks (see [`crate::HostInstall::claude_hook`]) only exist where the
//! agent host offers them. The lowest common denominator of *every* agent CLI
//! (Claude Code, Codex, Grok, OpenCode, Kilocode, a human terminal) is that
//! shell commands resolve binaries through `$PATH` of the login environment.
//! This module materializes a tiny platform-native `rg` shim into an app-owned
//! shim directory. POSIX callers pair it with a marker-delimited shell-startup
//! block; Windows callers pair the `.cmd` shim with a user-scoped `Path` entry.
//! In either case the same guard fires no matter which host spawned the shell.
//!
//! Safety posture:
//! - **Fail-open shim**: the only path that blocks is the guard binary
//!   explicitly exiting [`crate::agent_guard::BLOCK_EXIT_CODE`]. A missing or
//!   crashing guard binary falls through and `exec`s the real rg, so a broken
//!   install can never take ripgrep away from the user.
//! - **Non-clobbering rc edits**: only the marker-delimited block is ever
//!   written or removed; every user line outside it is preserved byte-exact.
//! - **Explicit enable only** (the DEC-88 discipline): nothing here runs on
//!   startup or tool calls — apps expose it behind an explicit opt-in flag.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Result of trying to materialize a guarded-tool shim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShimInstall {
    Installed { shim: PathBuf, real_tool: PathBuf },
    SkippedMissingTool { tool: String },
}

/// The marker pair delimiting the app-owned block in a shell rc file.
pub fn rc_markers(app_name: &str) -> (String, String) {
    (
        format!("# >>> {app_name} shell-guard >>>"),
        format!("# <<< {app_name} shell-guard <<<"),
    )
}

/// The full rc block: markers around a `PATH` prepend for the shim directory.
pub fn rc_block(app_name: &str, shim_dir: &Path) -> String {
    let (begin, end) = rc_markers(app_name);
    format!(
        "{begin}\n# Managed by `{app_name} enable --shell-guard` — remove with `--shell-guard --remove`.\ncase \":$PATH:\" in\n  *:\"{}\":*) ;;\n  *) export PATH=\"{}:$PATH\" ;;\nesac\n{end}\n",
        shim_dir.display(),
        shim_dir.display(),
    )
}

/// The POSIX-sh `rg` shim. `guard_bin` is the absolute path of the app binary
/// serving `agent-guard`, baked at install time; `app_name` labels messages.
pub fn rg_shim_script(app_name: &str, guard_bin: &str) -> String {
    format!(
        r#"#!/bin/sh
# {app_name} shell-guard shim for ripgrep.
# Blocks known output-corrupting flag misuse (rg short -r = --replace, the
# grep-muscle-memory footgun), then hands through to the real rg.
# Fails OPEN: if the guard binary is missing or errors, the real rg runs
# untouched. Remove with `{app_name} enable --shell-guard --remove`.
GUARD="{guard_bin}"
if [ -x "$GUARD" ]; then
  "$GUARD" agent-guard --check-rg -- "$@"
  # 86 is the guard's dedicated block code; anything else (including a clap
  # usage error, exit 2, from a version-skewed binary) falls through open.
  if [ $? -eq 86 ]; then
    exit 2
  fi
fi
SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
set -f
IFS=:
for dir in $PATH; do
  case "$dir" in
    ""|"$SELF_DIR") continue ;;
  esac
  if [ -x "$dir/rg" ]; then
    exec "$dir/rg" "$@"
  fi
done
echo "{app_name} shell-guard: ripgrep is not installed or is no longer on PATH behind the shim; install it, then re-enable the shell guard" >&2
exit 127
"#
    )
}

/// The Windows `cmd.exe` `rg` shim. The caller installs this as `rg.cmd` in a
/// directory that is prepended to the user's `Path`.
///
/// The shim deliberately finds `rg.exe` rather than calling `rg`: that avoids
/// recursively resolving itself while still preserving ripgrep's exit code and
/// argv. `where rg.exe` also lets a shim directory precede the real ripgrep
/// directory without knowing where the latter was installed.
pub fn windows_rg_shim_script(app_name: &str, guard_bin: &str) -> String {
    format!(
        r#"@echo off
rem {app_name} shell-guard shim for ripgrep.
rem Blocks known output-corrupting flag misuse, then hands through to real rg.exe.
rem Fails OPEN: only the guard's dedicated exit code blocks execution.
setlocal DisableDelayedExpansion
set "GUARD={guard_bin}"
if not exist "%GUARD%" goto find_real
"%GUARD%" agent-guard --check-rg -- %*
rem `if errorlevel` is >=, so test the range around 86 to require an exact match.
if errorlevel 87 goto find_real
if errorlevel 86 exit /b 2

:find_real
for /f "delims=" %%R in ('where rg.exe 2^>nul') do (
  "%%~fR" %*
  exit /b
)
echo {app_name} shell-guard: ripgrep is not installed or is no longer on PATH behind the shim; install it, then re-enable the shell guard 1>&2
exit /b 127
"#
    )
}

/// Idempotently write the executable `rg` shim when a real ripgrep is already
/// available on PATH. If it is absent, remove any stale shim and report the
/// skip so callers never turn "command not found" into a permanent exit 127.
pub fn ensure_rg_shim(shim_dir: &Path, app_name: &str, guard_bin: &str) -> Result<(), String> {
    ensure_rg_shim_report(shim_dir, app_name, guard_bin).map(|_| ())
}

/// [`ensure_rg_shim`] with an explicit installed/skipped result for readiness
/// surfaces that need to explain a missing guarded tool.
pub fn ensure_rg_shim_report(
    shim_dir: &Path,
    app_name: &str,
    guard_bin: &str,
) -> Result<ShimInstall, String> {
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    ensure_rg_shim_on_path(shim_dir, app_name, guard_bin, &search_path)
}

fn ensure_rg_shim_on_path(
    shim_dir: &Path,
    app_name: &str,
    guard_bin: &str,
    search_path: &OsStr,
) -> Result<ShimInstall, String> {
    let Some(real_tool) = find_real_tool(search_path, shim_dir, "rg") else {
        remove_rg_shim(shim_dir)?;
        return Ok(ShimInstall::SkippedMissingTool {
            tool: "rg".to_string(),
        });
    };
    let script = rg_shim_script(app_name, guard_bin);
    let path = shim_dir.join("rg");
    if fs::read_to_string(&path).ok().as_deref() == Some(script.as_str()) {
        ensure_executable(&path)?;
        return Ok(ShimInstall::Installed {
            shim: path,
            real_tool,
        });
    }
    fs::create_dir_all(shim_dir)
        .map_err(|e| format!("failed to create {}: {e}", shim_dir.display()))?;
    fs::write(&path, script).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    ensure_executable(&path)?;
    Ok(ShimInstall::Installed {
        shim: path,
        real_tool,
    })
}

/// Idempotently write the Windows `rg.cmd` shim into `shim_dir`.
pub fn ensure_windows_rg_shim(
    shim_dir: &Path,
    app_name: &str,
    guard_bin: &str,
) -> Result<(), String> {
    ensure_windows_rg_shim_report(shim_dir, app_name, guard_bin).map(|_| ())
}

/// [`ensure_windows_rg_shim`] with an explicit installed/skipped result for
/// readiness surfaces.
pub fn ensure_windows_rg_shim_report(
    shim_dir: &Path,
    app_name: &str,
    guard_bin: &str,
) -> Result<ShimInstall, String> {
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    ensure_windows_rg_shim_on_path(shim_dir, app_name, guard_bin, &search_path)
}

fn ensure_windows_rg_shim_on_path(
    shim_dir: &Path,
    app_name: &str,
    guard_bin: &str,
    search_path: &OsStr,
) -> Result<ShimInstall, String> {
    let Some(real_tool) = find_real_tool(search_path, shim_dir, "rg.exe") else {
        remove_windows_rg_shim(shim_dir)?;
        return Ok(ShimInstall::SkippedMissingTool {
            tool: "rg.exe".to_string(),
        });
    };
    let script = windows_rg_shim_script(app_name, guard_bin);
    let path = shim_dir.join("rg.cmd");
    if fs::read_to_string(&path).ok().as_deref() == Some(script.as_str()) {
        return Ok(ShimInstall::Installed {
            shim: path,
            real_tool,
        });
    }
    fs::create_dir_all(shim_dir)
        .map_err(|e| format!("failed to create {}: {e}", shim_dir.display()))?;
    fs::write(&path, script).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(ShimInstall::Installed {
        shim: path,
        real_tool,
    })
}

fn find_real_tool(search_path: &OsStr, shim_dir: &Path, name: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .filter(|dir| dir != shim_dir)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Remove the shim (and its directory when that leaves it empty). Missing
/// pieces are a no-op.
pub fn remove_rg_shim(shim_dir: &Path) -> Result<(), String> {
    let path = shim_dir.join("rg");
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    }
    if fs::read_dir(shim_dir).is_ok_and(|mut d| d.next().is_none()) {
        let _ = fs::remove_dir(shim_dir);
    }
    Ok(())
}

/// Remove the Windows `rg.cmd` shim (and its directory when that leaves it
/// empty). Missing pieces are a no-op.
pub fn remove_windows_rg_shim(shim_dir: &Path) -> Result<(), String> {
    let path = shim_dir.join("rg.cmd");
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    }
    if fs::read_dir(shim_dir).is_ok_and(|mut d| d.next().is_none()) {
        let _ = fs::remove_dir(shim_dir);
    }
    Ok(())
}

/// Return `path` with `shim_dir` prepended as one Windows `Path` segment.
///
/// The comparison is case-insensitive and ignores a trailing separator, as
/// Windows path identity does. Existing duplicates are removed so enabling is
/// idempotent and leaves exactly one Ishoo-owned segment.
pub fn windows_path_with_shim(path: &str, shim_dir: &Path) -> String {
    let shim = shim_dir.display().to_string();
    let mut parts = vec![shim.clone()];
    parts.extend(
        path.split(';')
            .filter(|part| !part.is_empty() && !same_windows_path(part, &shim))
            .map(str::to_owned),
    );
    parts.join(";")
}

/// Return `path` with every exact occurrence of `shim_dir` removed, preserving
/// all non-Ishoo user PATH segments byte-for-byte.
pub fn windows_path_without_shim(path: &str, shim_dir: &Path) -> String {
    let shim = shim_dir.display().to_string();
    path.split(';')
        .filter(|part| !same_windows_path(part, &shim))
        .collect::<Vec<_>>()
        .join(";")
}

fn same_windows_path(left: &str, right: &str) -> bool {
    left.trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.trim_end_matches(['\\', '/']))
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("failed to chmod {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Upsert the app's marker-delimited PATH block into an rc file: replace an
/// existing block in place, else append (with a separating blank line). Every
/// byte outside the markers is preserved.
pub fn ensure_rc_path_block(rc: &Path, app_name: &str, shim_dir: &Path) -> Result<(), String> {
    let block = rc_block(app_name, shim_dir);
    let existing = fs::read_to_string(rc).unwrap_or_default();
    let new_text = match find_block(&existing, app_name) {
        Some((start, end)) => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(&block);
            out.push_str(&existing[end..]);
            out
        }
        None => {
            let mut out = existing.clone();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    };
    if new_text == existing && rc.exists() {
        return Ok(());
    }
    if let Some(parent) = rc.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    fs::write(rc, new_text).map_err(|e| format!("failed to write {}: {e}", rc.display()))
}

/// Strip the app's marker block from an rc file, preserving everything else.
/// A missing file or absent block is a no-op; the file itself is never deleted.
pub fn remove_rc_path_block(rc: &Path, app_name: &str) -> Result<(), String> {
    let Ok(existing) = fs::read_to_string(rc) else {
        return Ok(());
    };
    let Some((start, end)) = find_block(&existing, app_name) else {
        return Ok(());
    };
    // Also swallow the one blank separator line the upsert added before the block.
    let mut head = &existing[..start];
    if head.ends_with("\n\n") {
        head = &head[..head.len() - 1];
    }
    let mut out = String::with_capacity(existing.len());
    out.push_str(head);
    out.push_str(&existing[end..]);
    fs::write(rc, out).map_err(|e| format!("failed to write {}: {e}", rc.display()))
}

/// Locate the marker block (inclusive of both marker lines and the trailing
/// newline of the end marker). Returns byte offsets `(start, end)`.
fn find_block(text: &str, app_name: &str) -> Option<(usize, usize)> {
    let (begin, end_marker) = rc_markers(app_name);
    let start = text.find(&begin)?;
    let end_start = text[start..].find(&end_marker)? + start;
    let mut end = end_start + end_marker.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn shim_is_written_executable_and_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let shim_dir = dir.path().join("shims");
        let real_dir = dir.path().join("real-bin");
        write_executable(
            &real_dir.join("rg"),
            "#!/bin/sh\nprintf 'real-rg %s' \"$1\"\n",
        );
        let search_path = std::env::join_paths([&real_dir]).unwrap();
        let installed =
            ensure_rg_shim_on_path(&shim_dir, "todo", "/opt/todo/bin/todo", &search_path).unwrap();
        assert!(matches!(installed, ShimInstall::Installed { .. }));
        let path = shim_dir.join("rg");
        let script = fs::read_to_string(&path).unwrap();
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("GUARD=\"/opt/todo/bin/todo\""));
        assert!(script.contains("agent-guard --check-rg -- \"$@\""));
        assert!(script.contains("exec \"$dir/rg\""), "fail-open passthrough");
        assert!(script.contains("ripgrep is not installed or is no longer on PATH"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let before = fs::read(&path).unwrap();
        ensure_rg_shim_on_path(&shim_dir, "todo", "/opt/todo/bin/todo", &search_path).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "second run is byte-identical"
        );

        #[cfg(unix)]
        {
            let mut invocation_path = vec![shim_dir.clone(), real_dir.clone()];
            invocation_path.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let output = std::process::Command::new(&path)
                .arg("sentinel")
                .env("PATH", std::env::join_paths(invocation_path).unwrap())
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"real-rg sentinel");
        }

        remove_rg_shim(&shim_dir).unwrap();
        assert!(!path.exists());
        assert!(!shim_dir.exists(), "empty shim dir removed");
    }

    #[test]
    fn missing_ripgrep_skips_install_and_removes_a_stale_shim() {
        let dir = tempfile::tempdir().unwrap();
        let shim_dir = dir.path().join("shims");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::write(shim_dir.join("rg"), "stale shim").unwrap();

        let empty_path = std::env::join_paths([dir.path().join("empty-bin")]).unwrap();
        let result =
            ensure_rg_shim_on_path(&shim_dir, "todo", "/opt/todo/bin/todo", &empty_path).unwrap();
        assert_eq!(
            result,
            ShimInstall::SkippedMissingTool {
                tool: "rg".to_string()
            }
        );
        assert!(!shim_dir.join("rg").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sourcing_the_rc_block_repeatedly_keeps_one_shim_path_segment() {
        let dir = tempfile::tempdir().unwrap();
        let shim_dir = dir.path().join("shim dir");
        let rc = dir.path().join("guard.rc");
        fs::write(&rc, rc_block("todo", &shim_dir)).unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(". \"$1\"; . \"$1\"; printf '%s' \"$PATH\"")
            .arg("guard-test")
            .arg(&rc)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(output.status.success());
        let path = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            path.split(':')
                .filter(|segment| *segment == shim_dir.to_string_lossy())
                .count(),
            1
        );
    }

    #[test]
    fn windows_shim_is_idempotent_and_only_blocks_the_dedicated_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let shim_dir = dir.path().join("shims");
        let real_dir = dir.path().join("real-bin");
        write_executable(&real_dir.join("rg.exe"), "fake windows executable");
        let search_path = std::env::join_paths([&real_dir]).unwrap();
        ensure_windows_rg_shim_on_path(
            &shim_dir,
            "todo",
            r"C:\Program Files\todo\todo.exe",
            &search_path,
        )
        .unwrap();
        let path = shim_dir.join("rg.cmd");
        let script = fs::read_to_string(&path).unwrap();
        assert!(script.starts_with("@echo off"));
        assert!(script.contains(r#"set "GUARD=C:\Program Files\todo\todo.exe"#));
        assert!(script.contains("agent-guard --check-rg -- %*"));
        assert!(script.contains("where rg.exe"));
        assert!(script.contains("if errorlevel 87 goto find_real"));
        assert!(script.contains("if errorlevel 86 exit /b 2"));

        let before = fs::read(&path).unwrap();
        ensure_windows_rg_shim_on_path(
            &shim_dir,
            "todo",
            r"C:\Program Files\todo\todo.exe",
            &search_path,
        )
        .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "second run is byte-identical"
        );

        remove_windows_rg_shim(&shim_dir).unwrap();
        assert!(!path.exists());
        assert!(!shim_dir.exists(), "empty shim dir removed");
    }

    #[test]
    fn windows_path_helpers_are_idempotent_and_remove_only_the_owned_segment() {
        let shim = Path::new(r"C:\Users\Juno\AppData\Local\ishoo\shims");
        let original = r"C:\Tools;C:\Users\Juno\AppData\Local\Ishoo\Shims\;C:\Tools2";
        let enabled = windows_path_with_shim(original, shim);
        assert_eq!(
            enabled,
            r"C:\Users\Juno\AppData\Local\ishoo\shims;C:\Tools;C:\Tools2"
        );
        assert_eq!(windows_path_with_shim(&enabled, shim), enabled);
        assert_eq!(
            windows_path_without_shim(&enabled, shim),
            r"C:\Tools;C:\Tools2"
        );
        assert_eq!(
            windows_path_without_shim(
                r"C:\ishoo\shims-extra;C:\ishoo\shims",
                Path::new(r"C:\ishoo\shims")
            ),
            r"C:\ishoo\shims-extra"
        );
    }

    #[test]
    fn rc_block_upsert_appends_replaces_and_removes_without_touching_user_lines() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("bashrc");
        fs::write(&rc, "export FOO=1\nalias ll='ls -l'\n").unwrap();

        ensure_rc_path_block(&rc, "todo", Path::new("/home/u/.local/share/todo/shims")).unwrap();
        let text = fs::read_to_string(&rc).unwrap();
        assert!(text.starts_with("export FOO=1\nalias ll='ls -l'\n"));
        assert!(text.contains("# >>> todo shell-guard >>>"));
        assert!(text.contains("export PATH=\"/home/u/.local/share/todo/shims:$PATH\""));

        // Idempotent re-run.
        let before = fs::read(&rc).unwrap();
        ensure_rc_path_block(&rc, "todo", Path::new("/home/u/.local/share/todo/shims")).unwrap();
        assert_eq!(fs::read(&rc).unwrap(), before);

        // A changed shim dir replaces the block in place (still exactly one block).
        ensure_rc_path_block(&rc, "todo", Path::new("/elsewhere/shims")).unwrap();
        let text = fs::read_to_string(&rc).unwrap();
        assert_eq!(text.matches("# >>> todo shell-guard >>>").count(), 1);
        assert!(text.contains("/elsewhere/shims"));
        assert!(!text.contains("/home/u/.local/share/todo/shims"));
        assert!(text.contains("alias ll='ls -l'"), "user lines preserved");

        // Removal strips the block and keeps user content.
        remove_rc_path_block(&rc, "todo").unwrap();
        let text = fs::read_to_string(&rc).unwrap();
        assert!(!text.contains("shell-guard"));
        assert!(text.contains("export FOO=1") && text.contains("alias ll='ls -l'"));
    }

    #[test]
    fn rc_block_on_a_missing_file_creates_it_and_remove_is_a_noop_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join("profile");
        ensure_rc_path_block(&rc, "todo", Path::new("/s")).unwrap();
        assert!(fs::read_to_string(&rc)
            .unwrap()
            .starts_with("# >>> todo shell-guard >>>"));

        let missing = dir.path().join("nope");
        remove_rc_path_block(&missing, "todo").unwrap();
        assert!(!missing.exists());
    }
}
