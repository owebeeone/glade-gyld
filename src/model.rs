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
        let mut tool = draft_tool();
        if !shape.strict {
            if let Some(fields) = tool.as_object_mut() {
                fields.remove("strict");
            }
        }
        let mut system = serde_json::json!({
            "type": "text",
            "text": self.prompt.system,
        });
        if shape.cache_control {
            system["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }
        serde_json::json!({
            "model": self.config.model,
            "tools": [tool],
            "system": [system],
            "messages": conversation::messages(
                &self.turns,
                &self.prompt.user,
                shape.cache_control,
            ),
        })
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
}

/// The one clean ending.
pub const END_TURN: &str = "end_turn";
/// The ending that means the output budget stopped it.
pub const MAX_TOKENS: &str = "max_tokens";
/// The ending that means the model declined.
pub const REFUSAL: &str = "refusal";
/// The ending that means the turn closed on a tool call.
///
/// A CLEAN ending here. This verb declares exactly one tool, whose whole
/// purpose is to carry a draft back, and it never answers the call: there is no
/// loop to continue and nothing more the model would say. A turn that ends by
/// making the offer the reader asked for is a turn that ended.
pub const TOOL_USE: &str = "tool_use";

impl ModelOutcome {
    /// The turn ended because the answer ended — or because it ended in the
    /// draft it was asked for, which is the same thing here (see [`TOOL_USE`]).
    pub fn complete(&self) -> bool {
        self.stop_reason == END_TURN || self.stop_reason == TOOL_USE
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

/// One consultation: count, check both input budgets, then stream.
///
/// The count happens BEFORE the call, so an over-budget turn is refused with
/// its numbers and costs nothing. `spent` is what this CONVERSATION has already
/// spent across its earlier turns — the running total of section 7 — and the
/// turn that would cross the ceiling is refused here rather than sent and
/// regretted.
///
/// The per-run budget is checked first: it is about this question, and a
/// question too big to ask is too big whatever the conversation has spent.
pub fn consult(
    client: &dyn ModelClient,
    request: &ModelRequest,
    spent: u64,
    on_event: &mut dyn FnMut(ModelEvent),
) -> Result<ModelOutcome, AskRefusal> {
    let counted = client
        .count_tokens(request, on_event)
        .map_err(|reason| AskRefusal::Transport { reason })?;
    if counted > request.config.max_input_tokens {
        return Err(AskRefusal::OverInputBudget {
            counted,
            budget: request.config.max_input_tokens,
        });
    }
    let budget = request.config.max_conversation_tokens;
    if budget > 0 && spent.saturating_add(counted) > budget {
        return Err(AskRefusal::OverConversationBudget {
            spent,
            counted,
            budget,
        });
    }
    client
        .stream(request, on_event)
        .map_err(|reason| AskRefusal::Transport { reason })
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
#[cfg(unix)]
mod platform {
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
mod platform {
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

/// The in-flight state of one folded stream: the outcome so far, and the tool
/// inputs still arriving.
///
/// A tool input does NOT arrive whole. It is opened by a `content_block_start`
/// naming the tool, filled by `input_json_delta` fragments, and closed by a
/// `content_block_stop` — so the fold has to hold the fragments somewhere until
/// the block closes. Here, and not on [`ModelOutcome`]: an outcome is what the
/// turn RESULTED in, and a half-arrived tool input is not a result.
#[derive(Debug, Default)]
pub struct Fold {
    pub outcome: ModelOutcome,
    /// The open tool blocks: content-block index, tool name, JSON so far.
    open: Vec<(u64, String, String)>,
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
            // A tool block opens here and is EMPTY: its input arrives as
            // fragments and is only whole at `content_block_stop`.
            let block = value.get("content_block");
            let kind = block
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if kind == "tool_use" {
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                fold.open.push((index_of(&value), name, String::new()));
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta");
            let kind = delta
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // A `thinking_delta` is reasoning and never reaches a log record.
            if kind == "text_delta" {
                if let Some(text) = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
                    on_event(ModelEvent::Text(text.to_string()));
                }
            }
            if kind == "input_json_delta" {
                let fragment = delta
                    .and_then(|d| d.get("partial_json"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let index = index_of(&value);
                if let Some(open) = fold.open.iter_mut().find(|(i, _, _)| *i == index) {
                    open.2.push_str(fragment);
                }
            }
        }
        "content_block_stop" => {
            let index = index_of(&value);
            if let Some(at) = fold.open.iter().position(|(i, _, _)| *i == index) {
                let (_, name, json) = fold.open.remove(at);
                if name == DRAFT_TOOL {
                    // A tool input that is not even JSON travels as the string
                    // it was, so the supplier can SAY what arrived rather than
                    // quietly drop it.
                    let input = match serde_json::from_str::<serde_json::Value>(&json) {
                        Ok(value) => value,
                        Err(_) => serde_json::Value::String(json),
                    };
                    on_event(ModelEvent::Draft(input));
                }
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
fn scrub(said: &str) -> String {
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
        pub transport: Option<String>,
        pub seen: Mutex<Vec<ModelRequest>>,
    }

    impl Default for Scripted {
        fn default() -> Scripted {
            Scripted {
                counted: Ok(0),
                transcript: String::new(),
                transport: None,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Scripted {
        pub fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
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
            let mut fold = Fold::default();
            fold_stream(self.transcript.as_bytes(), &mut fold, on_event)?;
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
            config: ModelConfig::default(),
            prompt: Prompt {
                system: "the stance and the passages".into(),
                user: "why is this blocked?".into(),
            },
            turns: Vec::new(),
        }
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
        let outcome = consult(&client, &request, 3_000, &mut |_| {}).expect("within budget");
        assert!(outcome.complete());

        // One more turn would cross it, so nothing is sent.
        let e = consult(&client, &request, 3_900, &mut |_| {}).expect_err("a refusal");
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
        assert!(consult(&client, &request, u64::MAX, &mut |_| {}).is_ok());
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
        let e = consult(&client, &request, 0, &mut |_| {}).expect_err("a refusal");
        let said = e.says();
        assert!(said.contains("1001") && said.contains("1000"), "{said}");
        assert_eq!(client.count(), 1, "the count happened, the call did not");

        request.config.max_input_tokens = 1001;
        let outcome = consult(&client, &request, 0, &mut |_| {}).expect("within budget");
        assert!(outcome.complete());
    }

    #[test]
    fn a_transport_failure_is_data_not_a_panic() {
        let client = Scripted {
            counted: Ok(10),
            transport: Some("connection reset".into()),
            ..Default::default()
        };
        let e = consult(&client, &request(), 0, &mut |_| {}).expect_err("a refusal");
        assert!(e.says().contains("connection reset"), "{e}");

        let counting = Scripted {
            counted: Err("dns failure".into()),
            ..Default::default()
        };
        let e = consult(&counting, &request(), 0, &mut |_| {}).expect_err("a refusal");
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
}
