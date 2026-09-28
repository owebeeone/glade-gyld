use super::*;

/// The one tool this verb declares, and the form a DRAFT comes back in
/// (GyldAskAgent.md section 8).
pub const DRAFT_TOOL: &str = "propose_draft";

/// The draft tool, declared on every request whatever the question asks.
///
/// **Why a tool and not a fenced JSON block.** The `claude-api` skill offers
/// two structured-output mechanisms: `output_config.format`, which constrains
/// the WHOLE response to one JSON document, and `strict: true` on a tool, which
/// guarantees that `tool_use.input` validates against the schema exactly. This
/// reply is PROSE — streamed to a reader as it arrives — with an offer
/// sometimes beside it, so a whole-response format is the wrong shape: it would
/// cost the reader the answer to get the draft. A tool call arrives as its own
/// content block alongside the text blocks, schema-checked, and never has to be
/// scraped back out of the prose the reader is already reading. A fenced block
/// in the prose would be guaranteed by nothing and rendered twice.
///
/// `tool_choice` stays at its default, `auto`. Forcing the call would have the
/// agent propose on every turn, including the turns that only asked what a
/// record says — and an agent that must always propose is an agent that rules.
///
/// `eager_input_streaming` is deliberately OFF. The skill turns it on so LARGE
/// tool inputs stream as they are generated, at the price of the client owning
/// validation and possibly parsing a truncated input; a draft is a slot, one
/// sentence and a few tags, so the buffered form is both small and the one that
/// arrives whole or not at all.
///
/// It is declared UNCONDITIONALLY — not only when the envelope offers
/// alternatives. Tools render at position 0, ahead of the system block, so a
/// tool set that varied with the question would move the cached prefix on every
/// turn: a conditional tool list is the silent cache invalidator the skill names
/// by name.
pub fn draft_tool() -> serde_json::Value {
    serde_json::json!({
        "name": DRAFT_TOOL,
        "description": "\
    Offer ONE alternative for this record together with the ruling text a reader \
    could take. Call this when the reader asks for a proposal, a recommendation, a \
    lean or draft ruling text — and not otherwise. Say your reasoning in prose \
    first; the call carries the offer, not the argument for it. A draft is an \
    OFFER: a human takes it, edits it or discards it, and calling this tool is not \
    ruling.",
        "input_schema": {
            "type": "object",
            "properties": {
                "alternative": {
                    "type": "string",
                    "description": "The QUALIFIED SLOT of the alternative you \
    propose, exactly as the context lists it under `Alternatives`. Propose only an \
    alternative that list offers.",
                },
                "ruling_text": {
                    "type": "string",
                    "description": "ONE sentence, in the form the overlays use: \
    `YYYY-MM-DD, owner: ...`.",
                },
                "sources": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "The source tags this draft leans on, from \
    the passages supplied. Cite only tags that were supplied.",
                },
            },
            "required": ["alternative", "ruling_text", "sources"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// One turn's request: the configuration, the two-part prompt, and the prior
/// turns of this conversation as its own records tell them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelRequest {
    pub config: ModelConfig,
    pub prompt: Prompt,
    /// The conversation so far, oldest first. Empty on a conversation's first
    /// turn (GyldAskAgent.md section 6).
    pub turns: Vec<Turn>,
    /// Every tool this request DECLARES, in the one order the registry ever
    /// produces (`ToolRegistry::declarations`): sorted by name, with
    /// `propose_draft` among them.
    ///
    /// Empty means the tool set this verb has always had — the draft tool
    /// alone. That is not a default hiding a setting: a supplier with no tools
    /// enabled makes exactly the request it made before section 11 existed.
    pub tools: Vec<serde_json::Value>,
    /// What THIS turn has already exchanged with the model: the assistant turn
    /// that asked for a tool, verbatim, and the user turn that answered it
    /// (11.1). Empty on a turn's first step, and grown by the loop.
    ///
    /// Held apart from `turns` because the two are different things. A prior
    /// TURN is replayed from the log, as the log tells it; these are this
    /// turn's own messages, as the API produced them, and they are replayed
    /// byte for byte because the API rejects a thinking block that was edited
    /// and 400s a `tool_use` nobody answered.
    pub steps: Vec<serde_json::Value>,
}

impl ModelRequest {
    /// The Messages API body.
    ///
    /// **Two cache breakpoints, and both on things that cannot move.**
    ///
    /// 1. The system block is the STABLE prefix — the constant stance, the
    ///    emitted facts of the envelope and every resolved passage. None of it
    ///    changes across the turns of one conversation, so it is byte-identical
    ///    from turn to turn and every follow-up reads it rather than paying for
    ///    the passages again.
    /// 2. The last PRIOR assistant turn, when there is one: settled history,
    ///    already written to the log and unable to change. The question this
    ///    turn asks is deliberately NOT marked — it is the one thing that
    ///    differs every turn, and a breakpoint after it writes an entry whose
    ///    tail is never read back.
    ///
    /// Render order is `tools` → `system` → `messages`, so the marker on the
    /// system block caches the tool declarations with it. Two of the four
    /// breakpoints a request may carry are ever spent.
    ///
    /// `usage.cache_read_input_tokens` staying at zero across a conversation is
    /// the symptom that something in that prefix is moving.
    ///
    /// `thinking` is not sent: on this model family thinking is ON by default
    /// and adaptive, and `display` stays at its default, so no reasoning text
    /// can reach a log record.
    pub fn body(&self, stream: bool) -> serde_json::Value {
        self.body_shaped(stream, self.config.shape)
    }

    /// The same body in an explicitly named shape — what a retry after a
    /// rejection is sent in. The configured shape is only ever the FIRST
    /// attempt's.
    pub fn body_shaped(&self, stream: bool, shape: Shape) -> serde_json::Value {
        let mut body = self.shared_shaped(shape);
        body["max_tokens"] = self.config.max_output_tokens.into();
        body["stream"] = stream.into();
        body
    }

    /// The `count_tokens` body: the same prompt and the same prior turns, with
    /// neither `max_tokens` nor `stream`. Counting the body that is about to be
    /// SENT is the whole point: a count of something else is not a budget, and
    /// a count that forgot the conversation is not this turn's count.
    pub fn count_body(&self) -> serde_json::Value {
        self.shared_shaped(self.config.shape)
    }

    /// The bytes the cache is keyed on, up to and including the system block:
    /// what must be byte-identical from one turn of a conversation to the next.
    /// Asserting on this is how a silent invalidator is caught by a test rather
    /// than by a bill.
    pub fn cached_prefix(&self) -> serde_json::Value {
        let body = self.shared();
        serde_json::json!({
            "model": body["model"],
            "tools": body["tools"],
            "system": body["system"],
        })
    }

    fn shared(&self) -> serde_json::Value {
        self.shared_shaped(self.config.shape)
    }

    fn shared_shaped(&self, shape: Shape) -> serde_json::Value {
        let declared = if self.tools.is_empty() {
            vec![draft_tool()]
        } else {
            self.tools.clone()
        };
        let tools: Vec<serde_json::Value> = declared
            .into_iter()
            .map(|mut tool| {
                if !shape.strict {
                    if let Some(fields) = tool.as_object_mut() {
                        fields.remove("strict");
                    }
                }
                tool
            })
            .collect();
        let mut system = serde_json::json!({
            "type": "text",
            "text": self.prompt.system,
        });
        if shape.cache_control {
            system["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        let mut messages =
            conversation::messages(&self.turns, &self.prompt.user, shape.cache_control);
        messages.extend(self.steps.iter().cloned());
        serde_json::json!({
            "model": self.config.model,
            "tools": tools,
            "system": [system],
            "messages": messages,
        })
    }

    /// The same request, one tool round further on: the assistant turn that
    /// asked, whole and unedited, then the one user turn that answers every
    /// call it made.
    ///
    /// **One user message, however many tools were called.** The skill's
    /// parallel-tool rule is explicit: splitting the results across messages
    /// silently teaches the model to stop calling tools in parallel. And every
    /// `tool_use` of the assistant turn is answered, including one this
    /// supplier refused — an unanswered call is a 400, and a dropped one
    /// teaches nothing.
    pub fn stepped(
        &self,
        asked: &[serde_json::Value],
        answers: Vec<serde_json::Value>,
    ) -> ModelRequest {
        let mut next = self.clone();
        next.steps.push(serde_json::json!({
            "role": "assistant",
            "content": asked,
        }));
        next.steps.push(serde_json::json!({
            "role": "user",
            "content": answers,
        }));
        next
    }
}
