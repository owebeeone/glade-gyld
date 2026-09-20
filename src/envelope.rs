//! The request / response / output-record envelopes (GyldGrythPlugins.md 4.7).
//! Small JSON, the honest smallest surface, modelled line for line on
//! `glade-gwz`:
//!
//! * [`GyldRequest`] rides the `ExchangeReq` payload — `{verb, args, stream?,
//!   principal?}`. `args` is a TYPED object, never an argv list: nothing a
//!   requester writes reaches a command line as a flag.
//! * [`GyldResponse`] rides the `ExchangeRes` payload — `{ok, run_id,
//!   output_dir?, exit, stdout, stderr, error?, done?, attributed_to}`.
//! * [`GyldOutputRecord`] rides each LOG op appended to the output surface for
//!   a streaming run — `{run_id, seq, principal?, stream, line?, done?, exit?}`,
//!   the `gwz.output` record shape exactly.
//! * [`GyldAskRecord`] rides each LOG op appended to the ASK surface for a
//!   consultation — the same field set plus two: the `conversation` it belongs
//!   to, which is the surface's KEY, and the `record` a citation carries. A
//!   consumer that has never heard of `citation` shows nothing for it, which is
//!   the rule the plugin already follows: absent records are absent lines,
//!   never blank ones.
//!
//! Failure is DATA: a bad envelope, a refused verb, a stream id that is not a
//! stream id, a path that leaves the bundle root, a spawn error, a timeout, or
//! a non-zero exit all resolve to a well-formed `GyldResponse{ok:false}`. The
//! WIRE `ExchangeRes.ok` stays `true` (the exchange always produced a
//! structured answer; the PAYLOAD `ok` carries the run's success).

use serde::{Deserialize, Serialize};

/// The typed argument object. Every field is optional here and validated per
/// verb in [`crate::verbs::plan`], so a missing or surplus field is a refusal
/// with a readable reason rather than a panic or a surprise argv.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GyldArgs {
    /// The stream a verb acts on (`answer`, `ask`, and the new id of a
    /// `fork` / `link`).
    #[serde(default)]
    pub stream: Option<String>,
    /// The parent stream a `fork` or `link` is taken from.
    #[serde(default)]
    pub parent: Option<String>,
    /// `diff`: the left-hand stream.
    #[serde(default)]
    pub left: Option<String>,
    /// `diff`: the right-hand stream.
    #[serde(default)]
    pub right: Option<String>,
    /// `answer` / `ask`: the overlay module text the decide window exported.
    /// Written verbatim as the stream's overlay module; the supplier never
    /// composes Gyld source itself.
    #[serde(default)]
    pub overlay: Option<String>,
    /// `ask`: the added question's module fragment, appended to `overlay`.
    #[serde(default)]
    pub question: Option<String>,
    /// One line of provenance recorded on a generated stream record.
    #[serde(default)]
    pub note: Option<String>,
    /// `rebuild`: an explicit ISO-8601 build stamp (the host defaults it to now).
    #[serde(default)]
    pub built: Option<String>,
    /// `explain`: the ask-context envelope (`gyld.ask-context.v1`), WHOLE.
    ///
    /// Held as raw JSON rather than as a typed field so a malformed envelope is
    /// the `explain` verb's own readable refusal — naming the field and what is
    /// wrong with it — instead of a flat `bad envelope` on every request that
    /// happens to carry a `context`.
    #[serde(default)]
    pub context: Option<serde_json::Value>,
    /// Overwrite an existing overlay module or diff document.
    #[serde(default)]
    pub force: bool,
}

/// A verb request over the gyld exchange surface.
///
/// `stream` (alias `stream_output`, the name section 4.7 uses) opts the run onto
/// the log surface. `principal` attributes the run and falls back to the
/// supplier's configured principal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GyldRequest {
    pub verb: String,
    #[serde(default)]
    pub args: GyldArgs,
    #[serde(default, alias = "stream_output")]
    pub stream: bool,
    #[serde(default)]
    pub principal: Option<String>,
}

