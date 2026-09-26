//! A child process is given the environment the supplier STARTED with, and a
//! variable set in the process after that reaches no child (ProcessGlobalsPlan
//! Step 3.1). The process-globals checker cannot see `env_clear()`, so this is
//! one of the tests that hold it.
//!
//! **One test in this file, on purpose.** It sets a variable in this process
//! after the snapshot is taken, and setting one while another test reads the
//! environment on another thread is a data race. A file of its own is a test
//! binary of its own, so nothing runs beside it.
//!
//! Both spawn sites are driven as the supplier drives them: a Gyld host through
//! `PythonRunner`, and `gh auth token` through `github::discover_with_gh`. Each
//! child is a shell script that lists its environment; only NAMES are compared,
//! and every value here is made up.

#[cfg(unix)]
mod unix {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use glade_gyld::github::{self, Source};
    use glade_gyld::{Environment, Limits, Plan, PythonRunner, Runner};

    /// The names a POSIX shell adds of its own to a script's environment.
    const SHELL_ADDED: [&str; 4] = ["OLDPWD", "PWD", "SHLVL", "_"];

    /// The NAMES in an `env -0` listing, less the shell's own. Entries are
    /// NUL-separated, so a value with a newline in it cannot pass for a name.
    /// No value leaves here.
    fn names(listing: &[u8]) -> BTreeSet<String> {
        listing
            .split(|byte| *byte == 0)
            .filter_map(|entry| {
                let at = entry.iter().position(|byte| *byte == b'=')?;
                Some(String::from_utf8_lossy(&entry[..at]).trim().to_string())
            })
            .filter(|name| !name.is_empty() && !SHELL_ADDED.contains(&name.as_str()))
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// An executable shell script at `path`.
    fn script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_variable_set_after_start_reaches_no_child() {
        let dir = std::env::temp_dir().join(format!("glade-gyld-child-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // A stand-in Gyld host that lists its environment, and a stand-in `gh`
        // that writes its listing where the snapshot says and answers with a
        // made-up token.
        let host = dir.join("host");
        script(&host, "#!/bin/sh\nexec /usr/bin/env -0\n");
        script(
            &bin.join("gh"),
            "#!/bin/sh\n/usr/bin/env -0 > \"$GYLD_MADE_UP_REPORT\"\necho made-up-token\n",
        );
        let report = dir.join("gh-environment");

        // Start: the snapshot is taken, as `main` takes it.
        let env = Environment::of([
            ("PATH", bin.display().to_string()),
            ("GYLD_MADE_UP_SNAPSHOT", "made-up".to_string()),
            ("GYLD_MADE_UP_REPORT", report.display().to_string()),
        ]);
        // After start: a variable set in this process, and nowhere else.
        std::env::set_var("GYLD_MADE_UP_AFTER_START", "made-up-after");
        assert!(
            std::env::var_os("GYLD_MADE_UP_AFTER_START").is_some(),
            "this process has it now"
        );

        // A Gyld host, through the runner the supplier uses.
        let plan = Plan {
            verb: "list".into(),
            write: None,
            merge: None,
            argv: Vec::new(),
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
        let out = PythonRunner::new(host, env.clone())
            .run(&plan, limits, &mut |_, _| {})
            .expect("the stand-in host ran");
        let host_saw = names(out.stdout.as_bytes());

        // `gh auth token`, through the discovery the supplier makes.
        let token = github::discover_with_gh(&env);
        let gh_saw = std::fs::read(&report)
            .map(|listing| names(&listing))
            .unwrap_or_default();

        let leaked: Vec<&str> = [("the host", &host_saw), ("gh", &gh_saw)]
            .into_iter()
            .filter(|(_, saw)| saw.contains("GYLD_MADE_UP_AFTER_START"))
            .map(|(child, _)| child)
            .collect();
        assert!(
            leaked.is_empty(),
            "a variable set after start reached {leaked:?}"
        );
        assert!(
            token.found() && token.source == Source::Gh,
            "the stand-in gh on the snapshot's PATH answered: {token:?}"
        );
        assert_eq!(
            host_saw,
            set(&[
                "GYLD_MADE_UP_REPORT",
                "GYLD_MADE_UP_SNAPSHOT",
                "PATH",
                "PYTHONDONTWRITEBYTECODE",
                "PYTHONPATH"
            ]),
            "the host is given the snapshot and its two Python variables only"
        );
        assert_eq!(
            gh_saw,
            set(&["GYLD_MADE_UP_REPORT", "GYLD_MADE_UP_SNAPSHOT", "PATH"]),
            "gh is given the snapshot only"
        );

        std::env::remove_var("GYLD_MADE_UP_AFTER_START");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
