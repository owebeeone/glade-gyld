//! The model client (GyldAskAgent.md section 7).
//!
//! **In the supplier process, never in the page.** There is no official
//! Anthropic SDK for Rust, so the call is raw HTTPS: `POST /v1/messages` with
//! `x-api-key` and `anthropic-version`, `"stream": true`, and the SSE events
//! folded into text chunks as they arrive.
//!
//! [`ModelClient`] is a trait for the same reason [`crate::exec::Runner`] is:
//! the whole verb path — envelope, resolution, prompt, records, refusals — is
//! driven in tests by a scripted double with no network anywhere near it. It is
//! SYNCHRONOUS, like `Runner`, and the supplier calls it from a blocking task.
//!
//! **Bounds are refusal boundaries, not hopes.** The input is counted with
//! `POST /v1/messages/count_tokens` BEFORE the call, so an over-budget turn is
//! refused with both numbers and costs nothing; the output is bounded by
//! `max_tokens`, and a turn that stops there is SAID to be partial rather than
//! passed off as an answer.
//!
//! **The key never travels.** It is read from the environment or from an
//! app-owned file at the moment of the call, put in one header, and dropped. It
//! is in no plan, no prompt, no record, no log line and no [`Debug`] output:
//! [`ModelConfig`] carries the key FILE's path and never a key.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::agent::Compat;
use crate::ask::{AskRefusal, KEY_ENV};
use crate::conversation::{self, Turn};
use crate::prompt::Prompt;
use crate::tools::{ToolPolicy, ToolRegistry};

/// The model `--agent-model` defaults to. Taken from the `claude-api` skill's
/// model table rather than from memory: `claude-opus-5`, 1M context, $5.00 per
/// 1M input tokens and $25.00 per 1M output tokens at first-party rates.
pub const DEFAULT_AGENT_MODEL: &str = "claude-opus-5";

/// The Messages API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The API version header every request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The per-run input budget. Generous next to one grounded envelope, and still
/// a bound: it is what stops a pathological context costing real money.
pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 200_000;

/// The per-run output budget, and the request's `max_tokens`. The streaming
/// default: a grounded answer is long output.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// The wall clock one consultation gets.
pub const DEFAULT_MODEL_TIMEOUT_SECS: u64 = 300;

/// The per-CONVERSATION ceiling: every token every turn of one conversation
/// spent, summed. A turn that would cross it is refused before it is sent.
///
/// The per-run budgets bound one question. This one bounds the thread: prior
/// turns are replayed into every follow-up, so a conversation left running is
/// the one thing here that grows on its own. The default is this model's own
/// context window, which is the largest a single turn could ever be — several
/// turns of it, not one. `0` lifts the ceiling.
pub const DEFAULT_MAX_CONVERSATION_TOKENS: u64 = 1_000_000;

/// What a request may carry beyond the bare Messages API — and therefore what
/// is DROPPED when an endpoint will not take it.
///
/// Both are optimisations, not meaning: `strict` guarantees the draft tool's
/// input validates, `cache_control` makes a follow-up read the passages from
/// the cache instead of paying for them again. A turn sent without either is
/// the same turn, more expensive and less checked. That is why an endpoint
/// that rejects one is answered by dropping it and saying so, rather than by
/// failing the reader's question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// `strict: true` on the draft tool.
    pub strict: bool,
    /// The two `cache_control` breakpoints.
    pub cache_control: bool,
}

impl Shape {
    /// Everything on: what the Anthropic path has always sent, and what every
    /// first attempt sends.
    pub fn full() -> Shape {
        Shape {
            strict: true,
            cache_control: true,
        }
    }

    /// The same request with `strict` dropped from the tool.
    pub fn without_strict(self) -> Shape {
        Shape {
            strict: false,
            ..self
        }
    }

    /// The same request with no cache breakpoints.
    pub fn without_cache_control(self) -> Shape {
        Shape {
            cache_control: false,
            ..self
        }
    }
}

impl Default for Shape {
    fn default() -> Shape {
        Shape::full()
    }
}

/// Everything the model call is configured with. No key: see the module note.
///
/// It is resolved per call from [`crate::agent::resolve`] — a config file, the
/// environment and the flags — so the endpoint and the model can change under a
/// running supplier.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelConfig {
    pub model: String,
    pub base_url: String,
    /// Which dialect of the Messages API `base_url` speaks.
    pub compat: Compat,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    /// The running total one conversation may spend across its turns; `0` is
    /// no ceiling.
    pub max_conversation_tokens: u64,
    pub timeout: Duration,
    /// Where a key is read from when the environment carries none.
    pub key_file: PathBuf,
    /// Ask the endpoint to count the input before the call. False where the
    /// endpoint has no `count_tokens`, and the budget is estimated instead.
    pub count_tokens: bool,
    /// What the FIRST request of a call carries. The client degrades from here
    /// on what the endpoint actually rejects.
    pub shape: Shape,
    /// Which tools this desk lets the agent reach for, and what one turn's tool
    /// use may cost (GyldAskAgent.md section 11.2, 11.3).
    pub tools: ToolPolicy,
}

impl Default for ModelConfig {
    fn default() -> ModelConfig {
        ModelConfig {
            model: DEFAULT_AGENT_MODEL.into(),
            base_url: DEFAULT_BASE_URL.into(),
            compat: Compat::Anthropic,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            max_conversation_tokens: DEFAULT_MAX_CONVERSATION_TOKENS,
            timeout: Duration::from_secs(DEFAULT_MODEL_TIMEOUT_SECS),
            key_file: PathBuf::new(),
            count_tokens: true,
            shape: Shape::full(),
            tools: ToolPolicy::default(),
        }
    }
}

impl ModelConfig {
    /// The default with the bundle root's own key file in it: what an
    /// unconfigured supplier resolves to.
    pub fn default_at(bundle_root: &Path) -> ModelConfig {
        ModelConfig {
            key_file: bundle_root.join(crate::ask::DEFAULT_KEY_FILE),
            ..ModelConfig::default()
        }
    }
}

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
    fn add(&mut self, step: &ModelOutcome) {
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

/// The model key, at the moment of the call and nowhere else.
///
/// `ANTHROPIC_API_KEY` in the supplier's environment first, then
/// [`crate::agent::AUTH_TOKEN_ENV`], then the key file. The second name is
/// there because it is the one Claude-shaped clients and the dabeest launchers
/// already export, and a local endpoint's token is a dummy value that must
/// nonetheless be present. The file is MODE CHECKED: a credential any other
/// account on the machine can read is refused rather than used, because a
/// supplier that quietly accepts one teaches everybody that it is fine.
pub fn discover_key(key_file: &Path) -> Result<String, AskRefusal> {
    for name in [KEY_ENV, crate::agent::AUTH_TOKEN_ENV] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    let read = std::fs::read_to_string(key_file);
    let text = match read {
        Ok(text) => text,
        Err(_) => {
            return Err(AskRefusal::NoModelKey {
                key_file: key_file.to_path_buf(),
            });
        }
    };
    platform::check_mode(key_file)?;
    let key = text.lines().next().unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Err(AskRefusal::NoModelKey {
            key_file: key_file.to_path_buf(),
        });
    }
    Ok(key)
}

