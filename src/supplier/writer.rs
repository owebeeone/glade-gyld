use super::*;

/// Pick one of this session's own chains back up before writing to it.
///
/// The origin is derived from the configured `(share, glade_id)` and nothing
/// else, so every session of a data directory writes under the SAME origin. The
/// chain behind a `(glade_id, key)` therefore already holds whatever earlier
/// sessions published, and a session that appends without having seen it starts
/// at seq 0 on a slot that is already filled — which the node refuses as an
/// equivocation (`node/src/store.rs`, `Verdict::Equivocation`) — and stamps a
/// lamport of 1, which loses the value fold to the very record it meant to
/// replace.
///
/// Subscribing is what makes the history visible. The node's ack names each
/// origin's head in the zone, and the subscribe returns once the replay has
/// reached every one of them (GladeSubstrateV1 §6, R5 and R7): the client's
/// session has folded the chain, and the next `append` continues it properly —
/// seq after the stored head, `prev` linked to it, lamport above every lamport
/// in it. Nothing is waited for past that, so a zone elsewhere in the session
/// that never falls quiet costs a resume nothing. These are value and log
/// surfaces, never the declared exchange one, so a subscribe here streams ops
/// back and never re-attaches a provider.
async fn resume(client: &GladeClient, share: &str, glade_id: &str, key: &[u8]) {
    let chain = chain_name(share, glade_id, key);
    // Said, and the write goes ahead anyway: a refused resume is a stale value,
    // a skipped write is no value at all, and a write the node then refuses is
    // said in its turn ([`Writer::write`]).
    match client.subscribe_outcome(share, glade_id, Some(key)).await {
        Ok(SubscribeOutcome::Accepted { .. }) => {}
        // R6: the reason's code, unless the connection ended before it came.
        Ok(SubscribeOutcome::Refused { code, message }) => {
            let code = code.map_or_else(|| "no code".to_string(), |code| format!("{code:?}"));
            eprintln!(
                "glade-gyld: could not resume {chain}: the node refused the subscribe: {code}, \
                 {message}"
            );
        }
        Err(e) => {
            eprintln!("glade-gyld: could not resume {chain}: {e}");
        }
    }
}

/// The chains this process has already picked up, so it picks each one up ONCE.
///
/// A log surface is written a record at a time — a streaming turn appends
/// hundreds — and only the FIRST of them is the one that has to land on a chain
/// this session has never seen. After that the session holds the chain and
/// `append` continues it on its own, so a resume per record would be a replay
/// per record for nothing. Until the node refuses a write to it: then the chain
/// is forgotten ([`Resumed::forget`]), and the next write picks it up again.
///
/// Keyed by `(glade_id, key)` because that is what a chain is keyed by
/// (`node/src/store.rs`, `ChainId`): a conversation and a run are separate
/// chains on one surface, and picking one up says nothing about the others. The
/// lock is held across the resume so a second writer to the same chain waits for
/// the first one's replay instead of racing past it.
#[derive(Default)]
struct Resumed {
    seen: tokio::sync::Mutex<std::collections::HashSet<(String, Vec<u8>)>>,
}

impl Resumed {
    /// Resume `(glade_id, key)` the first time this process writes to it, and
    /// the first time after it was forgotten.
    async fn once(&self, client: &GladeClient, share: &str, glade_id: &str, key: &[u8]) {
        let mut seen = self.seen.lock().await;
        if !seen.insert((glade_id.to_string(), key.to_vec())) {
            return;
        }
        resume(client, share, glade_id, key).await;
    }

    /// Forget `(glade_id, key)`, whose write the node refused. The client has
    /// dropped the refused op with the rest of its chain, and writes no more
    /// to that chain until a subscribe of its zone resumes it.
    async fn forget(&self, glade_id: &str, key: &[u8]) {
        self.seen
            .lock()
            .await
            .remove(&(glade_id.to_string(), key.to_vec()));
    }
}

/// Every write this session makes goes through one door, [`Writer::write`]: the
/// documents a build publishes, a consultation's records and a run's output. So
/// no op this supplier appends is refused unseen (client-writes plan, Step 4.1).
#[derive(Clone)]
pub(super) struct Writer {
    pub(super) client: GladeClient,
    /// The share every write goes to: the configured one.
    share: String,
    /// The chains this session has picked up, for its lifetime.
    resumed: Arc<Resumed>,
}

impl Writer {
    pub(super) fn new(client: GladeClient, share: String) -> Writer {
        Writer {
            client,
            share,
            resumed: Arc::new(Resumed::default()),
        }
    }

    /// Pick `(glade_id, key)` up, unless this process already has.
    pub(super) async fn resume(&self, glade_id: &str, key: &[u8]) {
        self.resumed
            .once(&self.client, &self.share, glade_id, key)
            .await;
    }

    /// Append one op to a chain of this session's own, and answer what the node
    /// said to it (GladeSubstrateV1 §6, R1):
    ///
    /// 1. the chain is picked up first, once per process;
    /// 2. the op goes out, and the node's answer to it is awaited;
    /// 3. a refusal is said on stderr, with the chain, the seq and the code, and
    ///    the write goes once more, on the chain picked up again;
    /// 4. a second refusal is said too, and the write is dropped.
    ///
    /// Every other answer stands: `Ok` and `Retention` settled the op, one not
    /// placed is the client's to send again (W5), and one whose connection
    /// ended first has no answer to give. `Err` is the client's own: no
    /// connection, or a chain it writes no more until a subscribe resumes it.
    pub(super) async fn write(
        &self,
        glade_id: &str,
        shape: &str,
        payload: Vec<u8>,
        key: &[u8],
    ) -> io::Result<OpOutcome> {
        let chain = chain_name(&self.share, glade_id, key);
        let (seq, outcome) = self.attempt(glade_id, shape, payload.clone(), key).await?;
        let OpOutcome::Refused { code, message } = &outcome else {
            return Ok(outcome);
        };
        eprintln!(
            "glade-gyld: the node refused seq {seq} of {chain}: {code:?}, {message}; picking the \
             chain up again to write it once more"
        );
        let (seq, outcome) = self.attempt(glade_id, shape, payload, key).await?;
        if let OpOutcome::Refused { code, message } = &outcome {
            eprintln!(
                "glade-gyld: the node refused seq {seq} of {chain} again: {code:?}, {message}; \
                 the write is dropped"
            );
        }
        Ok(outcome)
    }

    /// One attempt: the chain picked up unless it is, one append, and the
    /// node's answer, with the op's seq. A refused chain is forgotten, so the
    /// next attempt on it, this write's or a later one's, picks it up again.
    async fn attempt(
        &self,
        glade_id: &str,
        shape: &str,
        payload: Vec<u8>,
        key: &[u8],
    ) -> io::Result<(i64, OpOutcome)> {
        self.resume(glade_id, key).await;
        let (op, outcome) = self
            .client
            .append_outcome(&self.share, glade_id, shape, payload, Some(key))
            .await?;
        if matches!(outcome, OpOutcome::Refused { .. }) {
            self.resumed.forget(glade_id, key).await;
        }
        Ok((op.seq, outcome))
    }
}

/// A chain as a log line names it: `share/glade_id`, and `[key]` when it has
/// one — a stream, a lens, a run or a conversation.
fn chain_name(share: &str, glade_id: &str, key: &[u8]) -> String {
    match key.is_empty() {
        true => format!("{share}/{glade_id}"),
        false => format!("{share}/{glade_id}[{}]", String::from_utf8_lossy(key)),
    }
}
