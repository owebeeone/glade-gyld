//! The shutdown signals, taken the way grazel takes its own: no C handler, and
//! no process id kept in a static (G4, 2026-09-28).
//!
//! On unix they are SIGINT and SIGTERM. [`ShutdownSignals::block`] blocks both
//! in the calling thread, and `main` calls it first, before any thread exists,
//! so every thread the program starts inherits the block: neither signal runs a
//! handler or ends the process by default, and only the one thread
//! [`ShutdownSignals::watch`] starts takes them, with `sigwait`. This is
//! process-wide state, set once at the entry point, like the arguments. A child
//! would inherit the block too, and blocked, the two could never stop it, so
//! every spawn site unblocks them in the child ([`unblocked_in_child`]).
//!
//! On Windows they are a console's Ctrl-C and close events. A process learns of
//! those only through a handler routine, and tokio's listeners are that, as
//! they were before; nothing is blocked, so a child has nothing to undo.
//!
//! std blocks no signal and waits on none, and a crate for the four calls is
//! more than it is worth: each is declared here, from the system library std
//! already links, as `exec`'s `tree` declares its own.

pub(crate) use platform::unblocked_in_child;
pub use platform::ShutdownSignals;

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use tokio::sync::oneshot;

    /// A `sigset_t`, opaque: only the system's own functions read or write
    /// one. 128 bytes holds the largest there is (glibc's), aligned as any of
    /// them needs.
    #[derive(Clone, Copy)]
    #[repr(C, align(8))]
    struct SigSet([u8; 128]);

    extern "C" {
        fn sigemptyset(set: *mut SigSet) -> i32;
        fn sigaddset(set: *mut SigSet, signal: i32) -> i32;
        fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
        fn sigwait(set: *const SigSet, signal: *mut i32) -> i32;
    }

    /// `SIGINT` and `SIGTERM`, which are 2 and 15 on every unix.
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    /// `pthread_sigmask`'s `how`, numbered from 0 on Linux's generic ABI.
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "mips",
            target_arch = "mips32r6",
            target_arch = "mips64",
            target_arch = "mips64r6",
            target_arch = "sparc",
            target_arch = "sparc64"
        ))
    ))]
    mod how {
        pub const BLOCK: i32 = 0;
        pub const UNBLOCK: i32 = 1;
    }

    /// `pthread_sigmask`'s `how`, numbered from 1 on macOS and the BSDs. A unix
    /// named in neither module has no `how` and does not build, which is better
    /// than a guess.
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    mod how {
        pub const BLOCK: i32 = 1;
        pub const UNBLOCK: i32 = 2;
    }

    /// SIGINT and SIGTERM, blocked in the thread that made this.
    pub struct ShutdownSignals(SigSet);

    impl ShutdownSignals {
        /// Block SIGINT and SIGTERM in the calling thread. Called first thing
        /// in `main`, before any thread exists, it blocks them in every thread
        /// the program will start.
        pub fn block() -> ShutdownSignals {
            let set = shutdown_set();
            // SAFETY: a set `shutdown_set` filled, and no old set asked for.
            unsafe {
                pthread_sigmask(how::BLOCK, &set, std::ptr::null_mut());
            }
            ShutdownSignals(set)
        }

        /// Start the one thread that takes the two, with `sigwait`, and answer
        /// with what resolves once it has taken one. One is enough: the program
        /// is stopping, and a second stays blocked until it has.
        pub fn watch(self) -> io::Result<oneshot::Receiver<()>> {
            let (taken, receiver) = oneshot::channel();
            let set = self.0;
            std::thread::Builder::new()
                .name("shutdown-signals".into())
                .spawn(move || {
                    loop {
                        let mut signal = 0;
                        // SAFETY: a filled set and a live out-parameter.
                        // `sigwait` answers 0 once it has taken one of the set.
                        if unsafe { sigwait(&set, &mut signal) } == 0 {
                            break;
                        }
                    }
                    let _ = taken.send(());
                })?;
            Ok(receiver)
        }
    }

    /// SIGINT and SIGTERM, as a set.
    fn shutdown_set() -> SigSet {
        let mut set = SigSet([0; 128]);
        // SAFETY: a set at least as large as this system's `sigset_t`, which
        // these two fill in.
        unsafe {
            sigemptyset(&mut set);
            sigaddset(&mut set, SIGINT);
            sigaddset(&mut set, SIGTERM);
        }
        set
    }

    /// Unblock SIGINT and SIGTERM in the child `command` starts, between fork
    /// and exec. std hands a child its parent's signal mask, and every thread
    /// here blocks the two.
    pub fn unblocked_in_child(command: &mut Command) -> &mut Command {
        let set = shutdown_set();
        // SAFETY: the hook runs in the forked child before exec and makes one
        // call, `pthread_sigmask`, which is async-signal-safe, on a set built
        // beforehand.
        unsafe {
            command.pre_exec(move || {
                pthread_sigmask(how::UNBLOCK, &set, std::ptr::null_mut());
                Ok(())
            })
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::process::Command;

    use tokio::sync::oneshot;

    /// Nothing to hold: Windows has no signal mask.
    pub struct ShutdownSignals(());

    impl ShutdownSignals {
        /// Nothing to block.
        pub fn block() -> ShutdownSignals {
            ShutdownSignals(())
        }

        /// Listen for the console's Ctrl-C and close events, and answer with
        /// what resolves on the first. Called inside the runtime. tokio holds a
        /// close event's handler until the process exits, which gives the
        /// shutdown its time.
        pub fn watch(self) -> io::Result<oneshot::Receiver<()>> {
            let mut interrupt = tokio::signal::windows::ctrl_c()?;
            let mut close = tokio::signal::windows::ctrl_close()?;
            let (taken, receiver) = oneshot::channel();
            tokio::spawn(async move {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = close.recv() => {}
                }
                let _ = taken.send(());
            });
            Ok(receiver)
        }
    }

    /// Nothing to undo in a child.
    pub fn unblocked_in_child(command: &mut Command) -> &mut Command {
        command
    }
}
