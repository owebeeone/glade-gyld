use super::*;

/// Ground one consultation (GyldAskAgent.md sections 5 and 7).
///
/// Read the build's own source index, resolve the tags this record and its
/// ruling cite, and compose the prompt. One log line says how it went, with
/// both counts: a citation that resolves to nothing is a thing to be VISIBLE
/// about, in the log as well as in the answer.
///
/// An index that is absent, unreadable or of another format is a refusal as
/// data, exactly like every other failure here.
pub(super) fn ground(consult: &Consultation) -> Result<(Vec<ResolvedSource>, Prompt), String> {
    let index = sources::read(&consult.sources)?;
    let resolved = index.resolve(&consult.context);
    let (found, missing) = sources::counted(&resolved);
    eprintln!(
        "glade-gyld: explain {} on {} ({}): {found} source tag(s) resolved, {missing} unresolved",
        consult.context.record.slot, consult.context.stream, consult.conversation
    );
    let prompt = prompt::compose(&consult.context, &resolved);
    Ok((resolved, prompt))
}

/// Accept a consultation and answer at once: [`consult_run`] on its own task.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_consult(
    writer: Writer,
    config: Arc<GyldConfig>,
    model: Arc<dyn ModelClient>,
    token: Arc<Token>,
    ledger: Arc<Ledger>,
    handle: Handle,
    run_id: String,
    consult: Consultation,
    who: Option<String>,
) {
    handle.spawn(consult_run(
        writer, config, model, token, ledger, run_id, consult, who,
    ));
}