impl GyldRequest {
    /// Parse an envelope from the exchange payload bytes.
    pub fn parse(payload: &[u8]) -> Result<GyldRequest, String> {
        serde_json::from_slice(payload).map_err(|e| format!("bad envelope: {e}"))
    }
}

/// A verb answer, carried on the `ExchangeRes` payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GyldResponse {
    /// The verb succeeded (exit 0), OR a streaming run was accepted.
    pub ok: bool,
    /// Every answer carries a run id: it keys the log surface for a streaming
    /// run and stamps a synchronous one for the audit trail.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_id: Option<String>,
    /// The NEW bundle directory a mutating verb built, when it built one.
    /// Absolute; nothing is ever built over an existing directory.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// Streaming: `false` on the accept answer; the `done:true` marker lands on
    /// the log surface, not here.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub done: Option<bool>,
    /// The principal the run was attributed to (attribution as data).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attributed_to: Option<String>,
    /// The overlay module FILE this verb left behind — the NOTEBOOK, in the
    /// decisions root when the app configured one. Absolute.
    ///
    /// It sits BESIDE the Gyld host's own JSON in `stdout`, which names the
    /// staging path the host wrote: the host's answer is not rewritten, and this
    /// says where the file the owner commits actually is. A verb that left no
    /// file, and a verb whose file is not there yet, name none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub overlay_file: Option<String>,
}

impl GyldResponse {
    /// A completed synchronous run: `ok` reflects a clean exit.
    pub fn ran(
        run_id: String,
        exit: i32,
        stdout: String,
        stderr: String,
        output_dir: Option<String>,
        who: Option<String>,
    ) -> GyldResponse {
        GyldResponse {
            ok: exit == 0,
            run_id: Some(run_id),
            output_dir: if exit == 0 { output_dir } else { None },
            exit: Some(exit),
            stdout,
            stderr,
            attributed_to: who,
            ..Default::default()
        }
    }

    /// Failure as data: a bad envelope, a refused verb, a containment refusal,
    /// a spawn error, or a timeout.
    pub fn failed(error: impl Into<String>, who: Option<String>) -> GyldResponse {
        GyldResponse {
            ok: false,
            error: Some(error.into()),
            attributed_to: who,
            ..Default::default()
        }
    }

    /// A streaming run was accepted; output flows to the log surface under
    /// `run_id` and the result lands on the value surfaces.
    pub fn accepted(run_id: String, who: Option<String>) -> GyldResponse {
        GyldResponse {
            ok: true,
            run_id: Some(run_id),
            done: Some(false),
            attributed_to: who,
            ..Default::default()
        }
    }

    /// The same answer, naming the notebook the verb left behind. A builder
    /// rather than a seventh parameter on [`GyldResponse::ran`]: the four
    /// writing verbs are the only ones with anything to say here.
    pub fn leaving(mut self, overlay_file: Option<String>) -> GyldResponse {
        self.overlay_file = overlay_file;
        self
    }

    /// Serialize for the exchange payload. Never panics: a serialize failure of
    /// these plain structs is not reachable, and the fallback stays data.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"serialize failed\"}".to_vec())
    }
}

/// One appended record on the output log surface for a streaming run — the
/// `GwzOutputRecord` shape, field for field, so one consumer folds both.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GyldOutputRecord {
    pub run_id: String,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub principal: Option<String>,
    /// `"stdout" | "stderr" | "end"`.
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit: Option<i32>,
}

impl GyldOutputRecord {
    pub fn line(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        stream: &str,
        line: String,
    ) -> GyldOutputRecord {
        GyldOutputRecord {
            run_id: run_id.into(),
            seq,
            principal: who.clone(),
            stream: stream.into(),
            line: Some(line),
            ..Default::default()
        }
    }