/// The key file's mode check, behind an explicit platform boundary (the
/// workzone's conditional-compilation rule: no bare `#[cfg]` on a declaration).
///
/// `pub(crate)` because the search key gets the SAME check
/// ([`crate::websearch::SearchKey`]): a second, slightly different rule about
/// who may read a credential would be a rule nobody could state.
#[cfg(unix)]
pub(crate) mod platform {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use crate::ask::AskRefusal;

    /// Refuse a key file any group or other account can read.
    pub fn check_mode(key_file: &Path) -> Result<(), AskRefusal> {
        let mode = match std::fs::metadata(key_file) {
            Ok(meta) => meta.permissions().mode() & 0o777,
            Err(_) => {
                return Err(AskRefusal::NoModelKey {
                    key_file: key_file.to_path_buf(),
                });
            }
        };
        if mode & 0o077 != 0 {
            return Err(AskRefusal::KeyFileMode {
                key_file: key_file.to_path_buf(),
                mode,
            });
        }
        Ok(())
    }
}

#[cfg(not(unix))]
pub(crate) mod platform {
    use std::path::Path;

    use crate::ask::AskRefusal;

    /// No POSIX mode to check here; the file's own ACL is the platform's.
    pub fn check_mode(_key_file: &Path) -> Result<(), AskRefusal> {
        Ok(())
    }
}

/// What this client has LEARNED about the endpoint it is talking to.
///
/// A degradation discovered once is remembered, so a conversation does not pay
/// two wasted round trips on every single turn to rediscover that the endpoint
/// dislikes `strict`. It only ever narrows — nothing here turns a feature back
/// on — except when the endpoint itself changes, which is possible because the
/// configuration is re-read at every call: a fact learned about one endpoint is
/// no fact at all about the next, so it is dropped with the base URL it was
/// learned for.
#[derive(Debug, Default)]
struct Learned {
    base_url: String,
    no_strict: bool,
    no_cache_control: bool,
    no_count_tokens: bool,
}

/// The real client: raw HTTPS to the Messages API, over rustls.
pub struct HttpsModelClient {
    config: ModelConfig,
    http: OnceLock<reqwest::blocking::Client>,
    learned: std::sync::Mutex<Learned>,
}

/// A non-2xx answer, as data: the status is what decides whether a retry is
/// even worth attempting, so it does not get folded into the message first.
struct Rejected {
    status: u16,
    said: String,
}

impl Rejected {
    fn says(&self) -> String {
        format!("the model answered {}: {}", self.status, self.said)
    }
}

impl HttpsModelClient {
    pub fn new(config: ModelConfig) -> HttpsModelClient {
        HttpsModelClient {
            config,
            http: OnceLock::new(),
            learned: std::sync::Mutex::new(Learned::default()),
        }
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// The learned state for THIS endpoint, reset when the endpoint changes.
    fn learned(&self, base_url: &str) -> std::sync::MutexGuard<'_, Learned> {
        let mut held = self
            .learned
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if held.base_url != base_url {
            *held = Learned {
                base_url: base_url.to_string(),
                ..Default::default()
            };
        }
        held
    }

    /// The shape this call STARTS in: what the configuration asks for, minus
    /// everything this endpoint has already refused.
    fn starting_shape(&self, config: &ModelConfig) -> Shape {
        let learned = self.learned(&config.base_url);
        Shape {
            strict: config.shape.strict && !learned.no_strict,
            cache_control: config.shape.cache_control && !learned.no_cache_control,
        }
    }

    /// Remember a rejection, so the next turn does not rediscover it.
    fn remember(&self, config: &ModelConfig, shape: Shape) {
        let mut learned = self.learned(&config.base_url);
        learned.no_strict = learned.no_strict || !shape.strict;
        learned.no_cache_control = learned.no_cache_control || !shape.cache_control;
    }

    /// The blocking HTTP client, built on FIRST USE and not before.
    ///
    /// Deliberately lazy. A blocking HTTP client must not be built or called
    /// from inside an async context — it asserts that in debug builds — and
    /// every call this client makes is on a `spawn_blocking` task. Building it
    /// where it is used keeps that true by construction rather than by a note:
    /// building it at attach, on the async path, panicked the supplier at
    /// start-up.
    fn http(&self) -> Result<&reqwest::blocking::Client, String> {
        if let Some(held) = self.http.get() {
            return Ok(held);
        }
        let built = reqwest::blocking::Client::builder()
            .timeout(self.config.timeout)
            .build()
            .map_err(|e| format!("cannot build the model client: {e}"))?;
        let _ = self.http.set(built);
        self.http
            .get()
            .ok_or_else(|| "the model client vanished after it was built".to_string())
    }