/// Run one consultation and stream its reply.
///
/// The shape is [`stream_run`]'s, and deliberately: ground and call on a
/// BLOCKING task, append each chunk to the log as it arrives, close with a
/// terminal record carrying the exit. A refusal — no index, an over-budget
/// turn, a transport failure — is one line on the log and a non-zero exit,
/// never a hang and never a panic.
///
/// The partial text of a turn the output budget stopped is KEPT and said to be
/// partial: half an answer that says it is half an answer is data; half an
/// answer presented as a whole one is not.
///
/// `token` is the github token the supplier discovered at attach and holds: a
/// question never discovers one.
#[allow(clippy::too_many_arguments)]
async fn consult_run(
    writer: Writer,
    config: Arc<GyldConfig>,
    model: Arc<dyn ModelClient>,
    token: Arc<Token>,
    ledger: Arc<Ledger>,
    run_id: String,
    consult: Consultation,
    who: Option<String>,
) {
    let conversation = consult.conversation.clone();
    let question = consult.context.question.trim().to_string();

    // Pick the conversation's chain up first. A conversation id is the desk's
    // and outlives a supplier restart, so this session may be re-entering a
    // chain an earlier one wrote — and until it has seen it, the fold below
    // reads back nothing and every record this turn appends lands on a taken
    // slot. Once per conversation per process: the turn appends many records
    // and only the first of them meets an unseen chain.
    writer.resume(&config.ask_id, conversation.as_bytes()).await;

    // The transcript IS the log share (section 6): the supplier reads back its
    // own records for this conversation and replays them as prior turns. This
    // happens BEFORE anything of this turn is appended, so what comes back is
    // exactly the turns that came before.
    let prior = conversation::turns(
        &writer
            .client
            .fold_log(&config.share, &config.ask_id, Some(conversation.as_bytes()))
            .await,
    );
    let mut seq: u64 = 1;
    append_ask(
        &writer,
        &config,
        &conversation,
        &GyldAskRecord::question(&run_id, seq, &who, &conversation, question),
    )
    .await;

    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    // Read at every call, not once at attach: the config file is the only
    // channel a running desk has, so a model changed there takes effect on the
    // next question rather than on the next restart.
    let resolved = config.resolve_agent();
    let budget = resolved.config.max_output_tokens;
    // What resolving the configuration had to say — a file that did not decode,
    // a setting nobody has heard of — is the run's business too: a desk served
    // by the wrong model can read why here, next to the answer.
    for note in resolved.notes.iter() {
        seq += 1;
        append_ask(
            &writer,
            &config,
            &conversation,
            &GyldAskRecord::note(&run_id, seq, &who, &conversation, note.clone()),
        )
        .await;
    }
    let model_config = resolved.config;
    let drafted_by = model_config.model.clone();
    // The two roots a source index measures itself against, carried onto the
    // blocking task with everything else the tools need.
    let layout = config.layout.clone();
    let spent = ledger.spent(&conversation);
    let work =
        tokio::task::spawn_blocking(move || -> Result<crate::model::ModelOutcome, String> {
            let (sources, prompt) = ground(&consult)?;
            // The tools this desk allows, over the build this consultation was
            // grounded in — so a tool's answer and a citation can never name
            // two different snapshots (GyldAskAgent.md 11.6).
            let context = tools::ToolContext::beside(&consult.sources, &layout);
            // The allow-list a desk that wrote none means, resolved against
            // what IS configured: the local tools always, and a network tool
            // only where the thing it needs is already there (11.2, 11.7).
            let token: &Token = &token;
            let policy = model_config
                .tools
                .allowing(toolset::on_by_default(&model_config.tools, token));
            let (registry, said) = tools::ToolRegistry::build(
                &policy,
                toolset::offered(&context, &model_config.tools, token),
            );
            for note in said.into_iter() {
                let _ = tx.send(Reply::Note(note));
            }
            // The citations first, so a reader sees what the answer is grounded in
            // before the prose arrives — and sees it even when the call then fails.
            for source in sources.iter() {
                let _ = tx.send(Reply::Citation(
                    serde_json::to_value(source).unwrap_or_default(),
                ));
            }
            let request = ModelRequest {
                config: model_config,
                prompt,
                turns: prior,
                tools: registry.declarations(),
                steps: Vec::new(),
            };
            model::consult(model.as_ref(), &request, spent, &registry, &mut |event| {
                let reply = match event {
                    // A fallback the call had to make. It rides the same
                    // channel as the answer so it lands in the records in the
                    // ORDER it happened — before the prose it weakened.
                    ModelEvent::Note(note) => Reply::Note(note),
                    ModelEvent::Text(chunk) => Reply::Answer(chunk),
                    // The draft is READ here, against the envelope this
                    // consultation resolved, so an alternative the envelope
                    // does not offer is marked unresolved rather than bent onto
                    // one it does.
                    ModelEvent::Draft(input) => {
                        Reply::Draft(AskDraft::parse(&input, &consult.context, &drafted_by))
                    }
                    // A tool the agent reached for, and what it answered. Both
                    // ride the same channel as the prose, so they land in the
                    // records in the ORDER they happened — which is what lets a
                    // follow-up replay the turn as it ran (11.4).
                    ModelEvent::ToolCall(call) => Reply::ToolCall(call),
                    ModelEvent::ToolResult(answered) => Reply::ToolResult(answered),
                };
                let _ = tx.send(reply);
            })
            .map_err(|refusal| refusal.says())
        });

    // A draft that did not decode is not a draft: nothing is offered, and the
    // turn's close is where that is said.
    let mut malformed: Option<String> = None;
    while let Some(reply) = rx.recv().await {
        let record = match reply {
            Reply::Citation(source) => {
                GyldAskRecord::citation(&run_id, seq + 1, &who, &conversation, source)
            }
            Reply::Answer(chunk) => {
                GyldAskRecord::answer(&run_id, seq + 1, &who, &conversation, chunk)
            }
            Reply::Note(note) => GyldAskRecord::note(&run_id, seq + 1, &who, &conversation, note),
            Reply::ToolCall(call) => {
                GyldAskRecord::tool_call(&run_id, seq + 1, &who, &conversation, call)
            }
            Reply::ToolResult(answered) => {
                GyldAskRecord::tool_result(&run_id, seq + 1, &who, &conversation, answered)
            }
            Reply::Draft(Ok(draft)) => GyldAskRecord::draft(
                &run_id,
                seq + 1,
                &who,
                &conversation,
                serde_json::to_value(&draft).unwrap_or_default(),
            ),
            Reply::Draft(Err(reason)) => {
                malformed = Some(reason);
                continue;
            }
        };
        seq += 1;
        append_ask(&writer, &config, &conversation, &record).await;
    }

    let (exit, said) = match work.await {
        Ok(Ok(outcome)) => {
            // What the turn cost joins the conversation's running total, so the
            // NEXT turn is measured against what this one actually spent.
            ledger.spend(&conversation, outcome.tokens());
            (outcome.exit(), outcome.says(budget))
        }
        Ok(Err(refused)) => (1, Some(refused)),
        Err(e) => (-1, Some(format!("the consultation task failed: {e}"))),
    };
    // The prose still stands; the offer the reader asked for did not arrive, so
    // the turn did not end clean.
    let (exit, said) = match malformed {
        Some(reason) => {
            let exit = if exit == 0 { 1 } else { exit };
            match said {
                Some(already) => (exit, Some(format!("{already}; {reason}"))),
                None => (exit, Some(reason)),
            }
        }
        None => (exit, said),
    };
    seq += 1;
    let end = GyldAskRecord::end(&run_id, seq, &who, &conversation, exit, said);
    append_ask(&writer, &config, &conversation, &end).await;
}

/// One thing a consultation produces, on its way to a record.
enum Reply {
    Citation(serde_json::Value),
    Answer(String),
    /// Something the call had to do differently, on its way to a `note` record.
    Note(String),
    /// A tool the agent reached for, on its way to a `tool_call` record.
    ToolCall(serde_json::Value),
    /// What that call answered, on its way to a `tool_result` record.
    ToolResult(serde_json::Value),
    /// A draft, read against the envelope — or the reason it was not a draft.
    Draft(Result<AskDraft, String>),
}

/// Append one reply record to the ask surface, keyed by CONVERSATION.
async fn append_ask(
    writer: &Writer,
    config: &GyldConfig,
    conversation: &str,
    record: &GyldAskRecord,
) {
    let _ = writer
        .write(
            &config.ask_id,
            "log",
            record.to_bytes(),
            conversation.as_bytes(),
        )
        .await;
}
