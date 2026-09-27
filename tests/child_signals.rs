//! A child starts with SIGINT and SIGTERM unblocked (G4, 2026-09-28).
//!
//! glade-gyld blocks the two in every thread, from the start, and takes them
//! with `sigwait` (`ShutdownSignals`). std hands a child its parent's signal
//! mask, and blocked, the two could never stop the child, so each spawn site
//! unblocks them there. Both are driven as the supplier drives them: a Gyld host
//! through `PythonRunner`, and `gh auth token` through
//! `github::discover_with_gh`. Each child is a shell script that sends itself
//! the signal and says `survived` only if that did not end it.
//!
//! **One test in this file, on purpose.** It blocks the two on its own thread,
//! and a file of its own is a test binary of its own.

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use glade_gyld::github;
    use glade_gyld::{Environment, Limits, Plan, PythonRunner, Runner, ShutdownSignals};

    /// An executable shell script at `path`.
    fn script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Does `signal` end a shell that sends it to itself, with nothing blocked?
    /// A process started with SIGINT ignored, as a background job is, hands the
    /// same to its children, and then no mask can matter.
    fn ends_a_child(signal: &str) -> bool {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -{signal} $$; echo survived"))
            .output()
            .expect("run /bin/sh");
        !String::from_utf8_lossy(&out.stdout).contains("survived")
    }

    #[test]
    fn a_child_starts_with_the_shutdown_signals_unblocked() {
        let dir =
            std::env::temp_dir().join(format!("glade-gyld-child-signals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // The host is handed `-B <argv…>`, so its signal is its second
        // argument; `gh` is handed `auth token`, so its signal comes in the
        // environment, and it leaves a file to say it ran.
        let host = dir.join("host");
        script(&host, "#!/bin/sh\nkill -\"$2\" $$\necho survived\n");
        script(
            &bin.join("gh"),
            "#!/bin/sh\n: > \"$GYLD_MADE_UP_RAN\"\nkill -\"$GYLD_MADE_UP_SIGNAL\" $$\necho survived\n",
        );

        let signals: Vec<&str> = ["INT", "TERM"]
            .into_iter()
            .filter(|signal| {
                let ends = ends_a_child(signal);
                if !ends {
                    eprintln!("SKIP SIG{signal}: it is ignored here, so no mask can matter");
                }
                ends
            })
            .collect();
        assert!(signals.contains(&"TERM"), "SIGTERM ends a child here");

        // Blocked on this thread, as on every glade-gyld thread: each child
        // below starts from this thread's mask.
        let _blocked = ShutdownSignals::block();

        for signal in signals {
            let ran = dir.join(format!("gh-ran-{signal}"));
            let env = Environment::of([
                ("PATH", bin.display().to_string()),
                ("GYLD_MADE_UP_SIGNAL", signal.to_string()),
                ("GYLD_MADE_UP_RAN", ran.display().to_string()),
            ]);

            // A Gyld host, through the runner the supplier uses.
            let plan = Plan {
                verb: "list".into(),
                write: None,
                merge: None,
                argv: vec![signal.to_string()],
                cwd: dir.clone(),
                pythonpath: "src:.".into(),
                output_dir: None,
                read: None,
                consult: None,
                overlay: None,
                stream: None,
            };
            let limits = Limits {
                timeout: Duration::from_secs(30),
                max_output_bytes: 1 << 20,
            };
            let out = PythonRunner::new(host.clone(), env.clone())
                .run(&plan, limits, &mut |_, _| {})
                .expect("the stand-in host ran");
            assert!(
                !out.stdout.contains("survived") && out.exit != 0,
                "SIG{signal} reached the host blocked: it exited {} and said {:?}",
                out.exit,
                out.stdout
            );

            // `gh auth token`, through the discovery the supplier makes.
            let token = github::discover_with_gh(&env);
            assert!(ran.exists(), "the stand-in gh ran");
            assert!(
                !token.found(),
                "SIG{signal} reached gh blocked: it survived to answer"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
