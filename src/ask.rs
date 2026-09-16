//! The ask-context envelope the `explain` verb consults, and the refusals it
//! answers with (GyldAskAgent.md sections 3 and 4).
//!
//! `gyld.ask-context.v1` is composed in the page by `askEnvelope()` — a pure
//! function over grip values — and rides `args.context` WHOLE. It is a typed
//! object like every other `GyldArgs` field, so nothing a requester writes
//! reaches a command line, and in this verb's case nothing reaches a subprocess
//! at all.
//!
//! Everything here is PURE. [`AskContext::parse`] reads bytes the request
//! carried; [`AgentState`] is the supplier's already-taken reading of the world
//! (a key was found, the build emitted an index, these are the streams it
//! lists), handed to the planner as DATA so every refusal — including the two
//! that are about the world — is produced with no filesystem effect at all.
//!
//! A key VALUE never appears here. [`AgentState::key`] is a boolean and
//! [`AgentState::key_file`] is only ever the path a refusal names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The envelope format this verb accepts, and the only one.
pub const ASK_CONTEXT_FORMAT: &str = "gyld.ask-context.v1";

/// The source index a build emits beside `streams.json` (`gyld.sources.v1`).
pub const SOURCES_FILE: &str = "sources.json";

/// The Gyld host flag that points the index at the cited documents, named in
/// the no-source-index refusal so a reader is told how to get one.
pub const SOURCES_ROOT_OPTION: &str = "--sources-root";

/// The environment variable the supplier's model key is read from (section 7).
/// The page holds no credential: the key lives in the SUPPLIER's environment
/// and never reaches a share, a bundle, a log record or a browser.
pub const KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The key file, relative to the app-owned bundle root, read when the
/// environment carries none. `--agent-key-file` names another.
pub const DEFAULT_KEY_FILE: &str = "agent/api-key";

/// The largest envelope the supplier decodes. The envelope carries pointers,
/// not bytes (section 3), so a hand-sized bound is the honest one.
pub const MAX_CONTEXT_BYTES: usize = 256 * 1024;

/// The largest question the supplier passes to a model in one turn.
pub const MAX_QUESTION_BYTES: usize = 8 * 1024;

/// The longest conversation id accepted as a log key.
const MAX_CONVERSATION_ID: usize = 200;

/// The record the reader asked about, as this stream drew and declared it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskRecord {
    pub slot: String,
    #[serde(default)]
    pub label: String,
    /// The question's own text, as the host DREW it inside the box.
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub definition: String,
    #[serde(default)]
    pub description: String,
}

/// The emitted status block. `emitted` and `listed` are the two absences the
/// envelope SAYS rather than fills in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskStatus {
    #[serde(default)]
    pub emitted: bool,
    #[serde(default)]
    pub listed: bool,
    #[serde(default)]
    pub declared: String,
    #[serde(default)]
    pub effective: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub answerable_now: bool,
    /// The ONE emitted reason it is not answerable now.
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskAlternative {
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preferred: bool,
}

/// The ruling that decided this question, as the chain emitted it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskRuling {
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub principal: String,
    #[serde(default)]
    pub stamp: String,
    #[serde(default)]
    pub live: bool,
}

/// One source tag this record or its ruling cites, as the PAGE carried it.
///
/// The page's own resolution is read for the tag and where it says it came
/// from, and for nothing else: the supplier resolves every tag itself against
/// the build's index (section 5, "How the supplier resolves"), so a passage in
/// a prompt is always one the index emitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskSource {
    pub tag: String,
    /// `"ruling"` or `"record"` — which citation list the tag came from.
    #[serde(default)]
    pub cites: String,
}

/// The neighbourhood lens, as a POINTER. Geometry never rides the envelope.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskNeighbourhood {
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub perspective: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub member: bool,
}

/// The whole envelope, `gyld.ask-context.v1`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskContext {
    pub format: String,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub perspective: String,
    /// The identity the decide-now list was built from, carried whole and
    /// never interpreted: it is that file's own field.
    #[serde(default)]
    pub snapshot: Option<serde_json::Value>,
    #[serde(default)]
    pub record: AskRecord,
    #[serde(default)]
    pub status: AskStatus,
    #[serde(default)]
    pub alternatives: Vec<AskAlternative>,
    #[serde(default)]
    pub lean: Option<String>,
    #[serde(default)]
    pub ruling: Option<AskRuling>,
    #[serde(default)]
    pub sources: Vec<AskSource>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub unlocks: Vec<String>,
    #[serde(default)]
    pub gates: Vec<String>,
    #[serde(default)]
    pub neighbourhood: Option<AskNeighbourhood>,
    #[serde(default)]
    pub principal: String,
    #[serde(default)]
    pub conversation: String,
    #[serde(default)]
    pub question: String,
}

