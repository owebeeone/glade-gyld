use super::*;

/// Build the exchange handler. It is a synchronous `Fn` (the kit's contract) and
/// never returns `Err`, so the WIRE `ExchangeRes.ok` stays `true` and the PAYLOAD
/// carries success or failure. `token` is the github token the supplier holds
/// for its life.
#[allow(clippy::too_many_arguments)]
pub(super) fn make_handler(
    writer: Writer,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    model: Arc<dyn ModelClient>,
    token: Arc<Token>,
    handle: Handle,
    runs: Arc<Runs>,
    first: Arc<FirstBuild>,
) -> impl Fn(&ExchangeReq) -> Result<Vec<u8>, String> + Send + Sync + 'static {
    // One running total per conversation, for the supplier's lifetime
    // (GyldAskAgent.md section 7).
    let ledger = Arc::new(Ledger::default());
    // One gate, for the supplier's lifetime: writing verbs run one at a time.
    let writes = Arc::new(WriteGate::default());
    move |req: &ExchangeReq| -> Result<Vec<u8>, String> {
        let response = answer(
            &writer,
            &config,
            &runner,
            &model,
            &token,
            &ledger,
            &handle,
            &runs,
            &first,
            &writes,
            &req.payload,
        );
        Ok(response.to_bytes())
    }
}

