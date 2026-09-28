use super::*;

/// Accept a streaming run and answer at once: [`stream_run`] on its own task.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_stream(
    writer: Writer,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
    snapshot: Option<Snapshot>,
    previous: Option<PathBuf>,
    gate: Option<Writing>,
) {
    let inner = handle.clone();
    handle.spawn(stream_run(
        writer, config, runner, inner, run_id, plan, who, snapshot, previous, gate,
    ));
}

/// Run the plan on a blocking task, appending every output line to the log
/// surface keyed by `run_id`, then a terminal `{done:true, exit}` record.
/// Best effort: an append failure (a link drop mid-run) is dropped, because the
/// exchange answer already carried the run id.
///
/// Resolves when the run has landed and its terminal record is on the log, so a
/// caller that must know when a build finished — the first build — can await it.
///
/// The TERMINAL record carries the outcome of a writing verb, because the accept
/// answer could not: it went out before the host ran. A refused write appends the
/// `refusal` there and an accepted one the notebook it really left, so a desk says
/// "saved" once the run says so and not before.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_run(
    writer: Writer,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
    snapshot: Option<Snapshot>,
    previous: Option<PathBuf>,
    gate: Option<Writing>,
) {
    let inner = handle;
    let mut seq: u64 = 0;
    // The fragment path's merge, on a blocking task of its own and BEFORE the
    // rebuild: it writes the notebook this run is about to build, so nothing after
    // it can tell a fragment `answer` from a whole-module one.
    let (plan, merged) = match folded(&config, &runner, plan).await {
        Ok(both) => both,
        Err((plan, refusal)) => {
            eprintln!("{}", outcome::said(&plan.verb, &refusal, Put::Nothing));
            drop(gate);
            seq += 1;
            append(
                &writer,
                &config,
                &run_id,
                &GyldOutputRecord::end(&run_id, seq, &who, 1).refusing(Some(refusal)),
            )
            .await;
            return;
        }
    };
    if let Some(line) = merged {
        seq += 1;
        let record = GyldOutputRecord::line(&run_id, seq, &who, "stdout", line);
        append(&writer, &config, &run_id, &record).await;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
    let limits = config.limits;
    // The plan is judged HERE and run over there: a copy stays behind so a run
    // whose task died outright is still put back, which a plan handed to the task
    // and lost with it could not be.
    let judged = plan.clone();
    let work = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || {
            runner.run(&plan, limits, &mut |stream, line| {
                let _ = tx.send((stream.to_string(), line.to_string()));
            })
        })
    };

    while let Some((stream, line)) = rx.recv().await {
        seq += 1;
        let record = GyldOutputRecord::line(&run_id, seq, &who, &stream, line);
        append(&writer, &config, &run_id, &record).await;
    }

    let (exit, refused) = match work.await {
        Ok(Ok(out)) => {
            let refused = land(
                &config.layout,
                &judged,
                Ok(&out),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            if refused.is_none() {
                settle(&config, &judged, &out);
                finish(&writer, &config, &inner, &judged, &out);
            }
            (out.exit, refused)
        }
        Ok(Err(e)) => {
            seq += 1;
            let record = GyldOutputRecord::line(&run_id, seq, &who, "stderr", e.clone());
            append(&writer, &config, &run_id, &record).await;
            let refused = land(
                &config.layout,
                &judged,
                Err(&e),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            (-1, refused)
        }
        Err(e) => {
            let said = format!("run task failed: {e}");
            seq += 1;
            let record = GyldOutputRecord::line(&run_id, seq, &who, "stderr", said.clone());
            append(&writer, &config, &run_id, &record).await;
            let refused = land(
                &config.layout,
                &judged,
                Err(&said),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            (-1, refused)
        }
    };
    // The write has settled or been put back: the next writing verb may go.
    drop(gate);

    // The notebook this run really left, and only on a run that was not refused.
    let saved = match refused.is_some() {
        true => None,
        false => overlay_left(&judged),
    };
    seq += 1;
    append(
        &writer,
        &config,
        &run_id,
        &GyldOutputRecord::end(&run_id, seq, &who, exit)
            .refusing(refused.map(|(refusal, _)| refusal))
            .leaving(saved),
    )
    .await;
}

/// Append one output record to the log surface, keyed by run id.
pub(super) async fn append(
    writer: &Writer,
    config: &GyldConfig,
    run_id: &str,
    record: &GyldOutputRecord,
) {
    let _ = writer
        .write(
            &config.output_id,
            "log",
            record.to_bytes(),
            run_id.as_bytes(),
        )
        .await;
}