impl AskContext {
    /// Decode and check one `args.context`. A missing field, a wrong format, a
    /// stream id that is not one, a conversation id that could not be a log key
    /// and an empty question are each a readable refusal rather than a guess.
    pub fn parse(value: Option<&serde_json::Value>) -> Result<AskContext, String> {
        let value = value.ok_or("`context` (the ask envelope) is required")?;
        let bytes = serde_json::to_vec(value)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if bytes > MAX_CONTEXT_BYTES {
            return Err(format!(
                "the ask envelope is {bytes} bytes; the limit is {MAX_CONTEXT_BYTES}"
            ));
        }
        let context: AskContext =
            serde_json::from_value(value.clone()).map_err(|e| format!("bad ask envelope: {e}"))?;
        if context.format != ASK_CONTEXT_FORMAT {
            return Err(format!(
                "the ask envelope says {:?}; this verb reads {ASK_CONTEXT_FORMAT}",
                context.format
            ));
        }
        if !crate::verbs::valid_stream_id(&context.stream) {
            return Err(format!(
                "the ask envelope's `stream` value {:?} is not a stream id",
                context.stream
            ));
        }
        if context.record.slot.trim().is_empty() {
            return Err("the ask envelope names no `record.slot`".into());
        }
        if !valid_conversation_id(&context.conversation) {
            return Err(format!(
                "the ask envelope's `conversation` value {:?} is not a conversation id",
                context.conversation
            ));
        }
        if context.question.trim().is_empty() {
            return Err("the ask envelope carries no `question`".into());
        }
        if context.question.len() > MAX_QUESTION_BYTES {
            return Err(format!(
                "the question is {} bytes; the limit is {MAX_QUESTION_BYTES}",
                context.question.len()
            ));
        }
        Ok(context)
    }

    /// Every tag this record and its ruling cite, in the order the envelope
    /// gave them, with nothing repeated. This is the whole of what the supplier
    /// resolves: no search, no walking, no retrieval of any other kind.
    pub fn tags(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for source in self.sources.iter() {
            let tag = source.tag.trim();
            if !tag.is_empty() && !out.iter().any(|held| held == tag) {
                out.push(tag.to_string());
            }
        }
        out
    }
}

/// A conversation id, as section 6 mints it: `conv-<tabId>-<slot>-<stamp>`.
///
/// It becomes a LOG KEY, so it is checked the way a stream id is: bounded, and
/// made only of characters that key a surface and read back in a log — no
/// control character, no space, no quote.
pub fn valid_conversation_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_CONVERSATION_ID {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '+' | '~'))
}

/// What the supplier already found out about the agent's readiness, handed to
/// the PURE planner as DATA.
///
/// The planner reads no environment variable and stats no file, so the two
/// refusals that are about the WORLD — no model key, no source index — are
/// produced with no filesystem effect, exactly like the three that are about
/// the request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentState {
    /// A model key was discovered. Never the key itself: no key value reaches a
    /// plan, a prompt, a record, a log line or an argv.
    pub key: bool,
    /// Where a key file would be read from, for the refusal's own text.
    pub key_file: PathBuf,
    /// The latest build emitted a source index beside its stream listing.
    pub index: bool,
    /// The streams that build LISTS. A stream the build does not list is a
    /// stream this verb cannot answer about.
    pub streams: Vec<String>,
}

impl AgentState {
    /// The readiness of an agent with a key and an index over `streams` — the
    /// ordinary case, and the one the tests say the happy path from.
    pub fn ready(streams: &[&str]) -> AgentState {
        AgentState {
            key: true,
            key_file: PathBuf::new(),
            index: true,
            streams: streams.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// The refusals of section 4, as DATA.
///
/// Each one is a value with the numbers and paths that make it actionable, and
/// [`AskRefusal::says`] is the one place their wording lives — the supplier
/// answers with it synchronously and the log surface carries the same sentence.
#[derive(Debug, Clone, PartialEq)]
pub enum AskRefusal {
    /// Nothing started: no key was found in the environment or on disk.
    NoModelKey { key_file: PathBuf },
    /// Grounding was ruled in from day one: a build with no index is refused
    /// with the file named and the flag that emits it.
    NoSourceIndex { path: PathBuf },
    /// The envelope did not decode, or is not this format.
    BadEnvelope { reason: String },
    /// The envelope names a stream this build does not list.
    StreamNotListed { stream: String, listed: Vec<String> },
}

impl AskRefusal {
    pub fn says(&self) -> String {
        match self {
            AskRefusal::NoModelKey { key_file } => {
                format!(
                    "no model key: set ANTHROPIC_API_KEY in the supplier's environment, or write \
                     {}",
                    key_file.display()
                )
            }
            AskRefusal::NoSourceIndex { path } => {
                format!(
                    "this build emitted no source index: {} is absent, so nothing can be cited. \
                     Rebuild with emit_decision_streams.py {SOURCES_ROOT_OPTION} <the cited \
                     workzone>",
                    path.display()
                )
            }
            AskRefusal::BadEnvelope { reason } => reason.clone(),
            AskRefusal::StreamNotListed { stream, listed } => {
                format!("the ask envelope names stream {stream:?}; this build lists {listed:?}")
            }
        }
    }
}

impl std::fmt::Display for AskRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.says())
    }
}

