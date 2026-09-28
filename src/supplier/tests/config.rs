use super::*;

/// The key check reads the config's snapshot, never the process's own
/// environment, and a config holding a key prints its name and no value.
#[test]
fn the_key_check_reads_the_configured_environment_and_prints_no_value() {
    let bundle = root("key-env");
    let mut config = GyldConfig::new("ws://x", bundle.join("gyld"), bundle.clone());
    assert!(
        !agent_state(&config, None).key,
        "no snapshot and no key file is no key"
    );

    config.env = Environment::of([(ask::KEY_ENV, "made-up-api-key")]);
    assert!(agent_state(&config, None).key, "the snapshot's key counts");
    let printed = format!("{config:?}");
    assert!(printed.contains(ask::KEY_ENV), "{printed}");
    assert!(
        !printed.contains("made-up-api-key"),
        "a value is in no debug print: {printed}"
    );

    config.env = Environment::of([
        (ask::KEY_ENV, " "),
        (agent::AUTH_TOKEN_ENV, "made-up-auth-token"),
    ]);
    assert!(
        agent_state(&config, None).key,
        "so does the second name's, under a blank first"
    );

    config.env = Environment::of([(ask::KEY_ENV, " ")]);
    assert!(
        !agent_state(&config, None).key,
        "a blank variable is no key"
    );
    let _ = std::fs::remove_dir_all(&bundle);
}
