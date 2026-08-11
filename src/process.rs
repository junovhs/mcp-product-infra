//! Process-tree-safe command execution with a bounded deadline and output drain.

use crate::manifest::HandlerCommand;
use process_wrap::std::CommandWrap;
#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const WAIT_POLL: Duration = Duration::from_millis(5);
const POST_KILL_DRAIN: Duration = Duration::from_millis(250);

/// Captured output. `truncated` means inherited pipe handles did not close
/// before the bounded drain expired; the runner still returned on time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

/// Result of one process-tree run.
#[derive(Clone, Debug)]
pub enum ProcessOutcome {
    Completed {
        pid: u32,
        status: ExitStatus,
        output: ProcessOutput,
    },
    TimedOut {
        pid: u32,
        tree_terminated: bool,
        output: ProcessOutput,
    },
}

/// Run `spec` in its own process group (POSIX) or suspended Job Object
/// (Windows). On timeout the entire tree is terminated, and pipe draining is
/// bounded so an inherited handle cannot extend the call indefinitely.
pub fn run_with_timeout(spec: &HandlerCommand, timeout: Duration) -> io::Result<ProcessOutcome> {
    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }

    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);

    let mut child = command.spawn()?;
    let pid = child.id();
    let output_rx = start_output_readers(child.stdout().take(), child.stderr().take());
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(ProcessOutcome::Completed {
                pid,
                status,
                output: collect_output(output_rx, Instant::now() + POST_KILL_DRAIN),
            });
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(WAIT_POLL.min(deadline.saturating_duration_since(now)));
    }

    child.start_kill()?;
    let drain_deadline = Instant::now() + POST_KILL_DRAIN;
    let tree_terminated = loop {
        if child.try_wait()?.is_some() {
            break true;
        }
        let now = Instant::now();
        if now >= drain_deadline {
            break false;
        }
        thread::sleep(WAIT_POLL.min(drain_deadline.saturating_duration_since(now)));
    };

    Ok(ProcessOutcome::TimedOut {
        pid,
        tree_terminated,
        output: collect_output(output_rx, drain_deadline),
    })
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

fn start_output_readers(
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
) -> mpsc::Receiver<(OutputStream, io::Result<Vec<u8>>)> {
    let (tx, rx) = mpsc::channel();
    spawn_reader(OutputStream::Stdout, stdout, tx.clone());
    spawn_reader(OutputStream::Stderr, stderr, tx);
    rx
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: OutputStream,
    reader: Option<R>,
    tx: mpsc::Sender<(OutputStream, io::Result<Vec<u8>>)>,
) {
    thread::spawn(move || {
        let result = match reader {
            Some(mut reader) => {
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map(|_| bytes)
            }
            None => Ok(Vec::new()),
        };
        let _ = tx.send((stream, result));
    });
}

fn collect_output(
    rx: mpsc::Receiver<(OutputStream, io::Result<Vec<u8>>)>,
    deadline: Instant,
) -> ProcessOutput {
    let mut output = ProcessOutput::default();
    let mut received = 0;
    while received < 2 {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok((OutputStream::Stdout, Ok(bytes))) => output.stdout = bytes,
            Ok((OutputStream::Stderr, Ok(bytes))) => output.stderr = bytes,
            Ok((_, Err(_))) => output.truncated = true,
            Err(_) => break,
        }
        received += 1;
    }
    output.truncated |= received < 2;
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn command(program: &str, args: &[&str]) -> HandlerCommand {
        HandlerCommand {
            command: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_a_wrapper_and_its_pipe_inheriting_grandchild() {
        let spec = command(
            "sh",
            &["-c", "sleep 30 & child=$!; echo \"$$ $child\"; wait $child"],
        );
        assert_tree_timeout(spec);
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_a_wrapper_and_its_pipe_inheriting_grandchild() {
        let script = concat!(
            "$child = Start-Process -PassThru -NoNewWindow powershell.exe ",
            "-ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 30'; ",
            "Write-Output \"$PID $($child.Id)\"; $child.WaitForExit()"
        );
        let spec = command("powershell.exe", &["-NoProfile", "-Command", script]);
        assert_tree_timeout(spec);
    }

    fn assert_tree_timeout(spec: HandlerCommand) {
        let started = Instant::now();
        let outcome = run_with_timeout(&spec, Duration::from_millis(100)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let ProcessOutcome::TimedOut {
            pid,
            tree_terminated,
            output,
        } = outcome
        else {
            panic!("long-lived tree must time out");
        };
        assert!(
            tree_terminated,
            "the process tree must settle after termination"
        );
        assert!(
            !output.truncated,
            "killed descendants must close inherited pipes"
        );

        let pids: Vec<u32> = String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        assert_eq!(pids.first(), Some(&pid));
        assert_eq!(pids.len(), 2, "wrapper and grandchild PIDs are reported");
        for pid in pids {
            assert!(
                wait_until_dead(pid, Duration::from_secs(2)),
                "process {pid} survived the tree timeout"
            );
        }
    }

    fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !crate::sidecar::process_is_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        !crate::sidecar::process_is_alive(pid)
    }

    #[cfg(unix)]
    #[test]
    fn a_fast_command_returns_status_and_captured_output() {
        let outcome = run_with_timeout(
            &command("sh", &["-c", "printf out; printf err >&2; exit 7"]),
            Duration::from_secs(2),
        )
        .unwrap();
        let ProcessOutcome::Completed { status, output, .. } = outcome else {
            panic!("fast command must complete");
        };
        assert_eq!(status.code(), Some(7));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        assert!(!output.truncated);
    }

    #[cfg(windows)]
    #[test]
    fn a_fast_command_returns_status_and_captured_output() {
        let outcome = run_with_timeout(
            &command(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-Command",
                    "[Console]::Out.Write('out'); [Console]::Error.Write('err'); exit 7",
                ],
            ),
            Duration::from_secs(2),
        )
        .unwrap();
        let ProcessOutcome::Completed { status, output, .. } = outcome else {
            panic!("fast command must complete");
        };
        assert_eq!(status.code(), Some(7));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        assert!(!output.truncated);
    }
}
