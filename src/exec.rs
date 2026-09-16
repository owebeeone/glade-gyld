//! The bounded Gyld-host runner.
//!
//! One implementation serves both paths: a synchronous request collects the
//! output and answers with it, and a `stream:true` request runs the SAME way on
//! a blocking task with a line sink that appends each line to the log surface as
//! it arrives. There is no second command shape to keep in step.
//!
//! Everything is bounded. Wall clock: a hard timeout, after which the child is
//! killed and the failure is data. Output: a per-stream byte budget, after which
//! lines stop accumulating and the answer says so. Pipes drain on their own
//! threads, so a chatty host can never deadlock the wait.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::verbs::Plan;

/// The default wall-clock bound for one host run. A full re-capture of a bundle
/// is minutes of Python, not seconds, so this is generous next to `glade-gwz`'s
/// thirty seconds; it is still a bound.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// The default per-stream output budget: 1 MiB of stdout and 1 MiB of stderr.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1 << 20;

/// The two bounds every run carries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// A finished run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunOutput {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    /// One or both streams hit the byte budget.
    pub truncated: bool,
}

/// Runs a [`Plan`]. The trait exists so the supplier can be driven by a
/// recording double in tests without a Python interpreter anywhere near it.
pub trait Runner: Send + Sync + 'static {
    /// Run to completion, calling `on_line(stream, line)` for each output line
    /// as it arrives, and answer with the collected output. An `Err` is a spawn
    /// failure or a timeout, which the caller turns into data.
    fn run(
        &self,
        plan: &Plan,
        limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String>;
}

/// The real runner: `<python> -B <script> <args…>` from the Gyld root with
/// `PYTHONPATH=src:.`. `-B` is not decoration — the Gyld checkout is read-only
/// to this process and must not collect `__pycache__` from it.
#[derive(Clone, Debug)]
pub struct PythonRunner {
    pub python: PathBuf,
}

impl PythonRunner {
    pub fn new(python: PathBuf) -> PythonRunner {
        PythonRunner { python }
    }
}

impl Runner for PythonRunner {
    fn run(
        &self,
        plan: &Plan,
        limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        run_bounded(&self.python, plan, limits, on_line)
    }
}

/// Spawn the host, drain both pipes on threads, enforce both bounds.
pub fn run_bounded(
    python: &Path,
    plan: &Plan,
    limits: Limits,
    on_line: &mut dyn FnMut(&str, &str),
) -> Result<RunOutput, String> {
    let mut command = std::process::Command::new(python);
    command
        .arg("-B")
        .args(&plan.argv)
        .current_dir(&plan.cwd)
        .env("PYTHONPATH", &plan.pythonpath)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", python.display()))?;

    let (tx, rx) = mpsc::channel::<(&'static str, String)>();
    let mut readers = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        readers.push(drain(pipe, "stdout", tx.clone()));
    }
    if let Some(pipe) = child.stderr.take() {
        readers.push(drain(pipe, "stderr", tx.clone()));
    }
    drop(tx);

    let mut out = RunOutput::default();
    let deadline = Instant::now() + limits.timeout;
    let mut timed_out = false;
    let mut exit: Option<i32> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok((stream, line)) => {
                on_line(stream, &line);
                let sink = if stream == "stdout" {
                    &mut out.stdout
                } else {
                    &mut out.stderr
                };
                if sink.len() + line.len() < limits.max_output_bytes {
                    sink.push_str(&line);
                    sink.push('\n');
                } else {
                    out.truncated = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
    }

    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
    } else {
        match child.wait() {
            Ok(status) => exit = Some(status.code().unwrap_or(-1)),
            Err(e) => {
                return Err(format!("wait failed: {e}"));
            }
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    if timed_out {
        return Err(format!("timed out after {}ms", limits.timeout.as_millis()));
    }
    out.exit = exit.unwrap_or(-1);
    if out.truncated {
        out.stderr
            .push_str("[output truncated at the supplier's byte budget]\n");
    }
    Ok(out)
}

/// One pipe drainer. Lossy UTF-8 so a stray byte is a replacement character and
/// never a lost run.
fn drain<R: std::io::Read + Send + 'static>(
    pipe: R,
    stream: &'static str,
    tx: mpsc::Sender<(&'static str, String)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => {
                    break;
                }
                Ok(_) => {
                    while buffer.last() == Some(&b'\n') || buffer.last() == Some(&b'\r') {
                        buffer.pop();
                    }
                    let line = String::from_utf8_lossy(&buffer).into_owned();
                    if tx.send((stream, line)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::Layout;
    use crate::envelope::GyldRequest;
    use crate::verbs::plan;

    /// A shim script standing in for a Gyld host, so the bounds are exercised
    /// deterministically without a Python interpreter or a Gyld checkout.
    fn shim(tag: &str, body: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("glade-gyld-shim-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shim");
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn a_plan() -> Plan {
        let layout = Layout::new(std::env::temp_dir(), PathBuf::from("/b"));
        let request =
            GyldRequest::parse(br#"{"verb":"fork","args":{"parent":"base","stream":"a-b"}}"#)
                .unwrap();
        plan(
            &layout,
            &request,
            None,
            "build-1",
            &crate::ask::AgentState::default(),
        )
        .unwrap()
    }

    #[test]
    fn lines_reach_the_sink_and_the_collected_output() {
        let sh = shim(
            "lines",
            "#!/bin/sh\necho one\necho two\necho oops >&2\nexit 3\n",
        );
        let mut seen: Vec<(String, String)> = Vec::new();
        let out = run_bounded(&sh, &a_plan(), Limits::default(), &mut |s, l| {
            seen.push((s.into(), l.into()));
        })
        .unwrap();
        assert_eq!(out.exit, 3);
        assert_eq!(out.stdout, "one\ntwo\n");
        assert_eq!(out.stderr, "oops\n");
        assert!(seen.contains(&("stdout".into(), "one".into())), "{seen:?}");
        assert!(seen.contains(&("stderr".into(), "oops".into())), "{seen:?}");
        let _ = std::fs::remove_dir_all(sh.parent().unwrap());
    }

    #[test]
    fn a_slow_host_times_out_as_an_error() {
        let sh = shim("slow", "#!/bin/sh\nsleep 5\n");
        let limits = Limits {
            timeout: Duration::from_millis(150),
            ..Limits::default()
        };
        let e = run_bounded(&sh, &a_plan(), limits, &mut |_, _| {}).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        let _ = std::fs::remove_dir_all(sh.parent().unwrap());
    }

    #[test]
    fn output_is_bounded_and_says_so() {
        let sh = shim(
            "loud",
            "#!/bin/sh\ni=0\nwhile [ $i -lt 200 ]; do echo aaaaaaaaaa; i=$((i+1)); done\n",
        );
        let limits = Limits {
            max_output_bytes: 64,
            ..Limits::default()
        };
        let out = run_bounded(&sh, &a_plan(), limits, &mut |_, _| {}).unwrap();
        assert!(out.truncated, "{out:?}");
        assert!(
            out.stdout.len() <= 64,
            "stdout was {} bytes",
            out.stdout.len()
        );
        assert!(out.stderr.contains("truncated"), "{out:?}");
        let _ = std::fs::remove_dir_all(sh.parent().unwrap());
    }

    #[test]
    fn a_missing_interpreter_is_an_error_not_a_panic() {
        let e = run_bounded(
            Path::new("/no/such/python"),
            &a_plan(),
            Limits::default(),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(e.contains("failed to spawn"), "{e}");
    }
}
