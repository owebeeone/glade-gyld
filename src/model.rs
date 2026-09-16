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

/// Everything the model call is configured with. No key: see the module note.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelConfig {
    pub model: String,
    pub base_url: String,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    /// The running total one conversation may spend across its turns; `0` is
    /// no ceiling.
    pub max_conversation_tokens: u64,
    pub timeout: Duration,
    /// Where a key is read from when the environment carries none.
    pub key_file: PathBuf,
}

impl Default for ModelConfig {
    fn default() -> ModelConfig {
        ModelConfig {
            model: DEFAULT_AGENT_MODEL.into(),
            base_url: DEFAULT_BASE_URL.into(),
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            max_conversation_tokens: DEFAULT_MAX_CONVERSATION_TOKENS,
            timeout: Duration::from_secs(DEFAULT_MODEL_TIMEOUT_SECS),
            key_file: PathBuf::new(),
        }
    }
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
        let mut body = self.shared();
        body["max_tokens"] = self.config.max_output_tokens.into();
        body["stream"] = stream.into();
        body
    }

    /// The `count_tokens` body: the same prompt and the same prior turns, with
    /// neither `max_tokens` nor `stream`. Counting the body that is about to be
    /// SENT is the whole point: a count of something else is not a budget, and
    /// a count that forgot the conversation is not this turn's count.
    pub fn count_body(&self) -> serde_json::Value {
        self.shared()
    }

    /// The bytes the cache is keyed on, up to and including the system block:
    /// what must be byte-identical from one turn of a conversation to the next.
    /// Asserting on this is how a silent invalidator is caught by a test rather
    /// than by a bill.
    pub fn cached_prefix(&self) -> serde_json::Value {
        let body = self.shared();
        serde_json::json!({
            "model": body["model"],
            "system": body["system"],
        })
    }

    fn shared(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.config.model,
            "system": [{
                "type": "text",
                "text": self.prompt.system,
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": conversation::messages(&self.turns, &self.prompt.user),
        })
    }
}

/// One thing the stream produced.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    /// A chunk of the answer, as it arrived.
    Text(String),
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

impl ModelOutcome {
    /// The turn ended because the answer ended.
    pub fn complete(&self) -> bool {
        self.stop_reason == END_TURN
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
            END_TURN => None,
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
    /// Count the input tokens of `request` before it is sent.
    fn count_tokens(&self, request: &ModelRequest) -> Result<u64, String>;

    /// Stream the reply, calling `on_event` for each event as it arrives.
    /// An `Err` is a transport failure, which the caller turns into data.
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String>;
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
        .count_tokens(request)
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
/// `ANTHROPIC_API_KEY` in the supplier's environment first, then the key file.
/// The file is MODE CHECKED: a credential any other account on the machine can
/// read is refused rather than used, because a supplier that quietly accepts
/// one teaches everybody that it is fine.
pub fn discover_key(key_file: &Path) -> Result<String, AskRefusal> {
    if let Ok(value) = std::env::var(KEY_ENV) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
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

/// The real client: raw HTTPS to the Messages API, over rustls.
pub struct HttpsModelClient {
    config: ModelConfig,
    http: OnceLock<reqwest::blocking::Client>,
}

impl HttpsModelClient {
    pub fn new(config: ModelConfig) -> HttpsModelClient {
        HttpsModelClient {
            config,
            http: OnceLock::new(),
        }
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
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
    fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::blocking::Response, String> {
        let key = discover_key(&self.config.key_file).map_err(|r| r.says())?;
        let response = self
            .http()?
            .post(format!("{}{path}", self.config.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .map_err(|e| format!("the model call failed: {}", scrub(&e.to_string())))?;
        let status = response.status();
        if !status.is_success() {
            let said = response.text().unwrap_or_default();
            return Err(format!(
                "the model answered {}: {}",
                status.as_u16(),
                said.trim().chars().take(600).collect::<String>()
            ));
        }
        Ok(response)
    }
}

impl ModelClient for HttpsModelClient {
    fn count_tokens(&self, request: &ModelRequest) -> Result<u64, String> {
        let response = self.post("/v1/messages/count_tokens", &request.count_body())?;
        let value: serde_json::Value = response
            .json()
            .map_err(|e| format!("the token count did not decode: {e}"))?;
        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("the token count carried no `input_tokens`: {value}"))
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String> {
        let response = self.post("/v1/messages", &request.body(true))?;
        let mut outcome = ModelOutcome::default();
        fold_stream(BufReader::new(response), &mut outcome, on_event)?;
        Ok(outcome)
    }
}

/// Fold an SSE body into events and an outcome. Every line that is not a
/// `data:` payload — the `event:` names, the blank separators, a comment — is
/// skipped: the payload's own `type` is what dispatches.
pub fn fold_stream(
    body: impl BufRead,
    outcome: &mut ModelOutcome,
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
        fold_event(payload, outcome, on_event)?;
    }
    Ok(())
}

/// Fold ONE SSE payload. PURE, so the whole event vocabulary is asserted over a
/// scripted transcript with no network.
pub fn fold_event(
    payload: &str,
    outcome: &mut ModelOutcome,
    on_event: &mut dyn FnMut(ModelEvent),
) -> Result<(), String> {
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
        "content_block_delta" => {
            let delta = value.get("delta");
            let kind = delta
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // `text_delta` only. A `thinking_delta` is reasoning and never
            // reaches a log record; a `input_json_delta` belongs to a tool this
            // verb does not declare.
            if kind == "text_delta" {
                if let Some(text) = delta.and_then(|d| d.get("text")).and_then(|v| v.as_str()) {
                    on_event(ModelEvent::Text(text.to_string()));
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
        fn count_tokens(&self, request: &ModelRequest) -> Result<u64, String> {
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
            let mut outcome = ModelOutcome::default();
            fold_stream(self.transcript.as_bytes(), &mut outcome, on_event)?;
            Ok(outcome)
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
        let mut text: Vec<String> = Vec::new();
        let mut outcome = ModelOutcome::default();
        fold_stream(transcript.as_bytes(), &mut outcome, &mut |event| {
            let ModelEvent::Text(chunk) = event;
            text.push(chunk);
        })
        .expect("the transcript folded");
        (text, outcome)
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
    fn a_stream_error_and_a_silent_end_are_both_said() {
        let mut outcome = ModelOutcome::default();
        let e = fold_stream(STREAM_ERROR.as_bytes(), &mut outcome, &mut |_| {}).unwrap_err();
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
