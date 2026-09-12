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