    /// One request, with the key read at this moment and dropped when it
    /// returns. A non-2xx answer carries the API's own message as data.
    ///
    /// The key travels as `x-api-key` always, and ALSO as
    /// `Authorization: Bearer` under a profile that wants it: local endpoints
    /// serving this API authenticate the way Claude-shaped clients do, the
    /// value is the same one either way, and an endpoint that reads either
    /// header is satisfied by one request rather than by a probe.
    ///
    /// `config` is the REQUEST's, not the client's: the configuration is
    /// re-read at every call, so the endpoint this goes to is the one the
    /// caller resolved, never the one attach happened to see.
    fn post(
        &self,
        config: &ModelConfig,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::blocking::Response, Rejected> {
        let key = discover_key(&config.key_file).map_err(|r| Rejected {
            status: 0,
            said: r.says(),
        })?;
        let mut post = self
            .http()
            .map_err(|said| Rejected { status: 0, said })?
            .post(format!("{}{path}", config.base_url))
            .header("x-api-key", key.clone())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        if config.compat.sends_bearer() {
            post = post.header("authorization", format!("Bearer {key}"));
        }
        let response = post.json(body).send().map_err(|e| Rejected {
            status: 0,
            said: format!("the model call failed: {}", scrub(&e.to_string())),
        })?;
        let status = response.status();
        if !status.is_success() {
            let said = response.text().unwrap_or_default();
            return Err(Rejected {
                status: status.as_u16(),
                said: said.trim().chars().take(600).collect::<String>(),
            });
        }
        Ok(response)
    }
}

/// The largest number of times one call re-sends itself in a smaller shape.
/// Two: `strict`, then `cache_control`, then the 400 is the answer.
pub const MAX_DEGRADATIONS: usize = 2;

impl ModelClient for HttpsModelClient {
    /// Count the input — or estimate it, and SAY that is what happened.
    ///
    /// Two ways the count does not happen. The profile may already know there
    /// is none (Ollama has no `/v1/messages/count_tokens`), in which case no
    /// round trip is spent discovering that on every turn; or the endpoint may
    /// answer 404, which is the same discovery made the hard way, remembered so
    /// it is made only once. Either way the budget is still CHECKED — against
    /// an estimate that says it is one.
    fn count_tokens(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<u64, String> {
        let config = &request.config;
        let estimate = |why: &str, on_event: &mut dyn FnMut(ModelEvent)| -> u64 {
            let counted = estimate_tokens(request);
            on_event(ModelEvent::Note(format!(
                "{why}, so this turn's input budget is an ESTIMATE of about {counted} tokens                  (one per {ESTIMATED_CHARS_PER_TOKEN} characters of request), not a count"
            )));
            counted
        };
        if !config.count_tokens || self.learned(&config.base_url).no_count_tokens {
            return Ok(estimate(
                &format!(
                    "the {} endpoint has no /v1/messages/count_tokens",
                    config.compat.name()
                ),
                on_event,
            ));
        }
        let response = match self.post(config, "/v1/messages/count_tokens", &request.count_body()) {
            Ok(response) => response,
            Err(rejected) if rejected.status == 404 => {
                self.learned(&config.base_url).no_count_tokens = true;
                return Ok(estimate(
                    "this endpoint answered 404 for /v1/messages/count_tokens",
                    on_event,
                ));
            }
            Err(rejected) => {
                return Err(rejected.says());
            }
        };
        let value: serde_json::Value = response
            .json()
            .map_err(|e| format!("the token count did not decode: {e}"))?;
        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("the token count carried no `input_tokens`: {value}"))
    }

