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
//! can never deadlock the wait. A shutdown ends every running host the same way
//! ([`Hosts`]).
//!
//! A host runs in the environment the supplier STARTED with and nothing else
//! ([`Environment::apply_to`]), with the two Python variables set over it: a
//! variable set in this process since never reaches one.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::environment::Environment;
use crate::signals;
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
    /// failure, a timeout or a shutdown, which the caller turns into data.
    fn run(
        &self,
        plan: &Plan,
        limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String>;

    /// The hosts this runner has running, which the supplier's shutdown ends.
    /// A runner that starts no process, as every test double, has none.
    fn hosts(&self) -> Hosts {
        Hosts::default()
    }
}

/// What a run ended by a shutdown, or refused because one has begun, says.
pub const SHUTTING_DOWN: &str = "the supplier is shutting down";

/// The hosts running now, which a shutdown ends (G4, 2026-09-28).
///
/// A host is counted from just before it is spawned until its run has reaped
/// it. Its run's own thread ends it, as a timeout does: the only thread that
/// signals a host's tree is the one that reaps the host, and it signals first,
/// so no signal can reach a process that has since taken the host's id.
/// [`Hosts::end_all`] tells every run to end its host, waits for them, and
/// refuses every host after it.
///
/// The supplier holds this, and so does the runner that counts its hosts in: it
/// is shared between the two, never global.
#[derive(Clone, Debug, Default)]
pub struct Hosts(Arc<Tally>);

#[derive(Debug, Default)]
struct Tally {
    /// How many hosts are counted in.
    running: Mutex<usize>,
    /// Set once, with `running` locked, and never cleared.
    ending: AtomicBool,
    /// Told whenever a run counts its host out.
    counted_out: Condvar,
}

/// One host, counted in until this drops: when its run has returned, or
/// panicked.
struct Counted<'h>(&'h Tally);

impl Hosts {
    /// Count a host in, or refuse it once the end has begun.
    fn count_in(&self) -> Result<Counted<'_>, String> {
        let tally = &*self.0;
        let mut running = tally.running.lock().unwrap_or_else(|e| e.into_inner());
        if tally.ending.load(Ordering::SeqCst) {
            return Err(format!("not started: {SHUTTING_DOWN}"));
        }
        *running += 1;
        Ok(Counted(tally))
    }

    /// End every running host, refuse every host after, and wait up to
    /// `within` for the runs to reap theirs. Answers how many are still running:
    /// none, unless one outlived the wait.
    pub fn end_all(&self, within: Duration) -> usize {
        let tally = &*self.0;
        let deadline = Instant::now() + within;
        let mut running = tally.running.lock().unwrap_or_else(|e| e.into_inner());
        tally.ending.store(true, Ordering::SeqCst);
        while *running > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            running = match tally.counted_out.wait_timeout(running, deadline - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        *running
    }
}

impl Counted<'_> {
    /// Has the end begun?
    fn ending(&self) -> bool {
        self.0.ending.load(Ordering::SeqCst)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        let mut running = self.0.running.lock().unwrap_or_else(|e| e.into_inner());
        *running = running.saturating_sub(1);
        self.0.counted_out.notify_all();
    }
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
    /// Its hosts, counted in while they run.
    hosts: Hosts,
}

impl PythonRunner {
    pub fn new(python: PathBuf, env: Environment) -> PythonRunner {
        PythonRunner {
            python,
            env,
            hosts: Hosts::default(),
        }
    }
}

impl Runner for PythonRunner {
    fn run(
        &self,
        plan: &Plan,
        limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        run_counted(&self.hosts, &self.python, &self.env, plan, limits, on_line)
    }

    fn hosts(&self) -> Hosts {
        self.hosts.clone()
    }
}

/// How long a run reads its pipes before it looks up at the clock and the end.
const TICK: Duration = Duration::from_millis(25);

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
    run_counted(&Hosts::default(), python, env, plan, limits, on_line)
}

