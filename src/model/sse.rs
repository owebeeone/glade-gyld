use super::*;

/// The in-flight state of one folded stream: the outcome so far, and the
/// content blocks still arriving.
///
/// A content block does NOT arrive whole. It is opened by a
/// `content_block_start` carrying the block's own object, filled by deltas —
/// `text_delta`, `thinking_delta`, `signature_delta`, `input_json_delta` — and
/// closed by a `content_block_stop`, so the fold has to hold the block
/// somewhere until it closes. Here, and not on [`ModelOutcome`]: an outcome is
/// what the turn RESULTED in, and a half-arrived block is not a result.
///
/// **The endpoint's own object is what is kept.** The fold starts from the
/// `content_block` the start event carried and writes the deltas into it,
/// rather than composing a block of its own from the fields it recognises. A
/// block it has never heard of therefore survives whole, and a thinking block
/// keeps the `signature` the API checks — which is what makes
/// [`ModelOutcome::content`] replayable in the next step of a tool loop.
#[derive(Debug, Default)]
pub struct Fold {
    pub outcome: ModelOutcome,
    /// The blocks that have opened and not yet closed.
    open: Vec<Block>,
}

/// One content block, mid-arrival.
#[derive(Debug)]
struct Block {
    index: u64,
    /// The endpoint's own `content_block` object, with every delta so far
    /// written into it.
    value: serde_json::Value,
    /// A `tool_use` input arrives as JSON fragments and is only a value at
    /// `content_block_stop`.
    json: String,
}

impl Block {
    fn kind(&self) -> &str {
        self.value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    }

    fn append(&mut self, field: &str, text: &str) {
        let held = match self.value.get(field).and_then(|v| v.as_str()) {
            Some(held) => format!("{held}{text}"),
            None => text.to_string(),
        };
        self.value[field] = serde_json::Value::String(held);
    }
}

/// Fold an SSE body into events and an outcome. Every line that is not a
/// `data:` payload — the `event:` names, the blank separators, a comment — is
/// skipped: the payload's own `type` is what dispatches.
pub fn fold_stream(
    body: impl BufRead,
    fold: &mut Fold,
    on_event: &mut dyn FnMut(ModelEvent),
) -> Result<(), String> {
    for line in body.lines() {
        let line = line.map_err(|e| format!("the model stream broke: {e}"))?;
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => {
                continue;
            }
        };
        if payload.is_empty() {
            continue;
        }
        fold_event(payload, fold, on_event)?;
    }
    Ok(())
}

/// Fold ONE SSE payload. PURE, so the whole event vocabulary is asserted over a
/// scripted transcript with no network.
pub fn fold_event(
    payload: &str,
    fold: &mut Fold,
    on_event: &mut dyn FnMut(ModelEvent),
) -> Result<(), String> {
    let outcome = &mut fold.outcome;
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => {
            // A payload that is not JSON is not an answer and not a failure
            // either; the stream's own `error` event is how failure arrives.
            return Ok(());
        }
    };
    match value.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "message_start" => {
            let usage = value.pointer("/message/usage");
            outcome.input_tokens = number(usage, "input_tokens");
            outcome.cache_read_input_tokens = number(usage, "cache_read_input_tokens");
            outcome.cache_creation_input_tokens = number(usage, "cache_creation_input_tokens");
        }
        "content_block_start" => {
            // Every block opens here, carrying its own object. A tool block's
            // input is EMPTY at this point: it arrives as fragments and is only
            // whole at `content_block_stop`.
            let held = value
                .get("content_block")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "text", "text": ""}));
            fold.open.push(Block {
                index: index_of(&value),
                value: held,
                json: String::new(),
            });
        }
        "content_block_delta" => {
            let delta = value.get("delta");
            let kind = delta
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let index = index_of(&value);
            let said = |field: &str| -> String {
                delta
                    .and_then(|d| d.get(field))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            // An endpoint that streams a delta for a block it never opened is
            // taken at its word: the block is opened here rather than dropped,
            // so its text still reaches the reader and the replayed turn.
            if !fold.open.iter().any(|block| block.index == index) {
                let opening = match kind {
                    "thinking_delta" | "signature_delta" => {
                        serde_json::json!({"type": "thinking", "thinking": ""})
                    }
                    _ => serde_json::json!({"type": "text", "text": ""}),
                };
                fold.open.push(Block {
                    index,
                    value: opening,
                    json: String::new(),
                });
            }
            let block = match fold.open.iter_mut().find(|block| block.index == index) {
                Some(block) => block,
                None => {
                    return Ok(());
                }
            };
            match kind {
                "text_delta" => {
                    let text = said("text");
                    block.append("text", &text);
                    on_event(ModelEvent::Text(text));
                }
                // Reasoning is KEPT on the block and never emitted: a thinking
                // block has to be replayed to the API unedited, and no
                // reasoning text may reach a log record.
                // Only onto a THINKING block. A reasoning delta aimed at a
                // block of another kind would add a field the API never sent,
                // and the block is replayed verbatim in the next step of a tool
                // loop — so a field invented here is a 400 later.
                "thinking_delta" if block.kind() == "thinking" => {
                    let text = said("thinking");
                    block.append("thinking", &text);
                }
                "signature_delta" if block.kind() == "thinking" => {
                    block.value["signature"] = serde_json::Value::String(said("signature"));
                }
                "input_json_delta" => {
                    block.json.push_str(&said("partial_json"));
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = index_of(&value);
            if let Some(at) = fold.open.iter().position(|block| block.index == index) {
                let mut block = fold.open.remove(at);
                if block.kind() == "tool_use" {
                    // A tool input that is not even JSON travels as the string
                    // it was, so the supplier can SAY what arrived rather than
                    // quietly drop it.
                    let input = match serde_json::from_str::<serde_json::Value>(&block.json) {
                        Ok(value) => value,
                        Err(_) if block.json.trim().is_empty() => serde_json::json!({}),
                        Err(_) => serde_json::Value::String(block.json.clone()),
                    };
                    block.value["input"] = input.clone();
                    if block.value.get("name").and_then(|v| v.as_str()) == Some(DRAFT_TOOL) {
                        on_event(ModelEvent::Draft(input));
                    }
                }
                fold.outcome.content.push(block.value);
            }
        }
        "message_delta" => {
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                outcome.stop_reason = reason.to_string();
            }
            if let Some(details) = value.pointer("/delta/stop_details") {
                outcome.declined = Some(Declined {
                    category: details
                        .get("category")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    explanation: details
                        .get("explanation")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
            let usage = value.get("usage");
            let output = number(usage, "output_tokens");
            if output > 0 {
                outcome.output_tokens = output;
            }
        }
        "error" => {
            let kind = value
                .pointer("/error/type")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            let said = value
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message");
            return Err(format!("the model stream failed ({kind}): {said}"));
        }
        _ => {}
    }
    Ok(())
}

/// A content block's index, which is how a delta finds the block it belongs to.
fn index_of(value: &serde_json::Value) -> u64 {
    value.get("index").and_then(|v| v.as_u64()).unwrap_or(0)
}

fn number(value: Option<&serde_json::Value>, field: &str) -> u64 {
    value
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}