/// What an `explain` plan carries instead of an argv: the validated envelope,
/// the index it will be resolved against, and the conversation the reply is
/// keyed by.
///
/// There is deliberately no command, no interpreter and no output directory in
/// here. "The agent never writes an overlay" is a property of this type.
#[derive(Debug, Clone, PartialEq)]
pub struct Consultation {
    pub context: AskContext,
    /// The `sources.json` of the build `latest.json` names.
    pub sources: PathBuf,
    /// The log key every reply record takes (section 6).
    pub conversation: String,
}

impl Consultation {
    /// Validate an envelope against the world the supplier read, and resolve
    /// the consultation — or produce the refusal that stands in its place.
    ///
    /// The order is the order of cost: the request's own shape first, then the
    /// key (nothing started), then the index, then the stream the envelope
    /// names. PURE throughout.
    pub fn resolve(
        bundle: &Path,
        context: Option<&serde_json::Value>,
        agent: &AgentState,
    ) -> Result<Consultation, AskRefusal> {
        let context =
            AskContext::parse(context).map_err(|reason| AskRefusal::BadEnvelope { reason })?;
        if !agent.key {
            return Err(AskRefusal::NoModelKey {
                key_file: agent.key_file.clone(),
            });
        }
        let sources = bundle.join(SOURCES_FILE);
        if !agent.index {
            return Err(AskRefusal::NoSourceIndex { path: sources });
        }
        if !agent.streams.iter().any(|held| held == &context.stream) {
            return Err(AskRefusal::StreamNotListed {
                stream: context.stream.clone(),
                listed: agent.streams.clone(),
            });
        }
        let conversation = context.conversation.clone();
        Ok(Consultation {
            context,
            sources,
            conversation,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A well-formed envelope, as `askEnvelope()` composes one.
    pub(crate) fn envelope() -> serde_json::Value {
        serde_json::json!({
            "format": ASK_CONTEXT_FORMAT,
            "stream": "base",
            "perspective": "decisions",
            "snapshot": {"digest": "abc"},
            "record": {
                "slot": "glade_decisions:GladeDecisions.key_custody",
                "label": "key_custody",
                "lines": ["Key custody and recovery posture"],
                "kind": "question",
                "definition": "Question",
                "description": "who holds the keys"
            },
            "status": {
                "emitted": true, "listed": true, "declared": "open",
                "effective": "blocked", "tier": "now", "answerable_now": false,
                "reason": "waits on proof_family"
            },
            "alternatives": [
                {"slot": "a1", "label": "device", "description": "on device", "preferred": true}
            ],
            "lean": "a1",
            "ruling": null,
            "sources": [
                {"tag": "Q11", "cites": "record"},
                {"tag": "AZ-7", "cites": "record"},
                {"tag": "Q11", "cites": "ruling"}
            ],
            "requires": ["glade_decisions:GladeDecisions.proof_family"],
            "unlocks": [],
            "gates": [],
            "neighbourhood": {
                "stream": "base", "perspective": "decisions",
                "path": "streams/base/lenses/decisions.lens.json", "member": false
            },
            "principal": "gianni",
            "conversation": "conv-tab1-key_custody-1789",
            "question": "why is this blocked?"
        })
    }

    fn with(field: &str, value: serde_json::Value) -> serde_json::Value {
        let mut e = envelope();
        e[field] = value;
        e
    }

    #[test]
    fn a_well_formed_envelope_decodes_whole() {
        let c = AskContext::parse(Some(&envelope())).expect("envelope");
        assert_eq!(c.stream, "base");
        assert_eq!(c.record.label, "key_custody");
        assert_eq!(c.status.reason, "waits on proof_family");
        assert_eq!(c.alternatives.len(), 1);
        assert!(c.alternatives[0].preferred);
        assert_eq!(c.lean.as_deref(), Some("a1"));
        assert!(c.ruling.is_none());
        assert_eq!(c.conversation, "conv-tab1-key_custody-1789");
        assert_eq!(c.question, "why is this blocked?");
        assert_eq!(
            c.neighbourhood.as_ref().unwrap().path,
            "streams/base/lenses/decisions.lens.json"
        );
    }

    #[test]
    fn the_tags_are_the_cited_ones_in_order_with_nothing_repeated() {
        let c = AskContext::parse(Some(&envelope())).unwrap();
        assert_eq!(c.tags(), vec!["Q11", "AZ-7"]);
    }

    #[test]
    fn a_bad_envelope_is_a_readable_refusal() {
        let e = AskContext::parse(None).unwrap_err();
        assert!(e.contains("`context`"), "{e}");

        let e = AskContext::parse(Some(&serde_json::json!("not an object"))).unwrap_err();
        assert!(e.contains("bad ask envelope"), "{e}");

        let e = AskContext::parse(Some(&with("format", "gyld.ask-context.v2".into()))).unwrap_err();
        assert!(e.contains("this verb reads gyld.ask-context.v1"), "{e}");

        let e = AskContext::parse(Some(&with("stream", "../etc".into()))).unwrap_err();
        assert!(e.contains("not a stream id"), "{e}");

        let e =
            AskContext::parse(Some(&with("record", serde_json::json!({"slot": " "})))).unwrap_err();
        assert!(e.contains("names no `record.slot`"), "{e}");

        let e = AskContext::parse(Some(&with("conversation", "conv one".into()))).unwrap_err();
        assert!(e.contains("not a conversation id"), "{e}");

        let e = AskContext::parse(Some(&with("question", "   ".into()))).unwrap_err();
        assert!(e.contains("carries no `question`"), "{e}");
    }

    #[test]
    fn an_oversize_envelope_and_an_oversize_question_are_both_refused() {
        let big = "x".repeat(MAX_QUESTION_BYTES + 1);
        let e = AskContext::parse(Some(&with("question", big.into()))).unwrap_err();
        assert!(e.contains("the limit is"), "{e}");

        let huge = "y".repeat(MAX_CONTEXT_BYTES + 1);
        let e = AskContext::parse(Some(&with("perspective", huge.into()))).unwrap_err();
        assert!(e.contains("the ask envelope is"), "{e}");
    }

    #[test]
    fn conversation_ids_are_log_keys_and_are_checked_like_one() {
        for id in [
            "conv-tab1-glade_decisions:GladeDecisions.key_custody-1789363954989",
            "conv-tab-a-1",
        ] {
            assert!(valid_conversation_id(id), "{id}");
        }
        for id in [
            "",
            "conv one",
            "conv\n1",
            "conv/1",
            "conv\"1",
            &"c".repeat(MAX_CONVERSATION_ID + 1),
        ] {
            assert!(!valid_conversation_id(id), "{id:?} must be refused");
        }
    }

    #[test]
    fn the_four_refusals_say_what_to_do_about_them() {
        let bundle = Path::new("/b/builds/build-1");
        let ready = AgentState::ready(&["base", "stream-a"]);

        // 1. the envelope did not decode.
        let bad = Consultation::resolve(bundle, None, &ready).unwrap_err();
        assert!(matches!(bad, AskRefusal::BadEnvelope { .. }), "{bad:?}");
        assert!(bad.says().contains("`context`"), "{bad}");

        // 2. no model key — before anything else about the world.
        let no_key = AgentState {
            key: false,
            key_file: PathBuf::from("/b/agent/api-key"),
            ..AgentState::ready(&["base"])
        };
        let e = Consultation::resolve(bundle, Some(&envelope()), &no_key).unwrap_err();
        assert_eq!(
            e,
            AskRefusal::NoModelKey {
                key_file: PathBuf::from("/b/agent/api-key")
            }
        );
        assert!(e.says().contains("ANTHROPIC_API_KEY"), "{e}");
        assert!(e.says().contains("/b/agent/api-key"), "{e}");

        // 3. no source index — named, with the flag that emits one.
        let no_index = AgentState {
            index: false,
            ..AgentState::ready(&["base"])
        };
        let e = Consultation::resolve(bundle, Some(&envelope()), &no_index).unwrap_err();
        assert!(e.says().contains("build-1/sources.json"), "{e}");
        assert!(e.says().contains("--sources-root"), "{e}");

        // 4. a stream the build does not list.
        let elsewhere = AgentState::ready(&["stream-a"]);
        let e = Consultation::resolve(bundle, Some(&envelope()), &elsewhere).unwrap_err();
        assert!(e.says().contains("\"base\""), "{e}");
        assert!(e.says().contains("stream-a"), "{e}");
    }

    #[test]
    fn a_resolved_consultation_names_the_index_and_the_conversation() {
        let c = Consultation::resolve(
            Path::new("/b/builds/build-1"),
            Some(&envelope()),
            &AgentState::ready(&["base"]),
        )
        .expect("a consultation");
        assert_eq!(c.sources, PathBuf::from("/b/builds/build-1/sources.json"));
        assert_eq!(c.conversation, "conv-tab1-key_custody-1789");
        assert_eq!(c.context.record.label, "key_custody");
    }
}
