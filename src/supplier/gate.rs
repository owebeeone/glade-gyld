use super::*;

/// Writing verbs run ONE AT A TIME: the gate is taken before the write and freed
/// when the run has settled or been put back.
///
/// The kit already serialises the exchange handler — one `ExchangeReq` at a time
/// — so two writes can never interleave. What it does not serialise is a STREAMED
/// run, which is accepted at once and settles later on its own task. Without this
/// gate a second `answer` arriving mid-run would write its notebook, and the first
/// run's refusal would then put the FIRST notebook back over it: the second write
/// lost, and the bundle agreeing with neither.
///
/// A second writing verb is REFUSED as data rather than made to wait. Waiting
/// would have to happen in the kit's synchronous handler, which is the ONE loop
/// every request passes through, so a blocked write would freeze `list` and
/// `explain` and `diff` behind it for as long as a build takes — and `list` is
/// what a UI polls while it waits. A refusal that names the run in flight is
/// something a desk can say and a reader can act on; a desk that stops answering
/// is not. The owner's draft is his, so submitting again costs him nothing.
///
/// One gate and nothing else: no queue, no lock manager, nothing to configure. A
/// `std::sync::Mutex` rather than a `tokio::sync::Mutex` because the hold begins
/// in that synchronous handler and ends on a spawned task, and a guard that
/// crosses that seam must be `Send` and must not need a runtime flavour.
#[derive(Debug, Default)]
pub struct WriteGate {
    /// The run id of the write in flight, when one is.
    holder: std::sync::Mutex<Option<String>>,
}

/// The gate, HELD. Dropping it frees the gate, so a run that panics cannot wedge
/// the desk shut against every write after it.
#[derive(Debug)]
pub struct Writing(Arc<WriteGate>);

impl WriteGate {
    /// Take the gate for `run_id`, or say which run already has it.
    ///
    /// A poisoned lock is taken anyway: the state behind it is one `Option`, a
    /// panic cannot have left it torn, and refusing every write for the rest of
    /// the process is a worse answer than carrying on.
    pub fn try_hold(gate: &Arc<WriteGate>, run_id: &str) -> Result<Writing, String> {
        let mut holder = gate.holder.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = holder.as_deref() {
            return Err(format!(
                "a write is already in flight (run {held}); wait for it to finish and submit again"
            ));
        }
        *holder = Some(run_id.to_string());
        Ok(Writing(gate.clone()))
    }

    /// Is a write in flight: the gate taken and not yet freed?
    ///
    /// The exchange path asks before it lays the stage (G2). Every `fork` and
    /// `link` holds the gate from before its host starts until its run has
    /// settled, and only the handler, which the kit serialises, starts one: so
    /// while this says no, no such host is writing into the staging tree.
    pub(super) fn in_flight(&self) -> bool {
        let holder = self.holder.lock().unwrap_or_else(|e| e.into_inner());
        holder.is_some()
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut holder = self.0.holder.lock().unwrap_or_else(|e| e.into_inner());
        *holder = None;
    }
}
