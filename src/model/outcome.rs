use super::*;

/// One thing the call produced.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    /// Something the CALL had to do differently, as data: an input budget that
    /// is an estimate because the endpoint has no `count_tokens`, a `strict`
    /// the endpoint rejected, a cache breakpoint it would not take.
    ///
    /// **Never silent.** Each of these makes the turn weaker in a way a reader
    /// can act on — an unchecked draft input, a prompt paid for in full, a
    /// budget that is a guess — so each one is a record on the run beside the
    /// answer, not a log line in a terminal nobody is reading.
    Note(String),
    /// A chunk of the answer, as it arrived.
    Text(String),
    /// A draft, as the tool call carried it — the raw input, read by nothing
    /// here. What a draft MEANS is the envelope's business
    /// ([`crate::ask::AskDraft::parse`]); a tool input that was not even JSON
    /// arrives as a string, so the supplier can say what it was rather than
    /// swallow it.
    Draft(serde_json::Value),
    /// A tool the model asked for, as the loop is about to run it
    /// (`{id, name, input}`, GyldAskAgent.md 11.4). Emitted BEFORE the call, so
    /// a reader watching a turn sees what it is waiting on.
    ToolCall(serde_json::Value),
    /// What that call answered with (`{id, name, ok, summary, bytes,
    /// truncated}`). A refusal is `ok: false` and is a record like any other.
    ToolResult(serde_json::Value),
}

/// Why the model declined, as data (`stop_reason: "refusal"`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Declined {
    pub category: String,
    pub explanation: String,
}

/// How one turn ended, and what it cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelOutcome {
    /// `end_turn`, `max_tokens`, `refusal`, `stop_sequence`, `tool_use`,
    /// `pause_turn` — or empty, when the stream ended without saying.
    pub stop_reason: String,
    pub declined: Option<Declined>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// The assistant turn's content blocks, in the order they arrived and as
    /// they arrived: text, thinking with its signature, `tool_use` with its id
    /// and input, and anything else this endpoint sent.
    ///
    /// **Verbatim, because replay demands it.** A turn that asked for a tool is
    /// appended to `messages` whole before its results are; the API rejects a
    /// thinking block whose content was modified and 400s a `tool_use` that was
    /// dropped, so the fold keeps the endpoint's own object and only fills in
    /// the deltas that belong to it.
    pub content: Vec<serde_json::Value>,
}

/// The one clean ending.
pub const END_TURN: &str = "end_turn";
/// The ending that means the output budget stopped it.
pub const MAX_TOKENS: &str = "max_tokens";
/// The ending that means the model declined.
pub const REFUSAL: &str = "refusal";
/// The ending that means the turn closed on a tool call.
///
/// A CLEAN ending, still — but for two reasons now rather than one. It is the
/// ending of a turn that made the OFFER the reader asked for, which this
/// supplier never answers: `propose_draft` carries a draft back and there is
/// nothing more the model would say. And it is the ending of a turn the STEP
/// BUDGET stopped, where the model would have gone on and the supplier chose
/// not to (11.3) — said in a `note` record, with the prose it had written kept.
///
/// Every other `tool_use` is answered and the loop continues, so it is never
/// what a turn ENDS on.
pub const TOOL_USE: &str = "tool_use";

impl ModelOutcome {
    /// The turn ended because the answer ended — or because it ended in a tool
    /// call this supplier does not answer (see [`TOOL_USE`]).
    pub fn complete(&self) -> bool {
        self.stop_reason == END_TURN || self.stop_reason == TOOL_USE
    }

    /// The `tool_use` blocks of this turn that the LOOP must answer: every one
    /// but the draft tool, which is an offer and not a question.
    pub fn calls(&self) -> Vec<(String, String, serde_json::Value)> {
        self.content
            .iter()
            .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .map(|block| {
                (
                    block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                )
            })
            .filter(|(_, name, _)| name != DRAFT_TOOL)
            .collect()
    }

    /// Add one step's usage to a turn's running total.
    ///
    /// A multi-step turn is several calls and ONE turn: what it cost the
    /// conversation is the sum, and what it ended as is the last step's ending.
    pub(super) fn add(&mut self, step: &ModelOutcome) {
        self.input_tokens = self.input_tokens.saturating_add(step.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(step.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(step.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(step.cache_creation_input_tokens);
        self.stop_reason = step.stop_reason.clone();
        self.declined = step.declined.clone();
        self.content = step.content.clone();
    }

    /// What this turn cost, whole: the prompt however it was served — uncached,
    /// written to the cache, or read back from it — plus what came out.
    ///
    /// `input_tokens` is the uncached REMAINDER only, so summing the three is
    /// the only honest reading of a turn's size. A conversation's running total
    /// is these, added up.
    pub fn tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.output_tokens)
    }

    /// What to say about how it ended, when that is not simply "it ended".
    /// This is the line the run's terminal record carries.
    pub fn says(&self, max_output_tokens: u64) -> Option<String> {
        match self.stop_reason.as_str() {
            END_TURN | TOOL_USE => None,
            MAX_TOKENS => Some(format!(
                "the answer stopped at the output budget of {max_output_tokens} tokens and is \
                 partial",
            )),
            REFUSAL => {
                let declined = self.declined.clone().unwrap_or_default();
                Some(format!(
                    "the model declined ({}): {}",
                    empty_is(&declined.category, "no category"),
                    empty_is(&declined.explanation, "no explanation given"),
                ))
            }
            "" => Some("the stream ended without a stop reason".into()),
            other => Some(format!("the turn stopped with stop_reason {other:?}")),
        }
    }

    /// The exit code the terminal record carries: zero only for a clean turn.
    pub fn exit(&self) -> i32 {
        if self.complete() {
            0
        } else {
            1
        }
    }
}

fn empty_is<'a>(value: &'a str, absent: &'a str) -> &'a str {
    if value.trim().is_empty() {
        absent
    } else {
        value
    }
}