/// Parse, plan, prepare, then run or accept. Every branch resolves to a
/// [`GyldResponse`].
#[allow(clippy::too_many_arguments)]
fn answer(
    writer: &Writer,
    config: &Arc<GyldConfig>,
    runner: &Arc<dyn Runner>,
    model: &Arc<dyn ModelClient>,
    token: &Arc<Token>,
    ledger: &Arc<Ledger>,
    handle: &Handle,
    runs: &Arc<Runs>,
    first: &Arc<FirstBuild>,
    writes: &Arc<WriteGate>,
    payload: &[u8],
) -> GyldResponse {
    let request = match GyldRequest::parse(payload) {
        Ok(r) => r,
        Err(e) => {
            return GyldResponse::failed(e, config.principal.clone());
        }
    };
    // Attribution: the request's principal, else the supplier's configured one.
    let who = request
        .principal
        .clone()
        .or_else(|| config.principal.clone());

    // A streamed `fork` or `link` may still be running, its host writing its
    // module into the staging tree. While a write is in flight the stage is laid
    // without adopting, so no verb moves that file mid-run (G2); the run's own
    // settle adopts it once the host is done.
    let staged = match writes.in_flight() {
        true => bundle::ensure_stage_without_adopting(&config.layout),
        false => bundle::ensure_stage(&config.layout),
    };
    if let Err(e) = staged {
        return GyldResponse::failed(format!("bundle root unusable: {e}"), who);
    }
    let latest = bundle::latest_build(&config.layout);
    let stamp = bundle::build_stamp();
    let agent = agent_state(config, latest.as_deref());
    // The run id is minted BEFORE the plan, because the plan names a file after it:
    // a fragment `answer` lays its fragment down at `requests/<run-id>.json`, and
    // the planner stays pure by being handed the id rather than inventing one.
    let run_id = runs.mint();
    let plan = match verbs::plan(
        &config.layout,
        &request,
        latest.as_deref(),
        &stamp,
        &run_id,
        &agent,
    ) {
        Ok(p) => p,
        Err(e) => {
            // `fork` and `link` need no bundle and are unaffected; the five that
            // do are told the first build is on its way rather than that there
            // is none.
            return GyldResponse::failed(first.explain(e), who);
        }
    };

    // BEFORE anything is written: what the notebook's name held, so a write Gyld
    // then rejects can be put back exactly. A snapshot that cannot be taken
    // refuses the verb here, because the alternative is a restore that would
    // delete a notebook it could not read.
    let snapshot = match Snapshot::take(&config.layout, &plan) {
        Ok(s) => s,
        Err(e) => {
            return GyldResponse::failed(e, who);
        }
    };
    // One writing verb at a time, from before the write until the run has
    // settled or been put back. Taken only for a verb that leaves a notebook, and
    // a second one is refused as data rather than left to freeze the exchange.
    let gate = match snapshot.is_some() {
        true => match WriteGate::try_hold(writes, &run_id) {
            Ok(held) => Some(held),
            Err(e) => {
                return GyldResponse::failed(e, who);
            }
        },
        false => None,
    };
    // The whole-module path writes its notebook HERE, before the accept, exactly as
    // it always has. The fragment path writes nothing yet: the notebook's text is
    // the merge host's answer and that host runs inside the run ([`fold`]), so both
    // paths reach the rebuild with the same plan and the same file on disk.
    if plan.merge.is_none() {
        if let Err(e) = write_overlay(&config.layout, &plan) {
            return GyldResponse::failed(e, who);
        }
        if let Err(e) = stage_notebook(&config.layout, &plan) {
            return GyldResponse::failed(e, who);
        }
    }

    // `explain` runs no host: it consults. A consult plan carries no argv at
    // all, so nothing about it can reach a runner.
    //
    // It is ALWAYS a streaming run, whatever `stream_output` said: a
    // consultation is model time, and its reply is a stream by nature. The
    // accept answer carries the run id, and the reply lands on the log surface.
    if let Some(consult) = plan.consult.clone() {
        spawn_consult(
            writer.clone(),
            config.clone(),
            model.clone(),
            token.clone(),
            ledger.clone(),
            handle.clone(),
            run_id.clone(),
            consult,
            who.clone(),
        );
        return GyldResponse::accepted(run_id, who);
    }

    // `list` is the one verb with no subprocess: it reads the current bundle's
    // stream listing straight off the app-owned disk.
    if let Some(path) = plan.read.clone() {
        return match read_bounded(&path, config.limits.max_output_bytes) {
            Ok(text) => GyldResponse::ran(run_id, 0, text, String::new(), None, who),
            Err(e) => GyldResponse::failed(e, who),
        };
    }

    if request.stream {
        // Named before the plan is handed over: `answer` and `ask` have already
        // written their notebook, and that is what the accept says.
        //
        // It is not yet a notebook that is SAVED, and a desk must not present it
        // as one. Gyld has not seen the text — the host runs after this answer
        // goes out — so the file may be about to be put back. The run's TERMINAL
        // record is where the outcome lives: it carries `overlay_file` when the
        // write stood and a `refusal` when it did not (README, "A refused
        // write"). The field stays here for the readers that have it.
        let left = overlay_left(&plan);
        spawn_stream(
            writer.clone(),
            config.clone(),
            runner.clone(),
            handle.clone(),
            run_id.clone(),
            plan,
            who.clone(),
            snapshot,
            latest,
            gate,
        );
        return GyldResponse::accepted(run_id, who).leaving(left);
    }

    // The fragment path's merge runs here, inside the run and before the rebuild.
    // A merge Gyld refused wrote nothing, so there is nothing to put back and the
    // answer carries its code and its message like any other refusal.
    let (plan, merged) = match fold(config, runner.as_ref(), &plan) {
        Ok(both) => both,
        Err(refusal) => {
            eprintln!("{}", outcome::said(&plan.verb, &refusal, Put::Nothing));
            return GyldResponse::refused(
                run_id,
                1,
                String::new(),
                String::new(),
                &refusal,
                None,
                who,
            );
        }
    };
    let ran = runner.run(&plan, config.limits, &mut |_, _| {});
    let judged = match ran.as_ref() {
        Ok(out) => land(
            &config.layout,
            &plan,
            Ok(out),
            snapshot.as_ref(),
            latest.as_deref(),
        ),
        Err(e) => land(
            &config.layout,
            &plan,
            Err(e),
            snapshot.as_ref(),
            latest.as_deref(),
        ),
    };
    match (ran, judged) {
        // Refused: failure as data, with the message as `error` and Gyld's own
        // document as `validation` (GyldGrythPlugins.md 4.7). The notebook is
        // back, the build is gone and nothing was published.
        (Ok(out), Some((refusal, document))) => GyldResponse::refused(
            run_id, out.exit, out.stdout, out.stderr, &refusal, document, who,
        ),
        (Ok(out), None) => {
            settle(config, &plan, &out);
            let dir = finish(writer, config, handle, &plan, &out);
            let named = dir.as_ref().map(|d| d.display().to_string());
            // The merge's one summary line stands above the rebuild's own output, so
            // the synchronous answer says what a streamed one appends to the log.
            let stdout = match merged {
                Some(line) => format!("{line}\n{}", out.stdout),
                None => out.stdout,
            };
            GyldResponse::ran(run_id, out.exit, stdout, out.stderr, named, who)
                .leaving(overlay_left(&plan))
        }
        // A run that never landed at all — a spawn failure, a timeout. The
        // refusal says the same thing the plain failure used to, and the notebook
        // is put back before it is said.
        (Err(e), Some((refusal, document))) => {
            GyldResponse::refused(run_id, -1, String::new(), e, &refusal, document, who)
        }
        (Err(e), None) => GyldResponse::failed(e, who),
    }
}

/// Read a bundle document, bounded. A document larger than the budget is a
/// refusal rather than a giant exchange payload.
pub(super) fn read_bounded(path: &std::path::Path, max: usize) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() as usize > max {
        return Err(format!(
            "{} is {} bytes; the exchange budget is {max} (fetch it over the static path)",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}
