use super::*;

#[test]
fn the_key_comes_from_the_environment_first_and_a_mode_checked_file_second() {
    let dir = std::env::temp_dir().join(format!("glade-gyld-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("api-key");
    let none = Environment::default();
    // Every key is compared here and never printed: were the process's own
    // environment read after all, the key that came back would be real.
    let is = |env: &Environment, want: &str| discover_key(env, &path).ok().as_deref() == Some(want);
    let refusal = |env: &Environment| match discover_key(env, &path) {
        Ok(_) => panic!("a key was found (it is not printed) where a refusal was due"),
        Err(e) => e,
    };

    // No key in the snapshot and no file: the refusal names both places,
    // and never a value. What this process's own environment holds does
    // not matter; it is not read.
    let e = refusal(&none);
    assert!(e.says().contains("ANTHROPIC_API_KEY"), "{e}");
    assert!(e.says().contains("api-key"), "{e}");

    std::fs::write(&path, "sk-test-value\n# a comment\n").unwrap();
    modes::set(&path, 0o644);
    let said = refusal(&none).says();
    assert!(said.contains("readable"), "{said}");
    assert!(!said.contains("sk-test-value"), "a key never reaches data");

    modes::set(&path, 0o600);
    assert!(is(&none, "sk-test-value"), "the file's key");

    // The snapshot's key comes first, over a file that holds one too. A
    // blank one is no key, and the second name is asked next.
    let first = Environment::of([(KEY_ENV, "made-up-env-key")]);
    assert!(
        is(&first, "made-up-env-key"),
        "the snapshot's key, ahead of the file's"
    );
    let second = Environment::of([
        (KEY_ENV, "  "),
        (crate::agent::AUTH_TOKEN_ENV, "made-up-auth-token"),
    ]);
    assert!(
        is(&second, "made-up-auth-token"),
        "the second name's, under a blank first"
    );

    std::fs::write(&path, "\n").unwrap();
    modes::set(&path, 0o600);
    assert!(
        discover_key(&none, &path).is_err(),
        "an empty key file is no key"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mode setter, behind an explicit platform boundary like the checker
/// it exercises.
#[cfg(unix)]
mod modes {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    pub fn set(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

#[cfg(not(unix))]
mod modes {
    use std::path::Path;

    pub fn set(_path: &Path, _mode: u32) {}
}

#[test]
fn a_transport_error_never_carries_a_query_string() {
    let said = scrub("failed for url (https://api.anthropic.com/v1/messages?key=abc)");
    assert!(said.contains("/v1/messages?"), "{said}");
    assert!(!said.contains("key=abc"), "{said}");
}
