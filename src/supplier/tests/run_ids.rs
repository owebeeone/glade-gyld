use super::*;

/// A minted run id has to be unique across supplier RESTARTS, not just
/// within one process. The `gyld.output` log it keys lives in the node's
/// persistent store, so a second session that reused `run-1` would hand a
/// fresh run the FIRST session's terminal record — and report that run's
/// outcome for this one.
#[test]
fn two_sessions_mint_different_ids_for_the_same_counter() {
    assert_ne!(mint_run_id("mfk3x9p", 1), mint_run_id("mfk3xa2", 1));
    assert_ne!(mint_run_id("mfk3x9p", 7), mint_run_id("mfk3xa2", 7));
}

/// Within one session the counter still does its job: every id distinct,
/// numbered from one, in the order the runs were taken.
#[test]
fn one_session_mints_distinct_ids_numbered_from_one() {
    let runs = Runs::new();
    let minted: Vec<String> = (0..3).map(|_| runs.mint()).collect();
    let session = runs.session.clone();
    assert_eq!(
        minted,
        vec![
            mint_run_id(&session, 1),
            mint_run_id(&session, 2),
            mint_run_id(&session, 3)
        ],
        "the counter numbers the runs from one, within the session"
    );
}

/// The id is a KEY before it is anything else: it names the one file a
/// fragment `answer` lays down, and `gyld-ui.py` reads a run id back off a
/// log line by whitespace. So it holds no whitespace, it is ONE path
/// component, and the fragment path it names still passes the planner's
/// containment check.
#[test]
fn a_minted_id_is_a_valid_fragment_path_key() {
    let layout = Layout::new(PathBuf::from("/g"), PathBuf::from("/b"));
    let id = Runs::new().mint();
    assert!(
        !id.chars().any(char::is_whitespace),
        "no whitespace in a run id: {id:?}"
    );
    assert_eq!(
        std::path::Path::new(&id).components().count(),
        1,
        "a run id is one path component: {id:?}"
    );

    let fragment = layout.requests().join(format!("{id}.json"));
    assert!(
        bundle::contained(&layout.bundle_root, &fragment),
        "{} leaves the bundle root",
        fragment.display()
    );
    assert_eq!(fragment.parent(), Some(layout.requests().as_path()));
}

/// The FIRST build's id carries the session too. Its output and its terminal
/// record go on the same persistent `gyld.output` log, so a constant id gives
/// a second bootstrap of one node store — a bundle root purged or replaced
/// while the store is kept — the FIRST bootstrap's terminal record.
#[test]
fn two_sessions_mint_different_first_build_ids() {
    assert_ne!(mint_boot_run_id("mfk3x9p"), mint_boot_run_id("mfk3xa2"));
}

/// The first build's id is a key on the same terms as a numbered run's, and
/// the one thing a reader may know about it is its PREFIX: the desk's
/// `bootRunOf` tests that and then displays the whole string.
#[test]
fn a_minted_first_build_id_is_a_prefixed_path_key() {
    let id = Runs::new().boot();
    assert!(
        id.starts_with(FIRST_BUILD_RUN_PREFIX),
        "the desk singles the first build out by this prefix: {id:?}"
    );
    assert!(
        !id.chars().any(char::is_whitespace),
        "no whitespace in a run id: {id:?}"
    );
    assert_eq!(
        std::path::Path::new(&id).components().count(),
        1,
        "a run id is one path component: {id:?}"
    );
}

/// base36, because the session tag has to be SHORT and still sort as text
/// the way it sorts in time: at one width its digits ascend in ASCII too, so
/// a reader that orders ids as strings puts an older session's runs first.
#[test]
fn base36_is_short_and_sorts_as_it_counts() {
    assert_eq!(base36(0), "0");
    assert_eq!(base36(35), "z");
    assert_eq!(base36(36), "10");

    // A millisecond timestamp of today's size is eight characters, and the
    // next millisecond still sorts after it as plain text.
    let now = base36(1_789_247_615_547);
    let later = base36(1_789_247_615_548);
    assert_eq!(now.len(), 8, "{now}");
    assert!(now < later, "{now} < {later}");
}