    pub fn end(run_id: &str, seq: u64, who: &Option<String>, exit: i32) -> GyldOutputRecord {
        GyldOutputRecord {
            run_id: run_id.into(),
            seq,
            principal: who.clone(),
            stream: "end".into(),
            done: Some(true),
            exit: Some(exit),
            ..Default::default()
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// One appended record on the ASK surface for a consultation
/// (GyldAskAgent.md section 4, "The reply").
///
/// The [`GyldOutputRecord`] field set, plus `conversation` and `record`. Keyed
/// by the CONVERSATION and not by the run: that is what makes a conversation
/// one fold, one mount and one key, while each turn keeps its own `run_id` on
/// every record for the audit trail and closes with its own `end`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GyldAskRecord {
    pub run_id: String,
    pub seq: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub principal: Option<String>,
    /// The conversation this turn belongs to, and the surface's key.
    pub conversation: String,
    /// `"question" | "answer" | "citation" | "note" | "draft" | "end"`.
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<String>,
    /// A `citation`'s resolved source, or a `draft`'s offer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub record: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub done: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit: Option<i32>,
}

/// The record streams a consultation emits. `draft` is phase 3; `tool_call`
/// and `tool_result` are phase A.
///
/// `question` is the reader's own turn, appended before anything the model
/// produces. Section 6 says the transcript IS the log share; without the
/// question on it the surface carries answers to questions nobody kept, and a
/// follow-up could not replay the conversation from the records the supplier
/// wrote. A consumer that has never heard of it shows nothing for it, which is
/// the rule this surface already follows.
pub const ASK_QUESTION: &str = "question";
pub const ASK_ANSWER: &str = "answer";
pub const ASK_CITATION: &str = "citation";
/// The offer a turn made (section 8). Its `record` is a
/// [`crate::ask::AskDraft`], `drafted_by` and all.
pub const ASK_DRAFT: &str = "draft";
/// Something the CALL had to do differently, said beside the answer it
/// weakened: an input budget that is an estimate because the endpoint cannot
/// count, a `strict` the endpoint rejected, a cache breakpoint it would not
/// take, a config file that did not decode.
///
/// A sixth record stream, and on the same rule as `question`: a consumer that
/// has never heard of it draws nothing for it, and one that has draws the line
/// it carries. The point is that a fallback is never SILENT — a reader looking
/// at a weaker answer can see what weakened it, in the same place as the
/// answer, rather than in a terminal log nobody is reading.
pub const ASK_NOTE: &str = "note";

/// A tool the agent reached for, as the loop is about to run it
/// (GyldAskAgent.md 11.4). Its `record` is `{id, name, input}`, where `id` is
/// the API's own `tool_use_id` — so a result is paired with its call by
/// identity and never by position, which matters because one turn may call two
/// tools at once.
pub const ASK_TOOL_CALL: &str = "tool_call";

/// What that call answered with: `{id, name, ok, summary, bytes, truncated}`.
/// A tool that refused is `ok: false` with the reason in `summary`, and a
/// result the byte budget cut says so in `truncated` with the size it was
/// before the cut in `bytes`.
pub const ASK_TOOL_RESULT: &str = "tool_result";

pub const ASK_END: &str = "end";

impl GyldAskRecord {
    fn of(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        stream: &str,
    ) -> GyldAskRecord {
        GyldAskRecord {
            run_id: run_id.into(),
            seq,
            principal: who.clone(),
            conversation: conversation.into(),
            stream: stream.into(),
            ..Default::default()
        }
    }

    /// The reader's question, as they typed it and before anything is asked of
    /// a model. It opens the turn, so the fold reads a turn even when the call
    /// then fails outright.
    pub fn question(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        question: String,
    ) -> GyldAskRecord {
        GyldAskRecord {
            line: Some(question),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_QUESTION)
        }
    }

    /// One chunk of the answer, as the model streamed it.
    pub fn answer(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        chunk: String,
    ) -> GyldAskRecord {
        GyldAskRecord {
            line: Some(chunk),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_ANSWER)
        }
    }