    /// Stream the reply, dropping what the endpoint will not take.
    ///
    /// A 400 is the one status worth retrying, and only by sending LESS: the
    /// same prompt, the same transcript, the same question, without a feature
    /// the endpoint rejected. Every drop is an event before the retry, so the
    /// reader is told what the answer they are about to read was weakened by,
    /// and the client remembers it for the turns after this one.
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String> {
        let config = &request.config;
        let mut shape = self.starting_shape(config);
        for _ in 0..=MAX_DEGRADATIONS {
            let rejected =
                match self.post(config, "/v1/messages", &request.body_shaped(true, shape)) {
                    Ok(response) => {
                        let mut fold = Fold::default();
                        fold_stream(BufReader::new(response), &mut fold, on_event)?;
                        return Ok(fold.outcome);
                    }
                    Err(rejected) => rejected,
                };
            if rejected.status != 400 {
                return Err(rejected.says());
            }
            match degrade(shape, &rejected.said) {
                Some((smaller, note)) => {
                    on_event(ModelEvent::Note(note));
                    shape = smaller;
                    self.remember(config, shape);
                }
                None => {
                    return Err(rejected.says());
                }
            }
        }
        Err(format!(
            "the endpoint answered 400 to every shape this call could take, down to a request              with no `strict` and no `cache_control` ({} base-url {})",
            config.compat.name(),
            config.base_url
        ))
    }
}

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

/// Strip anything that could be a credential out of a transport error before it
/// becomes data. A reqwest error names the URL, never a header, but a redacted
/// query is cheaper than trusting that forever.
///
/// `pub(crate)` because the network tools of section 11.7 report their own
/// transport failures as data and must redact them the same way.
pub(crate) fn scrub(said: &str) -> String {
    said.split_whitespace()
        .map(|word| match word.split_once('?') {
            Some((head, _)) => format!("{head}?…"),
            None => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn empty_is<'a>(value: &'a str, absent: &'a str) -> &'a str {
    if value.trim().is_empty() {
        absent
    } else {
        value
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A scripted double: it answers a token count and replays an SSE
    /// transcript, so the whole verb path runs with no network in sight.
    pub(crate) struct Scripted {
        pub counted: Result<u64, String>,
        pub transcript: String,
        /// The transcripts the SECOND and later calls of one turn answer with,
        /// in order — what makes a multi-step turn scriptable. When it runs out,
        /// `transcript` answers again, which is what a client that keeps asking
        /// for the same tool looks like.
        pub then: Mutex<Vec<String>>,
        pub transport: Option<String>,
        pub seen: Mutex<Vec<ModelRequest>>,
    }

    impl Default for Scripted {
        fn default() -> Scripted {
            Scripted {
                counted: Ok(0),
                transcript: String::new(),
                then: Mutex::new(Vec::new()),
                transport: None,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Scripted {
        pub fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        /// A double that answers each call of one turn with the next
        /// transcript, and repeats the last one for ever after.
        pub fn replaying(steps: &[&str]) -> Scripted {
            let held: Vec<String> = steps.iter().map(|s| (*s).to_string()).collect();
            let last = held.last().cloned().unwrap_or_default();
            Scripted {
                // `transcript` is what a call gets once the script has run out,
                // so it is the LAST step: a model that kept being asked would
                // keep saying what it said last.
                transcript: last,
                then: Mutex::new(held),
                ..Default::default()
            }
        }

        /// The requests this double was sent, in order.
        pub fn requests(&self) -> Vec<ModelRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ModelClient for Scripted {
        fn count_tokens(
            &self,
            request: &ModelRequest,
            _on_event: &mut dyn FnMut(ModelEvent),
        ) -> Result<u64, String> {
            self.seen.lock().unwrap().push(request.clone());
            self.counted.clone()
        }

        fn stream(
            &self,
            _request: &ModelRequest,
            on_event: &mut dyn FnMut(ModelEvent),
        ) -> Result<ModelOutcome, String> {
            if let Some(e) = self.transport.as_deref() {
                return Err(e.to_string());
            }
            let next = {
                let mut held = self.then.lock().unwrap();
                if held.is_empty() {
                    self.transcript.clone()
                } else {
                    held.remove(0)
                }
            };
            let mut fold = Fold::default();
            fold_stream(next.as_bytes(), &mut fold, on_event)?;
            Ok(fold.outcome)
        }
    }

    /// A normal stream: a cached prefix, two text chunks, a clean end.
    pub(crate) const NORMAL: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1200,"#,
        r#""cache_read_input_tokens":900,"cache_creation_input_tokens":0}}}"#,
        "\n\nevent: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
        r#""text":"It is blocked "}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","#,
        r#""thinking":"never logged"}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
        r#""text":"by proof_family (Q11)."}}"#,
        "\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"#,
        r#""usage":{"output_tokens":42}}"#,
        "\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    /// The model declined.
    pub(crate) const DECLINED: &str = concat!(
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal","#,
        r#""stop_details":{"type":"refusal","category":"cyber","#,
        r#""explanation":"declined to continue"}},"usage":{"output_tokens":3}}"#,
        "\n\n",
    );

    /// The output budget stopped it, mid-answer.
    pub(crate) const BUDGET_STOP: &str = concat!(
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
        r#""text":"It is blo"}}"#,
        "\n\nevent: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"#,
        r#""usage":{"output_tokens":64000}}"#,
        "\n\n",
    );

    /// A turn that answered in prose and then made an OFFER: the tool block
    /// opens empty, its input arrives in fragments, and the turn closes on the
    /// call itself.
    pub(crate) const DRAFTED: &str = concat!(
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
        r#""text":"Two are offered."}}"#,
        "\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","#,
        r#""id":"toolu_1","name":"propose_draft","input":{}}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","#,
        r#""partial_json":"{\"alternative\": \"a1\", \"ruling_te"}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","#,
        r#""partial_json":"xt\": \"2026-09-16, owner: g: keep them.\"}"}}"#,
        "\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"#,
        r#""usage":{"output_tokens":90}}"#,
        "\n\n",
    );

    /// The tool input was cut off mid-JSON.
    pub(crate) const DRAFT_TRUNCATED: &str = concat!(
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","#,
        r#""id":"toolu_1","name":"propose_draft","input":{}}}"#,
        "\n\nevent: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","#,
        r#""partial_json":"{\"alternative\": \"a1\""}}"#,
        "\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );

    /// The stream itself failed.
    pub(crate) const STREAM_ERROR: &str = concat!(
        "event: error\n",
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        "\n\n",
    );

    pub(crate) fn request() -> ModelRequest {
        ModelRequest {
            tools: Vec::new(),
            steps: Vec::new(),
            config: ModelConfig::default(),
            prompt: Prompt {
                system: "the stance and the passages".into(),
                user: "why is this blocked?".into(),
            },
            turns: Vec::new(),
        }
    }

    /// A registry with no tool in it: what every test that is about the CALL
    /// rather than about the loop consults with, and the shape a supplier with
    /// nothing enabled has.
    pub(crate) fn none() -> crate::tools::ToolRegistry {
        crate::tools::ToolRegistry::default()
    }

    fn collect(transcript: &str) -> (Vec<String>, ModelOutcome) {
        let (text, drafts, outcome) = fold(transcript);
        assert!(drafts.is_empty(), "no draft in this transcript: {drafts:?}");
        (text, outcome)
    }

    /// Fold a transcript into its text, its drafts and its outcome.
    fn fold(transcript: &str) -> (Vec<String>, Vec<serde_json::Value>, ModelOutcome) {
        let mut text: Vec<String> = Vec::new();
        let mut drafts: Vec<serde_json::Value> = Vec::new();
        let mut fold = Fold::default();
        fold_stream(transcript.as_bytes(), &mut fold, &mut |event| match event {
            ModelEvent::Text(chunk) => {
                text.push(chunk);
            }
            ModelEvent::Draft(input) => {
                drafts.push(input);
            }
            // A fold produces no notes: they are the CLIENT's, made about the
            // endpoint, and never anything in the transcript.
            ModelEvent::Note(note) => {
                panic!("a folded transcript said {note:?}");
            }
            // Nor any tool record: those are the LOOP's, made about a call this
            // supplier ran, and never anything a stream carried.
            ModelEvent::ToolCall(call) => {
                panic!("a folded transcript called {call}");
            }
            ModelEvent::ToolResult(answered) => {
                panic!("a folded transcript answered {answered}");
            }
        })
        .expect("the transcript folded");
        (text, drafts, fold.outcome)
    }

    #[test]
    fn the_default_model_is_the_skills_current_one() {
        assert_eq!(DEFAULT_AGENT_MODEL, "claude-opus-5");
        assert_eq!(ModelConfig::default().model, DEFAULT_AGENT_MODEL);
    }

    #[test]
    fn the_body_caches_the_stable_prefix_and_carries_the_question_after_it() {
        let body = request().body(true);
        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(body["system"][0]["text"], "the stance and the passages");
        assert_eq!(
            body["system"][0]["cache_control"]["type"], "ephemeral",
            "the stable prefix is what is cached"
        );
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "why is this blocked?"
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none(),
            "the question is the volatile tail: it is never the breakpoint"
        );
        assert!(
            body.get("thinking").is_none(),
            "thinking is adaptive by default and its display stays default"
        );

        // The count is of the body that is about to be SENT, minus the two
        // fields the count endpoint has no use for.
        let count = request().count_body();
        assert_eq!(count["system"], body["system"]);
        assert_eq!(count["messages"], body["messages"]);
        assert!(count.get("max_tokens").is_none() && count.get("stream").is_none());
    }

    #[test]
    fn the_cached_prefix_is_byte_identical_across_the_turns_of_one_conversation() {
        let first = request();
        let mut third = request();
        third.prompt.user = "and what unlocks it?".into();
        third.turns = crate::conversation::turns(&{
            let mut records = crate::conversation::tests::turn(
                "run-1",
                "why is this blocked?",
                &["It is."],
                None,
            );
            records.extend(crate::conversation::tests::turn(
                "run-2",
                "by what?",
                &["By proof_family."],
                None,
            ));
            records
        });

        // The whole of what the cache is keyed on, to the byte.
        assert_eq!(
            serde_json::to_string(&first.cached_prefix()).unwrap(),
            serde_json::to_string(&third.cached_prefix()).unwrap(),
            "the stable prefix moved between turns of one conversation"
        );

        // And the third turn carries both prior turns, in order, with the
        // second breakpoint on the settled history rather than on the question.
        let body = third.body(true);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 5, "two prior turns, then this one");
        assert_eq!(messages[0]["content"][0]["text"], "why is this blocked?");
        assert_eq!(messages[2]["content"][0]["text"], "by what?");
        assert_eq!(messages[4]["content"][0]["text"], "and what unlocks it?");
        let marked = messages
            .iter()
            .filter(|m| m["content"][0].get("cache_control").is_some())
            .count();
        assert_eq!(marked, 1, "{messages:?}");
        assert_eq!(
            messages[3]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );

        // Two breakpoints in the whole request, of the four one may carry.
        assert_eq!(
            serde_json::to_string(&body)
                .unwrap()
                .matches("cache_control")
                .count(),
            2
        );
    }

    #[test]
    fn the_conversation_budget_refuses_the_turn_that_would_cross_it() {
        let mut request = request();
        request.config.max_conversation_tokens = 5_000;
        let client = Scripted {
            counted: Ok(1_200),
            transcript: NORMAL.into(),
            ..Default::default()
        };

        // Room for it: the turn goes.
        let outcome =
            consult(&client, &request, 3_000, &none(), &mut |_| {}).expect("within budget");
        assert!(outcome.complete());

        // One more turn would cross it, so nothing is sent.
        let e = consult(&client, &request, 3_900, &none(), &mut |_| {}).expect_err("a refusal");
        assert_eq!(
            e,
            AskRefusal::OverConversationBudget {
                spent: 3_900,
                counted: 1_200,
                budget: 5_000
            }
        );
        let said = e.says();
        assert!(
            said.contains("3900") && said.contains("1200") && said.contains("5000"),
            "{said}"
        );
        assert!(said.contains("--agent-max-conversation-tokens"), "{said}");
        assert_eq!(
            client.count(),
            2,
            "the count happened, the second call did not"
        );

        // Zero is no ceiling.
        request.config.max_conversation_tokens = 0;
        assert!(consult(&client, &request, u64::MAX, &none(), &mut |_| {}).is_ok());
    }

    #[test]
    fn a_turns_cost_is_the_prompt_however_it_was_served_plus_the_output() {
        let outcome = ModelOutcome {
            input_tokens: 300,
            cache_creation_input_tokens: 50,
            cache_read_input_tokens: 900,
            output_tokens: 42,
            ..Default::default()
        };
        assert_eq!(outcome.tokens(), 1292);
        assert_eq!(ModelOutcome::default().tokens(), 0);
    }

    #[test]
    fn a_normal_stream_folds_into_text_chunks_and_a_clean_outcome() {
        let (text, outcome) = collect(NORMAL);
        assert_eq!(text, vec!["It is blocked ", "by proof_family (Q11)."]);
        assert!(outcome.complete() && outcome.exit() == 0);
        assert_eq!(outcome.says(DEFAULT_MAX_OUTPUT_TOKENS), None);
        assert_eq!(outcome.input_tokens, 1200);
        assert_eq!(outcome.cache_read_input_tokens, 900);
        assert_eq!(outcome.output_tokens, 42);
    }

    #[test]
    fn a_refusal_and_a_budget_stop_are_both_outcomes_with_a_reason() {
        let (text, declined) = collect(DECLINED);
        assert!(text.is_empty());
        assert!(!declined.complete() && declined.exit() == 1);
        let said = declined.says(DEFAULT_MAX_OUTPUT_TOKENS).expect("a reason");
        assert!(said.contains("the model declined (cyber)"), "{said}");
        assert!(said.contains("declined to continue"), "{said}");

        let (text, stopped) = collect(BUDGET_STOP);
        assert_eq!(text, vec!["It is blo"], "the partial text is KEPT");
        assert_eq!(stopped.exit(), 1);
        let said = stopped.says(DEFAULT_MAX_OUTPUT_TOKENS).expect("a reason");
        assert!(said.contains("is partial"), "{said}");
        assert!(said.contains("64000"), "{said}");
    }

    #[test]
    fn the_draft_tool_is_declared_on_every_request_and_sits_in_the_cached_prefix() {
        let body = request().body(true);
        let tools = body["tools"].as_array().expect("one tool");
        assert_eq!(tools.len(), 1, "one tool, and only one");
        assert_eq!(tools[0]["name"], DRAFT_TOOL);
        assert_eq!(
            tools[0]["strict"], true,
            "strict is what guarantees the input validates"
        );
        assert_eq!(tools[0]["input_schema"]["additionalProperties"], false);
        assert_eq!(
            tools[0]["input_schema"]["required"],
            serde_json::json!(["alternative", "ruling_text", "sources"])
        );
        assert!(
            body.get("tool_choice").is_none(),
            "auto: an agent that must always propose is an agent that rules"
        );
        assert!(
            tools[0].get("eager_input_streaming").is_none(),
            "a draft is small and arrives whole or not at all"
        );
        assert!(
            body.get("output_config").is_none(),
            "the response is PROSE with an offer beside it, not one JSON document"
        );

        // Tools render before the system block, so they are part of what the
        // system breakpoint caches — and part of what must not move.
        let prefix = request().cached_prefix();
        assert_eq!(prefix["tools"], body["tools"]);
        assert_eq!(request().count_body()["tools"], body["tools"]);
    }

    #[test]
    fn a_degraded_shape_drops_strict_and_the_breakpoints_and_changes_nothing_else() {
        let mut request = request();
        request.turns = crate::conversation::turns(&crate::conversation::tests::turn(
            "run-1",
            "why is this blocked?",
            &["It is."],
            None,
        ));
        let full = request.body(true);
        assert_eq!(full["tools"][0]["strict"], true);
        assert_eq!(
            serde_json::to_string(&full)
                .unwrap()
                .matches("cache_control")
                .count(),
            2
        );

        request.config.shape = Shape::full().without_strict();
        let lax = request.body(true);
        assert!(
            lax["tools"][0].get("strict").is_none(),
            "an endpoint that rejects `strict` gets the tool without it"
        );
        assert_eq!(
            lax["tools"][0]["input_schema"], full["tools"][0]["input_schema"],
            "the schema itself is untouched: only the guarantee goes"
        );
        assert_eq!(lax["messages"], full["messages"]);

        request.config.shape = Shape::full().without_cache_control();
        let uncached = request.body(true);
        assert_eq!(
            serde_json::to_string(&uncached)
                .unwrap()
                .matches("cache_control")
                .count(),
            0,
            "no breakpoint survives, in the system block or the history"
        );
        assert_eq!(uncached["tools"][0]["strict"], true, "the other stays on");
        assert_eq!(
            uncached["system"][0]["text"], full["system"][0]["text"],
            "the stance and the passages are the same bytes"
        );
        assert_eq!(uncached["max_tokens"], full["max_tokens"]);

        // The count body degrades with it: counting a body that is not the one
        // about to be sent is not a budget.
        request.config.shape = Shape {
            strict: false,
            cache_control: false,
        };
        let counted = request.count_body();
        assert!(counted["tools"][0].get("strict").is_none());
        assert_eq!(
            serde_json::to_string(&counted)
                .unwrap()
                .matches("cache_control")
                .count(),
            0
        );
    }

    #[test]
    fn a_drafted_turn_folds_its_tool_call_into_a_draft_and_ends_clean() {
        let (text, drafts, outcome) = fold(DRAFTED);
        assert_eq!(text, vec!["Two are offered."], "the prose still arrives");
        assert_eq!(drafts.len(), 1, "{drafts:?}");
        assert_eq!(drafts[0]["alternative"], "a1", "the fragments rejoin");
        assert_eq!(drafts[0]["ruling_text"], "2026-09-16, owner: g: keep them.");

        // A turn that ends by making the offer it was asked for is a turn that
        // ended: this verb answers no tool call and has no loop to continue.
        assert_eq!(outcome.stop_reason, TOOL_USE);
        assert!(outcome.complete() && outcome.exit() == 0);
        assert_eq!(outcome.says(DEFAULT_MAX_OUTPUT_TOKENS), None);

        // A truncated input travels as the string it was, so the supplier can
        // SAY what arrived rather than quietly drop it.
        let (_, drafts, _) = fold(DRAFT_TRUNCATED);
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].as_str(),
            Some("{\"alternative\": \"a1\""),
            "{drafts:?}"
        );
    }

    #[test]
    fn the_ladder_drops_one_feature_at_a_time_and_then_runs_out() {
        // Named: it goes straight to the rung the endpoint blamed.
        let (shape, note) =
            degrade(Shape::full(), "system.0: unexpected field `cache_control`").expect("a rung");
        assert_eq!(
            shape,
            Shape::full().without_cache_control(),
            "`strict` was not what it complained about"
        );
        assert!(
            note.contains("`cache_control`") && note.contains("400"),
            "{note}"
        );
        assert!(note.contains("pays for its passages"), "{note}");

        // Unnamed: the rungs are taken in order, cheapest loss first.
        let (shape, note) = degrade(Shape::full(), "invalid request").expect("a rung");
        assert_eq!(shape, Shape::full().without_strict());
        assert!(note.contains("`strict`"), "{note}");
        assert!(note.contains("checked by this supplier"), "{note}");

        let (shape, _) = degrade(shape, "invalid request").expect("the second rung");
        assert_eq!(
            shape,
            Shape {
                strict: false,
                cache_control: false
            }
        );

        // And then there is nothing left to drop: a 400 is the answer.
        assert_eq!(degrade(shape, "invalid request"), None);
        assert_eq!(degrade(shape, "unexpected field `strict`"), None);

        // A rung already taken is not taken twice.
        let lax = Shape::full().without_strict();
        let (shape, note) = degrade(lax, "unexpected field `strict`").expect("a rung");
        assert_eq!(
            shape,
            Shape {
                strict: false,
                cache_control: false
            }
        );
        assert!(note.contains("`cache_control`"), "{note}");

        // An endpoint's own words reach the note, bounded and on one line.
        let (_, note) = degrade(Shape::full(), &format!("a\n{}", "x".repeat(900))).expect("a rung");
        assert!(!note.contains('\n'), "{note}");
        assert!(note.len() < 500, "{} chars", note.len());
    }

    #[test]
    fn an_estimate_is_of_the_body_about_to_be_sent_and_grows_with_it() {
        let first = request();
        let small = estimate_tokens(&first);
        assert!(small > 0);

        let mut bigger = request();
        bigger.prompt.system = format!("{} {}", first.prompt.system, "passage ".repeat(1000));
        assert!(
            estimate_tokens(&bigger) > small + 1000,
            "it moves with the prompt it is an estimate of"
        );

        // Pessimistic on purpose: an estimate that under-counts is not a bound.
        let body = serde_json::to_string(&first.count_body()).unwrap();
        assert_eq!(small, (body.chars().count() as u64).div_ceil(3));
        assert!(small >= (body.chars().count() as u64) / 4);
    }

    #[test]
    fn a_stream_error_and_a_silent_end_are_both_said() {
        let mut fold = Fold::default();
        let e = fold_stream(STREAM_ERROR.as_bytes(), &mut fold, &mut |_| {}).unwrap_err();
        assert!(
            e.contains("overloaded_error") && e.contains("Overloaded"),
            "{e}"
        );

        let silent = ModelOutcome::default();
        assert_eq!(
            silent.says(1).as_deref(),
            Some("the stream ended without a stop reason")
        );
        let paused = ModelOutcome {
            stop_reason: "pause_turn".into(),
            ..Default::default()
        };
        assert!(paused.says(1).unwrap().contains("pause_turn"));
    }

    #[test]
    fn the_input_budget_refuses_before_the_call_and_costs_nothing() {
        let mut request = request();
        request.config.max_input_tokens = 1000;
        let client = Scripted {
            counted: Ok(1001),
            transcript: NORMAL.into(),
            ..Default::default()
        };
        let e = consult(&client, &request, 0, &none(), &mut |_| {}).expect_err("a refusal");
        let said = e.says();
        assert!(said.contains("1001") && said.contains("1000"), "{said}");
        assert_eq!(client.count(), 1, "the count happened, the call did not");

        request.config.max_input_tokens = 1001;
        let outcome = consult(&client, &request, 0, &none(), &mut |_| {}).expect("within budget");
        assert!(outcome.complete());
    }

    #[test]
    fn a_transport_failure_is_data_not_a_panic() {
        let client = Scripted {
            counted: Ok(10),
            transport: Some("connection reset".into()),
            ..Default::default()
        };
        let e = consult(&client, &request(), 0, &none(), &mut |_| {}).expect_err("a refusal");
        assert!(e.says().contains("connection reset"), "{e}");

        let counting = Scripted {
            counted: Err("dns failure".into()),
            ..Default::default()
        };
        let e = consult(&counting, &request(), 0, &none(), &mut |_| {}).expect_err("a refusal");
        assert!(e.says().contains("dns failure"), "{e}");
    }

    #[test]
    fn the_key_comes_from_the_environment_first_and_a_mode_checked_file_second() {
        let dir = std::env::temp_dir().join(format!("glade-gyld-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api-key");

        // No environment key in this process and no file: the refusal names
        // both places, and never a value.
        if std::env::var(KEY_ENV)
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            let e = discover_key(&path).expect_err("a refusal");
            assert!(e.says().contains("ANTHROPIC_API_KEY"), "{e}");
            assert!(e.says().contains("api-key"), "{e}");

            std::fs::write(&path, "sk-test-value\n# a comment\n").unwrap();
            modes::set(&path, 0o644);
            let e = discover_key(&path).expect_err("a mode refusal");
            let said = e.says();
            assert!(said.contains("readable"), "{said}");
            assert!(!said.contains("sk-test-value"), "a key never reaches data");

            modes::set(&path, 0o600);
            assert_eq!(discover_key(&path).unwrap(), "sk-test-value");

            std::fs::write(&path, "\n").unwrap();
            modes::set(&path, 0o600);
            assert!(discover_key(&path).is_err(), "an empty key file is no key");
        } else {
            eprintln!("SKIP: {KEY_ENV} is set in this environment");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mode setter, behind an explicit platform boundary like the checker
    /// it exercises.
    #[cfg(unix)]
    mod modes {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        pub fn set(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    #[cfg(not(unix))]
    mod modes {
        use std::path::Path;

        pub fn set(_path: &Path, _mode: u32) {}
    }

    #[test]
    fn a_transport_error_never_carries_a_query_string() {
        let said = scrub("failed for url (https://api.anthropic.com/v1/messages?key=abc)");
        assert!(said.contains("/v1/messages?"), "{said}");
        assert!(!said.contains("key=abc"), "{said}");
    }

    // ---- The tool loop (GyldAskAgent.md section 11.1, plan step A.1) -------

    /// One SSE body out of the events it carries.
    fn sse(events: Vec<serde_json::Value>) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    /// A turn that says something and then asks for a tool.
    fn calling(text: &str, calls: &[(&str, &str, serde_json::Value)]) -> String {
        let mut events = vec![
            serde_json::json!({"type": "message_start",
                               "message": {"usage": {"input_tokens": 100}}}),
            serde_json::json!({"type": "content_block_start", "index": 0,
                               "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "text_delta", "text": text}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ];
        for (at, (id, name, input)) in calls.iter().enumerate() {
            let index = at + 1;
            events.push(serde_json::json!({
                "type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
            events.push(serde_json::json!({
                "type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
            events.push(serde_json::json!({"type": "content_block_stop", "index": index}));
        }
        events.push(serde_json::json!({"type": "message_delta",
                                       "delta": {"stop_reason": TOOL_USE},
                                       "usage": {"output_tokens": 20}}));
        events.push(serde_json::json!({"type": "message_stop"}));
        sse(events)
    }

    /// A turn that answers and ends.
    fn answering(text: &str) -> String {
        sse(vec![
            serde_json::json!({"type": "message_start",
                               "message": {"usage": {"input_tokens": 300}}}),
            serde_json::json!({"type": "content_block_start", "index": 0,
                               "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "text_delta", "text": text}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta",
                               "delta": {"stop_reason": END_TURN},
                               "usage": {"output_tokens": 40}}),
            serde_json::json!({"type": "message_stop"}),
        ])
    }

    /// Everything one consultation produced, in the order it produced it.
    fn ran(
        client: &Scripted,
        tools: &crate::tools::ToolRegistry,
    ) -> (Vec<ModelEvent>, ModelOutcome) {
        let mut events: Vec<ModelEvent> = Vec::new();
        let outcome = consult(client, &request(), 0, tools, &mut |event| {
            events.push(event);
        })
        .expect("the turn ran");
        (events, outcome)
    }

    fn calls_of(events: &[ModelEvent]) -> Vec<serde_json::Value> {
        events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    fn results_of(events: &[ModelEvent]) -> Vec<serde_json::Value> {
        events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolResult(answered) => Some(answered.clone()),
                _ => None,
            })
            .collect()
    }

    fn notes_of(events: &[ModelEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::Note(note) => Some(note.clone()),
                _ => None,
            })
            .collect()
    }

    fn reading(text: &str) -> crate::tools::ToolRegistry {
        crate::tools::ToolRegistry::of(
            vec![crate::tools::tests::Scripted::answering(
                "read_source",
                text,
            )],
            crate::tools::ToolBudgets::default(),
        )
    }

    #[test]
    fn a_two_step_turn_runs_the_tool_and_answers_with_what_it_returned() {
        let client = Scripted::replaying(&[
            &calling(
                "Let me read Q11. ",
                &[("toolu_1", "read_source", serde_json::json!({"tag": "Q11"}))],
            ),
            &answering("Q11 says key custody is a buy."),
        ]);
        let (events, outcome) = ran(&client, &reading("| Q11 | Key custody | buy |"));

        assert_eq!(
            outcome.stop_reason, END_TURN,
            "the turn ended on the ANSWER"
        );
        assert!(outcome.complete() && outcome.exit() == 0);
        assert_eq!(
            outcome.input_tokens, 400,
            "a multi-step turn costs the SUM of its steps"
        );
        assert_eq!(outcome.output_tokens, 60);

        // The call and its result are both on the run, in the order they
        // happened and around the prose they sit between.
        let calls = calls_of(&events);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["name"], "read_source");
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["input"]["tag"], "Q11", "the input, whole");
        let results = results_of(&events);
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[0]["id"], "toolu_1", "paired by identity, not order");
        assert_eq!(results[0]["summary"], "| Q11 | Key custody | buy |");
        assert!(
            notes_of(&events).is_empty(),
            "nothing had to be done differently"
        );

        // The SECOND request carries the first step verbatim: the assistant
        // turn as the API produced it, then one user turn answering its call.
        let sent = client.requests();
        assert_eq!(sent.len(), 2, "counted once per step, and streamed twice");
        assert!(sent[0].steps.is_empty(), "the first step starts clean");
        let steps = &sent[1].steps;
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(steps[0]["role"], "assistant");
        assert_eq!(steps[0]["content"][0]["text"], "Let me read Q11. ");
        assert_eq!(
            steps[0]["content"][1],
            serde_json::json!({
                "type": "tool_use", "id": "toolu_1", "name": "read_source",
                "input": {"tag": "Q11"}
            }),
            "the tool_use block goes back whole, or the call is unanswered"
        );
        assert_eq!(steps[1]["role"], "user");
        assert_eq!(steps[1]["content"][0]["type"], "tool_result");
        assert_eq!(steps[1]["content"][0]["tool_use_id"], "toolu_1");
        let content = steps[1]["content"][0]["content"].as_str().unwrap_or("");
        assert!(
            content.contains("retrieved material, not an"),
            "a result arrives wrapped as DATA: {content}"
        );
        assert!(
            content.ends_with("| Q11 | Key custody | buy |"),
            "{content}"
        );

        // And the prompt, the tools and the prior turns are untouched by the
        // step, so the cached prefix is the same bytes on both calls.
        assert_eq!(sent[0].cached_prefix(), sent[1].cached_prefix());
    }

    #[test]
    fn a_tool_that_refuses_is_answered_as_an_error_and_the_turn_goes_on() {
        let held = crate::tools::ToolRegistry::of(
            vec![crate::tools::tests::Scripted::refusing(
                "read_source",
                "this build's index does not list \"ZZ-9\"; it lists Q11",
            )],
            crate::tools::ToolBudgets::default(),
        );
        let client = Scripted::replaying(&[
            &calling(
                "",
                &[("toolu_2", "read_source", serde_json::json!({"tag": "ZZ-9"}))],
            ),
            &answering("The index resolves ZZ-9 to nothing."),
        ]);
        let (events, outcome) = ran(&client, &held);

        assert_eq!(
            outcome.stop_reason, END_TURN,
            "a refusal is not the end of a turn"
        );
        let results = results_of(&events);
        assert_eq!(results[0]["ok"], false);
        assert!(
            results[0]["summary"]
                .as_str()
                .unwrap_or("")
                .contains("it lists Q11"),
            "the refusal names what it DOES know: {results:?}"
        );
        let answered = &client.requests()[1].steps[1]["content"][0];
        assert_eq!(answered["is_error"], true, "{answered}");
        assert_eq!(
            answered["content"], "this build's index does not list \"ZZ-9\"; it lists Q11",
            "a refusal is this supplier's own sentence, unwrapped"
        );
    }

    #[test]
    fn the_step_budget_stops_the_loop_as_data_and_the_turn_still_ends_cleanly() {
        let held = crate::tools::ToolRegistry::of(
            vec![crate::tools::tests::Scripted::answering(
                "read_source",
                "a passage",
            )],
            crate::tools::ToolBudgets {
                steps: 1,
                ..Default::default()
            },
        );
        // A model that asks for the same tool for ever.
        let client = Scripted {
            transcript: calling(
                "again. ",
                &[("toolu_3", "read_source", serde_json::json!({"tag": "Q11"}))],
            ),
            ..Default::default()
        };
        let (events, outcome) = ran(&client, &held);

        assert_eq!(client.count(), 2, "one step, then the budget");
        assert_eq!(calls_of(&events).len(), 1, "one tool was RUN");
        let notes = notes_of(&events);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("per-turn budget"), "{notes:?}");
        assert!(notes[0].contains("1 tool step"), "{notes:?}");
        assert!(
            outcome.complete() && outcome.exit() == 0,
            "the budget is a fact about the run, not a failure of it: {outcome:?}"
        );
        assert_eq!(
            outcome.says(64_000),
            None,
            "the close says nothing; the note already said it"
        );
    }

    #[test]
    fn a_result_over_the_byte_cap_is_cut_marked_and_said_to_be_a_prefix() {
        let held = crate::tools::ToolRegistry::of(
            vec![crate::tools::tests::Scripted::answering(
                "read_source",
                &"p".repeat(200),
            )],
            crate::tools::ToolBudgets {
                bytes: 20,
                ..Default::default()
            },
        );
        let client = Scripted::replaying(&[
            &calling("", &[("toolu_4", "read_source", serde_json::json!({}))]),
            &answering("done"),
        ]);
        let (events, _) = ran(&client, &held);
        let results = results_of(&events);
        assert_eq!(results[0]["truncated"], true);
        assert_eq!(results[0]["bytes"], 200, "the size BEFORE the cut");
        assert_eq!(results[0]["summary"], "p".repeat(20));
        let content = client.requests()[1].steps[1]["content"][0]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            content.contains("capped") && content.contains("a prefix"),
            "{content}"
        );
    }

    #[test]
    fn a_tool_the_allow_list_does_not_carry_is_refused_as_data_without_running() {
        let client = Scripted::replaying(&[
            &calling(
                "",
                &[(
                    "toolu_5",
                    "fetch_url",
                    serde_json::json!({"url": "https://example.test/"}),
                )],
            ),
            &answering("I cannot reach a page from here."),
        ]);
        let (events, outcome) = ran(&client, &reading("never asked for"));

        assert_eq!(outcome.stop_reason, END_TURN);
        let results = results_of(&events);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["ok"], false);
        let said = results[0]["summary"].as_str().unwrap_or("").to_string();
        assert!(said.contains("no tool \"fetch_url\" is enabled"), "{said}");
        assert!(
            said.contains("read_source"),
            "it names what IS enabled: {said}"
        );
        assert_eq!(
            client.requests()[1].steps[1]["content"][0]["is_error"],
            true,
            "the call is ANSWERED, because an unanswered tool_use is a 400"
        );
    }

    #[test]
    fn a_turn_whose_only_call_is_the_draft_still_ends_and_one_beside_a_read_does_not() {
        // The draft alone: the offer IS the ending, and no tool runs (section 8).
        let only = Scripted {
            transcript: calling(
                "I propose owner_held_only. ",
                &[(
                    "toolu_6",
                    DRAFT_TOOL,
                    serde_json::json!({"alternative": "a1", "ruling_text": "x", "sources": []}),
                )],
            ),
            ..Default::default()
        };
        let (events, outcome) = ran(&only, &reading("a passage"));
        assert_eq!(only.count(), 1, "one call, and no second");
        assert_eq!(outcome.stop_reason, TOOL_USE);
        assert!(outcome.complete());
        assert!(
            calls_of(&events).is_empty(),
            "the supplier never runs the draft tool"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ModelEvent::Draft(_)))
                .count(),
            1,
            "and the draft still arrives"
        );

        // Beside a read, the turn continues — and the draft's own call is
        // answered too, because every tool_use of a replayed turn must be.
        let beside = Scripted::replaying(&[
            &calling(
                "",
                &[
                    ("toolu_7", "read_source", serde_json::json!({"tag": "Q11"})),
                    (
                        "toolu_8",
                        DRAFT_TOOL,
                        serde_json::json!({"alternative": "a1", "ruling_text": "x", "sources": []}),
                    ),
                ],
            ),
            &answering("and that is why."),
        ]);
        let (events, outcome) = ran(&beside, &reading("a passage"));
        assert_eq!(outcome.stop_reason, END_TURN);
        assert_eq!(calls_of(&events).len(), 1, "only the read is run");
        let answers = beside.requests()[1].steps[1]["content"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(answers.len(), 2, "both calls are answered: {answers:?}");
        let ids: Vec<&str> = answers
            .iter()
            .map(|a| a["tool_use_id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, vec!["toolu_7", "toolu_8"]);
        assert!(
            answers[1]["content"]
                .as_str()
                .unwrap_or("")
                .contains("not a ruling"),
            "{answers:?}"
        );
    }

    #[test]
    fn a_thinking_block_is_kept_whole_for_replay_and_never_reaches_a_record() {
        let transcript = sse(vec![
            serde_json::json!({"type": "message_start", "message": {"usage": {}}}),
            serde_json::json!({"type": "content_block_start", "index": 0,
                               "content_block": {"type": "thinking", "thinking": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "thinking_delta", "thinking": "weighing it"}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                               "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "content_block_start", "index": 1,
                               "content_block": {"type": "tool_use", "id": "toolu_9",
                                                 "name": "read_source", "input": {}}}),
            serde_json::json!({"type": "content_block_delta", "index": 1,
                               "delta": {"type": "input_json_delta",
                                         "partial_json": "{\"tag\":\"Q11\"}"}}),
            serde_json::json!({"type": "content_block_stop", "index": 1}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": TOOL_USE}}),
        ]);
        let client = Scripted::replaying(&[&transcript, &answering("read.")]);
        let (events, _) = ran(&client, &reading("a passage"));

        assert!(
            !events.iter().any(|event| matches!(
                event, ModelEvent::Text(chunk) if chunk.contains("weighing")
            )),
            "reasoning is not text and reaches no record: {events:?}"
        );
        let replayed = &client.requests()[1].steps[0]["content"][0];
        assert_eq!(
            replayed,
            &serde_json::json!({
                "type": "thinking", "thinking": "weighing it", "signature": "sig-abc"
            }),
            "the block goes back exactly as it arrived, signature and all"
        );
    }
}
