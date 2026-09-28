use super::*;

/// The run ids one supplier process mints, and the counter numbering them.
///
/// The counter alone is not enough. A run's output and its terminal record are
/// appended to the `gyld.output` log keyed by run id, and that log lives in the
/// node's PERSISTENT store — so a counter that restarts with the process gives a
/// fresh run an id the previous session already spent, and a reader that looks up
/// this run's outcome finds the OLD run's terminal record instead. The session
/// tag is what makes the id unique across restarts; the counter still orders the
/// runs within one.
pub(super) struct Runs {
    /// Fixed for the life of this process, and different in the next one.
    pub(super) session: String,
    next: AtomicU64,
}

impl Runs {
    /// Take the session tag ONCE, here: every id this process mints shares it.
    pub(super) fn new() -> Runs {
        Runs {
            session: session_tag(),
            next: AtomicU64::new(0),
        }
    }

    /// The next run's id — distinct from every other id this process mints, and
    /// from the ids of every other process.
    pub(super) fn mint(&self) -> String {
        mint_run_id(&self.session, self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// The id of the first build this process may make for itself: the same
    /// session tag its numbered runs carry, and no number of its own, because
    /// nobody asked for that run.
    ///
    /// It needs the session for the reason every other id does. Its output and
    /// its terminal record go on the persistent `gyld.output` log too, so a
    /// constant id makes a second bootstrap of one node store — a bundle root
    /// purged or replaced while the store is kept — append a second first build
    /// under the first one's key, and a reader looking up this bootstrap's
    /// outcome finds the earlier one's terminal record.
    pub(super) fn boot(&self) -> String {
        mint_boot_run_id(&self.session)
    }
}

/// One run id, from the session it was minted in and the number it took.
///
/// OPAQUE to every reader: the log is keyed by the whole string and
/// `requests/<run-id>.json` uses it as a path component, so the only properties
/// that matter are that it holds no whitespace, stays one path component, and
/// that no two runs anywhere ever share one.
pub(super) fn mint_run_id(session: &str, n: u64) -> String {
    format!("run-{session}-{n}")
}

/// The first build's run id, from the session it was minted in.
///
/// Opaque on the same terms as [`mint_run_id`]'s: the one thing a reader may know
/// about it is [`FIRST_BUILD_RUN_PREFIX`], which the desk's `bootRunOf` tests
/// before displaying the whole string.
pub(super) fn mint_boot_run_id(session: &str) -> String {
    format!("{FIRST_BUILD_RUN_PREFIX}{session}")
}

/// A tag for this process, read off the clock when the counter is made.
///
/// Milliseconds since the epoch in base36: short (eight characters until 2059)
/// and, at one width, ordered as text the way it is ordered in time, so a reader
/// that sorts ids as strings still puts an older session's runs first. A clock
/// before the epoch degrades to `0` rather than panicking, as
/// [`bundle::build_stamp`] does with the same reading.
fn session_tag() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    base36(millis)
}

/// `n` in lowercase base36, most significant digit first.
pub(super) fn base36(mut n: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    out.reverse();
    out.into_iter().collect()
}

/// The prefix on the run id of the supplier's own first build — `boot-<session>`.
/// Deliberately not `run-`: nobody asked for this run, so it is not numbered
/// among the ones that were. The session tag behind it is what a reader must NOT
/// assume, so this is all that is public: the id itself is minted per process,
/// and singling the run out means testing this prefix.
pub const FIRST_BUILD_RUN_PREFIX: &str = "boot-";
