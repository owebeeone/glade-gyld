use super::*;

/// Calls a model. Synchronous, like [`crate::exec::Runner`], and driven from a
/// blocking task; the tests drive a scripted double instead.
pub trait ModelClient: Send + Sync + 'static {
    /// Count the input tokens of `request` before it is sent — or estimate
    /// them, saying so through `on_event`, where the endpoint cannot count.
    fn count_tokens(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<u64, String>;

    /// Stream the reply, calling `on_event` for each event as it arrives.
    /// An `Err` is a transport failure, which the caller turns into data.
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String>;
}

/// How many characters of request body one token is taken to be, when the
/// endpoint cannot be asked.
///
/// Deliberately pessimistic. The usual rule of thumb is four characters a
/// token; three OVER-counts by about a third, and an over-count refuses a turn
/// that would have fitted while an under-count sends one that does not. A
/// budget that can be crossed silently is not a budget, and the refusal names
/// both numbers either way.
pub const ESTIMATED_CHARS_PER_TOKEN: u64 = 3;

/// The input size of a request, without asking anybody.
///
/// It is measured on the body that is about to be SENT — the same document
/// `count_tokens` would have been given — so it moves with the prompt, the
/// tool declaration and every prior turn, exactly as the real count does.
pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    let body = serde_json::to_string(&request.count_body()).unwrap_or_default();
    (body.chars().count() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN)
}

/// What one rejection of a request FEATURE costs, and what is left to try.
///
/// The ladder is `strict`, then `cache_control`, then nothing: two rungs, so a
/// call makes at most three attempts and an endpoint that simply dislikes the
/// request cannot be retried at forever. An error naming the field goes
/// straight to that rung; one that names neither takes them in order, because
/// a local endpoint's 400 often says only that a field is unknown.
///
/// PURE, so the whole ladder is asserted with no endpoint in sight.
pub fn degrade(shape: Shape, said: &str) -> Option<(Shape, String)> {
    let names = |field: &str| said.contains(field);
    let blamed = |field: &str, shape: Shape| -> Option<(Shape, String)> {
        Some((
            shape,
            format!(
                "the endpoint answered 400; retrying without `{field}` ({}){}",
                one_line(said),
                consequence(field)
            ),
        ))
    };
    if shape.strict && (names("strict") || !names("cache_control")) {
        return blamed("strict", shape.without_strict());
    }
    if shape.cache_control {
        return blamed("cache_control", shape.without_cache_control());
    }
    if shape.strict {
        return blamed("strict", shape.without_strict());
    }
    None
}

/// What a reader loses by the drop, said once and in the note.
fn consequence(field: &str) -> &'static str {
    match field {
        "strict" => ", so a draft's input is checked by this supplier rather than guaranteed",
        _ => ", so this conversation pays for its passages on every turn",
    }
}

/// An endpoint's own message, trimmed to one bounded line — it lands in a
/// record a page draws.
fn one_line(said: &str) -> String {
    said.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

/// One consultation, which is a LOOP: count, check both input budgets, stream,
/// and — while the model asks for a tool it may have — run it, answer it and
/// call again (GyldAskAgent.md section 11.1).
///
/// The count happens BEFORE every call, so an over-budget step is refused with
/// its numbers and costs nothing. It is before EVERY call and not only the
/// first because the body grows: each round appends an assistant turn and its
/// results, and a budget checked once against the smallest body it will ever
/// have is not a budget. `spent` is what this CONVERSATION has already spent
/// across its earlier turns — the running total of section 7 — and this turn's
/// own steps are added to it as they are paid for.
///
/// The per-run budget is checked first: it is about this question, and a
/// question too big to ask is too big whatever the conversation has spent.
///
/// Four ways out, and each is a fact rather than an accident:
///
/// * `end_turn` — the answer ended.
/// * a turn whose only tool call is `propose_draft` — the offer is the ending
///   (section 8), and this supplier never answers that call.
/// * the step budget — the loop stops, a `note` says so, and the turn still
///   ends cleanly with the prose it had (11.3).
/// * a transport failure, or a budget crossed — a refusal as data, as before.
///
/// Anything else the endpoint says — `max_tokens`, `refusal`, `pause_turn`, a
/// stop reason nobody has heard of — ends the loop too and is reported as the
/// turn's own ending, exactly as a single-call turn already reports it.
pub fn consult(
    client: &dyn ModelClient,
    request: &ModelRequest,
    spent: u64,
    tools: &ToolRegistry,
    on_event: &mut dyn FnMut(ModelEvent),
) -> Result<ModelOutcome, AskRefusal> {
    let budgets = tools.budgets();
    let mut turn = request.clone();
    let mut total = ModelOutcome::default();
    for step in 0..=budgets.steps {
        let counted = client
            .count_tokens(&turn, on_event)
            .map_err(|reason| AskRefusal::Transport { reason })?;
        if counted > turn.config.max_input_tokens {
            return Err(AskRefusal::OverInputBudget {
                counted,
                budget: turn.config.max_input_tokens,
            });
        }
        let budget = turn.config.max_conversation_tokens;
        let so_far = spent.saturating_add(total.tokens());
        if budget > 0 && so_far.saturating_add(counted) > budget {
            return Err(AskRefusal::OverConversationBudget {
                spent: so_far,
                counted,
                budget,
            });
        }
        let outcome = client
            .stream(&turn, on_event)
            .map_err(|reason| AskRefusal::Transport { reason })?;
        total.add(&outcome);
        let calls = outcome.calls();
        if outcome.stop_reason != TOOL_USE || calls.is_empty() {
            return Ok(total);
        }
        if step == budgets.steps {
            // The budget is the gentle boundary: the model would have gone on,
            // the supplier chose not to, and the reader is told which — with
            // whatever prose the turn had already written kept.
            on_event(ModelEvent::Note(format!(
                "this turn asked for a tool again after {} tool step(s), which is this \
                 supplier's per-turn budget, so the loop stopped here and the answer may be \
                 incomplete",
                budgets.steps
            )));
            return Ok(total);
        }
        let mut answers: Vec<serde_json::Value> = Vec::new();
        for (id, name, input) in calls.iter() {
            on_event(ModelEvent::ToolCall(crate::tools::call_record(
                id, name, input,
            )));
            let answered = tools.call(id, name, input);
            on_event(ModelEvent::ToolResult(answered.record()));
            answers.push(answered.block());
        }
        // A draft made on the SAME turn as a read is still a tool call the API
        // expects an answer to, and the honest answer is what happened to it:
        // it was recorded and shown, and it is not a ruling.
        for block in outcome.content.iter() {
            if block.get("name").and_then(|v| v.as_str()) != Some(DRAFT_TOOL) {
                continue;
            }
            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            answers.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": "The draft was recorded on this run and offered to the reader, who \
                            takes it, edits it or discards it. It is not a ruling and no \
                            decision has been taken.",
            }));
        }
        turn = turn.stepped(&outcome.content, answers);
    }
    Ok(total)
}
