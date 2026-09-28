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
//! **The key never travels.** It is read from the environment the supplier
//! started with, or from an app-owned file, at the moment of the call, put in
//! one header, and dropped. It is in no plan, no prompt, no record, no log line
//! and no [`Debug`] output: [`ModelConfig`] carries the key FILE's path and
//! never a key.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::agent::Compat;
use crate::ask::{AskRefusal, KEY_ENV};
use crate::conversation::{self, Turn};
use crate::environment::Environment;
use crate::prompt::Prompt;
use crate::tools::{ToolPolicy, ToolRegistry};

mod client;
mod config;
mod credentials;
mod https;
mod outcome;
mod request;
mod sse;

pub use client::*;
pub use config::*;
pub use credentials::*;
pub use https::*;
pub use outcome::*;
pub use request::*;
pub use sse::*;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    mod client;
    mod credentials;
    mod outcome;
    mod request;
    mod tool_loop;

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
}
