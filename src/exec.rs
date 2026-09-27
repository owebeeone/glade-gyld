//! The bounded Gyld-host runner.
//!
//! One implementation serves both paths: a synchronous request collects the
//! output and answers with it, and a `stream:true` request runs the SAME way on
//! a blocking task with a line sink that appends each line to the log surface as
//! it arrives. There is no second command shape to keep in step.
//!
//! Everything is bounded. Wall clock: a hard timeout, after which the host is
//! killed with every process it started (the `tree` module) and the failure is
//! data. Output: a per-stream byte budget, after which lines stop accumulating
//! and the answer says so. Pipes drain on their own threads, so a chatty host
//! can never deadlock the wait.
//!
//! A host runs in the environment the supplier STARTED with and nothing else
//! ([`Environment::apply_to`]), with the two Python variables set over it: a
//! variable set in this process since never reaches one.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::environment::Environment;
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
    /// The environment the supplier started with: all a host run is given,
    /// with the two Python variables set over it.
    pub env: Environment,
}

impl PythonRunner {
    pub fn new(python: PathBuf, env: Environment) -> PythonRunner {
        PythonRunner { python, env }
    }
}

impl Runner for PythonRunner {
    fn run(
        &self,
        plan: &Plan,
        limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        run_bounded(&self.python, &self.env, plan, limits, on_line)
    }
}

/// Spawn the host, drain both pipes on threads, enforce both bounds.
///
/// The host is given `env` and nothing else. The snapshot goes on FIRST,
/// because `env_clear()` would drop the two Python variables set before it.
pub fn run_bounded(
    python: &Path,
    env: &Environment,
    plan: &Plan,
    limits: Limits,
    on_line: &mut dyn FnMut(&str, &str),
) -> Result<RunOutput, String> {
    let mut command = std::process::Command::new(python);
    env.apply_to(&mut command)
        .arg("-B")
        .args(&plan.argv)
        .current_dir(&plan.cwd)
        .env("PYTHONPATH", &plan.pythonpath)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (mut child, host) = tree::spawn(&mut command)
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
        host.end(&mut child);
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

/// A host's WHOLE process tree, which a timeout ends, behind an explicit module
/// boundary per platform (the workzone's conditional-compilation rule).
///
/// Killing the host alone was not enough (G3, 2026-09-27): a process the host
/// started inherits its output pipes and holds them open for as long as it
/// lives, so the drainers, and with them the run, waited for it, and it went on
/// running after the timeout that was meant to end it.
///
/// std signals no process group and makes no job object, and a crate for the
/// one call each needs is more than it is worth: each is declared here, from
/// the system library std already links.
#[cfg(unix)]
mod tree {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    extern "C" {
        /// `kill(2)`: a negative pid signals that process group.
        fn kill(pid: i32, signal: i32) -> i32;
    }

    /// `SIGKILL`, which is 9 on every unix.
    const SIGKILL: i32 = 9;

    /// The host's process group, which every process it starts joins unless
    /// it leaves on purpose. Its id is the host's own pid.
    pub struct Tree(u32);

    /// Spawn the host as the leader of a process group of its own.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        let child = command.process_group(0).spawn()?;
        let group = child.id();
        Ok((child, Tree(group)))
    }

    impl Tree {
        /// Kill the whole group, and the host itself should the group not be
        /// signalled. Called BEFORE the host is waited for: until then the
        /// group's id is the unreaped host's pid, which nothing else can have.
        pub fn end(&self, child: &mut Child) {
            if let Ok(group) = i32::try_from(self.0) {
                // SAFETY: kill(2) is handed two integers and reads no memory.
                unsafe {
                    kill(-group, SIGKILL);
                }
            }
            let _ = child.kill();
        }
    }
}