    /// One cited source, as the build's index resolved it — or as it failed to.
    /// The `record` is the index's own entry, never the model's rendering of it.
    pub fn citation(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        source: serde_json::Value,
    ) -> GyldAskRecord {
        GyldAskRecord {
            record: Some(source),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_CITATION)
        }
    }

    /// One thing the call had to do differently, as one line. It is appended
    /// as it happens — before the answer it weakened, because that is when it
    /// is discovered.
    pub fn note(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        note: String,
    ) -> GyldAskRecord {
        GyldAskRecord {
            line: Some(note),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_NOTE)
        }
    }

    /// The draft this turn proposed: an OFFER, carrying the model id that made
    /// it, so it can never be mistaken for a person's text. It is not a ruling
    /// and nothing here makes it one — a human takes it, edits it or discards
    /// it, and the decide window's own refusals are untouched.
    pub fn draft(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        draft: serde_json::Value,
    ) -> GyldAskRecord {
        GyldAskRecord {
            record: Some(draft),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_DRAFT)
        }
    }

    /// One tool the agent reached for, before it is run. It is appended BEFORE
    /// the call so a reader watching a turn sees what it is waiting on.
    pub fn tool_call(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        call: serde_json::Value,
    ) -> GyldAskRecord {
        GyldAskRecord {
            record: Some(call),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_TOOL_CALL)
        }
    }

    /// What one tool call answered with — including a refusal, which is data
    /// here like everywhere else.
    pub fn tool_result(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        answered: serde_json::Value,
    ) -> GyldAskRecord {
        GyldAskRecord {
            record: Some(answered),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_TOOL_RESULT)
        }
    }

    /// The turn's close: `done`, the exit, and — on anything but a clean end —
    /// the one line saying why.
    pub fn end(
        run_id: &str,
        seq: u64,
        who: &Option<String>,
        conversation: &str,
        exit: i32,
        said: Option<String>,
    ) -> GyldAskRecord {
        GyldAskRecord {
            line: said,
            done: Some(true),
            exit: Some(exit),
            ..GyldAskRecord::of(run_id, seq, who, conversation, ASK_END)
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parses_minimal_and_full() {
        let m = GyldRequest::parse(br#"{"verb":"list"}"#).unwrap();
        assert_eq!(m.verb, "list");
        assert!(!m.stream && m.principal.is_none() && m.args.stream.is_none());

        let f = GyldRequest::parse(
            br#"{"verb":"fork","args":{"parent":"base","stream":"keys-a","note":"n","force":true},
                 "stream":true,"principal":"gianni"}"#,
        )
        .unwrap();
        assert_eq!(f.verb, "fork");
        assert_eq!(f.args.parent.as_deref(), Some("base"));
        assert_eq!(f.args.stream.as_deref(), Some("keys-a"));
        assert_eq!(f.args.note.as_deref(), Some("n"));
        assert!(f.args.force && f.stream);
        assert_eq!(f.principal.as_deref(), Some("gianni"));
    }

    #[test]
    fn stream_output_is_accepted_as_the_streaming_flag() {
        let r = GyldRequest::parse(br#"{"verb":"rebuild","stream_output":true}"#).unwrap();
        assert!(r.stream, "section 4.7 spells the flag `stream_output`");
    }

    #[test]
    fn bad_envelope_is_an_error_not_a_panic() {
        let e = GyldRequest::parse(b"not json").unwrap_err();
        assert!(e.contains("bad envelope"), "{e}");
        let e = GyldRequest::parse(br#"{"verb":42}"#).unwrap_err();
        assert!(e.contains("bad envelope"), "{e}");
    }

    #[test]
    fn response_ok_reflects_exit_and_skips_none_fields() {
        let ok = GyldResponse::ran(
            "run-1".into(),
            0,
            "hi".into(),
            String::new(),
            Some("/b/builds/x".into()),
            Some("gianni".into()),
        );
        assert!(ok.ok && ok.exit == Some(0));
        assert_eq!(ok.output_dir.as_deref(), Some("/b/builds/x"));

        // A failed run never advertises an output directory: nothing was built.
        let bad = GyldResponse::ran(
            "run-2".into(),
            1,
            String::new(),
            "boom".into(),
            Some("/b/builds/y".into()),
            None,
        );
        assert!(!bad.ok && bad.exit == Some(1) && bad.output_dir.is_none());

        let s = String::from_utf8(ok.to_bytes()).unwrap();
        assert!(!s.contains("\"error\"") && !s.contains("\"done\""), "{s}");
        assert!(s.contains("\"attributed_to\":\"gianni\""), "{s}");
    }

    #[test]
    fn accepted_carries_run_id_and_done_false() {
        let a = GyldResponse::accepted("run-1".into(), Some("p".into()));
        assert!(a.ok && a.run_id.as_deref() == Some("run-1") && a.done == Some(false));
    }

    #[test]
    fn ask_records_are_the_gwz_shape_plus_the_conversation() {
        let who = Some("gianni".into());
        let a = GyldAskRecord::answer("run-3", 1, &who, "conv-1", "It is blocked ".into());
        let s = String::from_utf8(a.to_bytes()).unwrap();
        for field in [
            "\"run_id\":\"run-3\"",
            "\"seq\":1",
            "\"principal\":\"gianni\"",
            "\"conversation\":\"conv-1\"",
            "\"stream\":\"answer\"",
            "\"line\":\"It is blocked \"",
        ] {
            assert!(s.contains(field), "{field} missing from {s}");
        }
        assert!(!s.contains("\"record\"") && !s.contains("\"done\""), "{s}");

        let c = GyldAskRecord::citation(
            "run-3",
            2,
            &who,
            "conv-1",
            serde_json::json!({"tag": "AZ-7", "resolved": false, "reason": "nothing declares it"}),
        );
        assert_eq!(c.stream, ASK_CITATION);
        assert_eq!(c.record.as_ref().unwrap()["tag"], "AZ-7");
        assert!(c.line.is_none(), "a citation is a record, not a line");

        // A clean end says nothing beyond how it ended.
        let e = GyldAskRecord::end("run-3", 3, &who, "conv-1", 0, None);
        assert_eq!(
            (e.stream.as_str(), e.done, e.exit),
            (ASK_END, Some(true), Some(0))
        );
        assert!(e.line.is_none());

        // A refused one carries the reason on the close, where the ops panel
        // already prints it.
        let e = GyldAskRecord::end("run-3", 3, &who, "conv-1", 1, Some("declined".into()));
        assert_eq!(e.exit, Some(1));
        assert_eq!(e.line.as_deref(), Some("declined"));

        // And it round-trips: one consumer folds these and `gyld.output` both.
        let held: GyldAskRecord = serde_json::from_slice(&e.to_bytes()).unwrap();
        assert_eq!(held, e);
    }

    #[test]
    fn output_records_match_the_gwz_shape() {
        let l = GyldOutputRecord::line("run-1", 1, &Some("p".into()), "stdout", "x".into());
        let s = String::from_utf8(l.to_bytes()).unwrap();
        assert!(
            s.contains("\"run_id\":\"run-1\"") && s.contains("\"seq\":1"),
            "{s}"
        );
        assert!(
            s.contains("\"stream\":\"stdout\"") && s.contains("\"line\":\"x\""),
            "{s}"
        );
        let e = GyldOutputRecord::end("run-1", 2, &None, 0);
        assert_eq!(e.done, Some(true));
        assert_eq!(e.exit, Some(0));
        assert_eq!(e.stream, "end");
    }
}