/// [`run_bounded`], with the host counted in `hosts` while it runs, so
/// [`Hosts::end_all`] ends it.
fn run_counted(
    hosts: &Hosts,
    python: &Path,
    env: &Environment,
    plan: &Plan,
    limits: Limits,
    on_line: &mut dyn FnMut(&str, &str),
) -> Result<RunOutput, String> {
    let counted = hosts.count_in()?;
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
    signals::unblocked_in_child(&mut command);

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
    let mut ended = false;
    let mut exit: Option<i32> = None;

    loop {
        match rx.recv_timeout(TICK) {
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
        if counted.ending() {
            ended = true;
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
    }

    // Both pipes have closed, which a host does by exiting, or by closing them
    // and running on. So it is watched rather than waited for, and the clock
    // and the end still reach it.
    while !(timed_out || ended) && exit.is_none() {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = Some(status.code().unwrap_or(-1));
            }
            Ok(None) if counted.ending() => {
                ended = true;
            }
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
            }
            Ok(None) => {
                std::thread::sleep(TICK);
            }
            Err(e) => {
                return Err(format!("wait failed: {e}"));
            }
        }
    }

    if timed_out || ended {
        host.end(&mut child);
        let _ = child.wait();
    }
    for reader in readers {
        let _ = reader.join();
    }
    if ended {
        return Err(format!("ended: {SHUTTING_DOWN}"));
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

/// A host's WHOLE process tree, which a timeout or a shutdown ends, behind an
/// explicit module boundary per platform (the workzone's conditional-compilation
/// rule).
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

    /// A stand-in for a Gyld host, so the bounds are exercised deterministically
    /// without a Python interpreter or a Gyld checkout: `body`, one of
    /// [`stand_in`]'s scripts, as a file this platform runs.
    fn shim(tag: &str, body: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("glade-gyld-shim-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(stand_in::FILE);
        std::fs::write(&path, body).unwrap();
        stand_in::runnable(&path);
        path
    }

    /// The stand-in hosts, one set per platform (G5, 2026-09-28): `/bin/sh`
    /// scripts on unix, and on Windows, which cannot run those, `.cmd` scripts
    /// doing the same, which std runs through `cmd.exe`. The Windows set is what
    /// puts the job-object `tree` under test.
    #[cfg(unix)]
    mod stand_in {
        use std::path::Path;

        use crate::environment::Environment;

        pub const FILE: &str = "shim";

        /// Two lines out, one on stderr, and exit 3.
        pub const LINES: &str = "#!/bin/sh\necho one\necho two\necho oops >&2\nexit 3\n";

        /// Ends at once when `WARM` is set. Otherwise it starts a child that
        /// holds the output pipes, says `started` and waits for that child.
        pub const SLOW: &str =
            "#!/bin/sh\n[ -n \"$WARM\" ] && exit 0\nsleep 30 &\necho started\nwait\n";

        /// 200 lines of ten bytes.
        pub const LOUD: &str =
            "#!/bin/sh\ni=0\nwhile [ $i -lt 200 ]; do echo aaaaaaaaaa; i=$((i+1)); done\n";

        /// A script runs once it is executable.
        pub fn runnable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// `vars` and nothing else: these scripts need nothing of an environment.
        pub fn env(vars: &[(&str, &str)]) -> Environment {
            Environment::of(vars.iter().copied())
        }
    }

    #[cfg(windows)]
    mod stand_in {
        use std::ffi::OsString;
        use std::path::Path;

        use crate::environment::Environment;

        pub const FILE: &str = "shim.cmd";

        /// Two lines out, one on stderr, and exit 3.
        pub const LINES: &str = "@echo off\r\necho one\r\necho two\r\n>&2 echo oops\r\nexit 3\r\n";

        /// As unix's. cmd has no `sleep`, so `PING.EXE` waits, a second a ping:
        /// the child's output goes nowhere, and its stderr, the host's, stays
        /// open for as long as it runs. (`timeout.exe` refuses a null stdin.)
        pub const SLOW: &str = "@echo off\r\n\
            if defined WARM exit 0\r\n\
            start \"\" /b \"%SystemRoot%\\System32\\PING.EXE\" -n 31 127.0.0.1 >nul\r\n\
            echo started\r\n\
            \"%SystemRoot%\\System32\\PING.EXE\" -n 31 127.0.0.1 >nul\r\n";

        /// 200 lines of ten bytes.
        pub const LOUD: &str = "@echo off\r\nfor /l %%i in (1,1,200) do echo aaaaaaaaaa\r\n";

        /// A `.cmd` script runs as it is.
        pub fn runnable(_path: &Path) {}

        /// `vars` over this process's own environment. cmd.exe runs no batch
        /// file without `SystemRoot`: it exits 1 and says nothing (G5b,
        /// 2026-09-28). `PING.EXE` is found, and finds its network stack,
        /// through it too.
        pub fn env(vars: &[(&str, &str)]) -> Environment {
            let given = vars
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)));
            Environment::of(given.chain(std::env::vars_os()))
        }
    }

    /// No environment at all, for a host that never runs. A stand-in that runs
    /// is given [`stand_in::env`], which on Windows is not empty.
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

    /// Run `plan` through `runner` on a thread of its own, handing each output
    /// line to the first receiver and the answer to the second.
    fn running(
        runner: PythonRunner,
        plan: Plan,
    ) -> (
        mpsc::Receiver<String>,
        mpsc::Receiver<Result<RunOutput, String>>,
    ) {
        let (said, lines) = mpsc::channel();
        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || {
            let out = runner.run(&plan, Limits::default(), &mut |_, line| {
                let _ = said.send(line.to_string());
            });
            let _ = answered.send(out);
        });
        (lines, answer)
    }

    #[test]
    fn lines_reach_the_sink_and_the_collected_output() {
        let sh = shim("lines", stand_in::LINES);
        let env = stand_in::env(&[]);
        let mut seen: Vec<(String, String)> = Vec::new();
        let out = run_bounded(&sh, &env, &a_plan(), Limits::default(), &mut |s, l| {
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
        let sh = shim("slow", stand_in::SLOW);
        // A new script's first run can wait on the system's check of a new
        // executable for longer than the timeout below (0.1 to 0.4 s on macOS),
        // and a host killed before it has started its child proves nothing. One
        // run that ends at once pays for that check here.
        let warm = stand_in::env(&[("WARM", "1")]);
        run_bounded(&sh, &warm, &a_plan(), Limits::default(), &mut |_, _| {}).unwrap();

        let limits = Limits {
            timeout: Duration::from_millis(150),
            ..Limits::default()
        };
        let cold = stand_in::env(&[]);
        let mut seen: Vec<String> = Vec::new();
        let began = Instant::now();
        let e = run_bounded(&sh, &cold, &a_plan(), limits, &mut |_, line| {
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

    /// Ending the hosts ends a running host's WHOLE tree, as a timeout does
    /// (G4, 2026-09-28). The stand-in host has started a child that holds the
    /// output pipes, so a run that answers at all is the proof that the child is
    /// gone with the host.
    #[test]
    fn ending_the_hosts_ends_a_running_hosts_tree() {
        let sh = shim("ended", stand_in::SLOW);
        let runner = PythonRunner::new(sh.clone(), stand_in::env(&[]));
        let hosts = runner.hosts();
        let (lines, answer) = running(runner, a_plan());
        let first = lines.recv_timeout(Duration::from_secs(10));
        assert_eq!(
            first.as_deref(),
            Ok("started"),
            "the host started its child"
        );

        let began = Instant::now();
        let left = hosts.end_all(Duration::from_secs(5));
        let out = answer.recv_timeout(Duration::from_secs(5));
        let took = began.elapsed();
        assert_eq!(left, 0, "a host outlived the end");
        let e = out
            .expect("the run answered once its host was ended")
            .unwrap_err();
        assert!(e.contains(SHUTTING_DOWN), "{e}");
        assert!(took < Duration::from_secs(5), "the end took {took:?}");
        let _ = std::fs::remove_dir_all(sh.parent().unwrap());
    }

    /// Once the hosts have been ended no host starts: a run asked for after
    /// that is refused as data and spawns nothing, because a host started then
    /// would outlive the supplier (G4).
    #[test]
    fn no_host_starts_once_the_hosts_are_ended() {
        let sh = shim("after", stand_in::LINES);
        let runner = PythonRunner::new(sh.clone(), none());
        assert_eq!(
            runner.hosts().end_all(Duration::ZERO),
            0,
            "none was running"
        );
        let e = runner
            .run(&a_plan(), Limits::default(), &mut |_, _| {})
            .unwrap_err();
        assert!(e.contains(SHUTTING_DOWN), "{e}");
        let _ = std::fs::remove_dir_all(sh.parent().unwrap());
    }

    #[test]
    fn output_is_bounded_and_says_so() {
        let sh = shim("loud", stand_in::LOUD);
        let limits = Limits {
            max_output_bytes: 64,
            ..Limits::default()
        };
        let env = stand_in::env(&[]);
        let out = run_bounded(&sh, &env, &a_plan(), limits, &mut |_, _| {}).unwrap();
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

        /// A stand-in host that sends both its output pipes to /dev/null, says
        /// so in the file `MARK` names, and sleeps on; or, when `WARM` is set,
        /// ends at once.
        const CLOSES_ITS_OUTPUT: &str = "#!/bin/sh\n[ -n \"$WARM\" ] && exit 0\n\
            exec >/dev/null 2>&1\n[ -n \"$MARK\" ] && : > \"$MARK\"\nexec /bin/sleep 20\n";

        /// A host that has closed both its output pipes and runs on is past
        /// the loop that reads them, and ending the hosts still ends it: its
        /// run watches for the end while it waits for the host (G4).
        #[test]
        fn ending_the_hosts_ends_a_host_that_closed_its_output() {
            let sh = shim("closed", CLOSES_ITS_OUTPUT);
            let mark = sh.with_file_name("closed");
            let runner = PythonRunner::new(sh.clone(), Environment::of([("MARK", &mark)]));
            let hosts = runner.hosts();
            let (_, answer) = running(runner, a_plan());
            let began = Instant::now();
            while !mark.exists() && began.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(mark.exists(), "the host closed its output");

            let left = hosts.end_all(Duration::from_secs(5));
            let out = answer.recv_timeout(Duration::from_secs(5));
            assert_eq!(left, 0, "a host outlived the end");
            let e = out
                .expect("the run answered once its host was ended")
                .unwrap_err();
            assert!(e.contains(SHUTTING_DOWN), "{e}");
            let _ = std::fs::remove_dir_all(sh.parent().unwrap());
        }

        /// The same host is bounded by the timeout too, which the module
        /// promises of every run: its run watches the clock while it waits.
        #[test]
        fn a_host_that_closed_its_output_still_times_out() {
            let sh = shim("closed-slow", CLOSES_ITS_OUTPUT);
            // Paid here, the system's check of a new executable cannot hold
            // the pipes open past the timeout below (a_slow_host_times_out_as_an_error).
            let warm = Environment::of([("WARM", "1")]);
            run_bounded(&sh, &warm, &a_plan(), Limits::default(), &mut |_, _| {}).unwrap();
            let limits = Limits {
                timeout: Duration::from_secs(1),
                ..Limits::default()
            };
            let began = Instant::now();
            let ran = run_bounded(&sh, &none(), &a_plan(), limits, &mut |_, _| {});
            let took = began.elapsed();
            let e = ran.unwrap_err();
            assert!(e.contains("timed out"), "{e}");
            assert!(
                took < Duration::from_secs(5),
                "the run answered after {took:?}"
            );
            let _ = std::fs::remove_dir_all(sh.parent().unwrap());
        }
    }
}