#[cfg(windows)]
mod tree {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::{Child, Command};

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
    }

    /// A job object holding the host, which every process it starts joins.
    /// `None` when none could be made or the host could not be put in it: then
    /// the host alone is killed, as before.
    ///
    /// The host joins it just after it starts, so a process it started before
    /// then would stay outside; a Python host starts nothing that early.
    pub struct Tree(Option<OwnedHandle>);

    /// Spawn the host and put it in a job object of its own.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        let child = command.spawn()?;
        let job = job_for(&child);
        Ok((child, Tree(job)))
    }

    fn job_for(child: &Child) -> Option<OwnedHandle> {
        // SAFETY: no security attributes and no name, both of which may be null.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: a handle CreateJobObjectW has just returned, owned by nothing
        // else, which the OwnedHandle closes.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: two live handles, the job's and the one std holds for the host.
        let assigned =
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
        match assigned != 0 {
            true => Some(job),
            false => None,
        }
    }

    impl Tree {
        /// Kill every process in the job, and the host itself should there be
        /// no job.
        pub fn end(&self, child: &mut Child) {
            if let Some(job) = self.0.as_ref() {
                // SAFETY: a live job handle this Tree owns.
                unsafe {
                    TerminateJobObject(job.as_raw_handle(), 1);
                }
            }
            let _ = child.kill();
        }
    }
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

    /// No environment at all: these shims need nothing from one.
    fn none() -> Environment {
        Environment::default()
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
            "run-7",
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
        let out = run_bounded(&sh, &none(), &a_plan(), Limits::default(), &mut |s, l| {
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

    /// The timeout ends the host's WHOLE process tree. The stand-in host starts
    /// a child of its own, which holds the output pipes open for as long as it
    /// lives: a timeout that killed the host alone left the run waiting for that
    /// child to finish (G3, 2026-09-27), so an answer long before it would have
    /// is the proof the child is gone.
    #[test]
    fn a_slow_host_times_out_as_an_error() {
        let sh = shim(
            "slow",
            "#!/bin/sh\n[ -n \"$WARM\" ] && exit 0\nsleep 30 &\necho started\nwait\n",
        );
        // A new script's first run can wait on the system's check of a new
        // executable for longer than the timeout below (0.1 to 0.4 s on macOS),
        // and a host killed before it has started its child proves nothing. One
        // run that ends at once pays for that check here.
        let warm = Environment::of([("WARM", "1")]);
        run_bounded(&sh, &warm, &a_plan(), Limits::default(), &mut |_, _| {}).unwrap();

        let limits = Limits {
            timeout: Duration::from_millis(150),
            ..Limits::default()
        };
        let mut seen: Vec<String> = Vec::new();
        let began = Instant::now();
        let e = run_bounded(&sh, &none(), &a_plan(), limits, &mut |_, line| {
            seen.push(line.into());
        })
        .unwrap_err();
        let took = began.elapsed();
        assert!(e.contains("timed out"), "{e}");
        assert_eq!(seen, ["started"], "the host started its child in time");
        assert!(
            took < Duration::from_secs(5),
            "the run answered after {took:?}: the host's own child outlived the timeout"
        );
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
        let out = run_bounded(&sh, &none(), &a_plan(), limits, &mut |_, _| {}).unwrap();
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
            &none(),
            &a_plan(),
            Limits::default(),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(e.contains("failed to spawn"), "{e}");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use crate::environment::tests::unix::{names_past_the_shell, set};

        /// A host is given the snapshot and the two Python variables set over
        /// it, and nothing of this process's own environment. The stand-in host
        /// lists its environment; only the names are compared.
        #[test]
        fn a_host_is_given_the_snapshot_and_its_two_python_variables_only() {
            let sh = shim("env", "#!/bin/sh\nexec /usr/bin/env -0\n");
            let env = Environment::of([("GYLD_MADE_UP_SNAPSHOT", "made-up")]);
            let out = run_bounded(&sh, &env, &a_plan(), Limits::default(), &mut |_, _| {})
                .expect("the stand-in host ran");
            assert_eq!(out.exit, 0, "the stand-in host exited {}", out.exit);
            assert_eq!(
                names_past_the_shell(out.stdout.as_bytes()),
                set(&[
                    "GYLD_MADE_UP_SNAPSHOT",
                    "PYTHONDONTWRITEBYTECODE",
                    "PYTHONPATH"
                ])
            );
            let _ = std::fs::remove_dir_all(sh.parent().unwrap());
        }
    }
}
