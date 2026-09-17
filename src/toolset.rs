//! The tools this supplier actually offers (GyldAskAgent.md section 11.6).
//!
//! [`crate::tools`] is the machinery — what a tool is, which are enabled, what
//! one call may cost. This is the SET: the two concrete tools a build can be
//! asked about, and the one function the supplier calls to offer them.
//!
//! The two tools this module DEFINES are read-only over the build
//! `latest.json` names and over nothing else. The network tools live in their
//! own modules ([`crate::fetch`], [`crate::github`], and `web_search` in phase
//! C) and are assembled here by [`offered`], because what a desk may reach for
//! is one question with one answer.
//!
//! **Two guards, and they are the planner's own** (`crate::verbs`). Every
//! stream id a model names is checked against Gyld's `ID_PATTERN` before it can
//! reach a path, and every path is checked for containment under the build
//! directory. The planner applies both to every verb a REQUESTER sends; they
//! matter more here, where the id is chosen by a model that may have read a
//! page telling it what to choose.
//!
//! **Neither tool opens a cited document.** `read_source` answers out of the
//! index's own passages, which is what section 5 already says the supplier
//! resolves a citation from: *if the index does not carry a passage, there is
//! no passage, and the model is told so*. A tool that grepped the corpus would
//! be a different feature with a different design.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::bundle;
use crate::github::Token;
use crate::sources::{self, SourceIndex};
use crate::tools::{Tool, ToolContext, ToolOutput, ToolPolicy, ToolRefusal};
use crate::verbs::valid_stream_id;

/// The tool that reads a passage out of the build's source index.
pub const READ_SOURCE: &str = "read_source";

/// The tool that reads the build itself.
pub const GYLD_QUERY: &str = "gyld_query";

/// The largest emitted document either tool will read into memory.
///
/// A `projection.json` is a few hundred kilobytes and a `snapshot.json` can be
/// larger; the bound is here so a surprising build cannot be read whole by a
/// tool a model chose to call.
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// The longest slot a tool accepts. Gyld imposes none; a bound keeps a
/// pathological name out of a comparison and out of a refusal's own text.
const MAX_SLOT: usize = 512;

/// How many names a refusal lists before it stops. A refusal is meant to make
/// the NEXT call a better one; a thousand tags in it would make it unreadable
/// and would cost the byte budget it is trying to save.
const NAMED_IN_A_REFUSAL: usize = 40;

/// Every tool this supplier can offer for `context`'s build.
///
/// The allow-list chooses from this list and never adds to it
/// ([`crate::tools::ToolRegistry::build`]), which is what makes "local tools on
/// by default, network tools off until configured" one rule rather than two: a
/// tool that is not offered cannot be enabled by naming it, and a desk that
/// names nothing gets exactly this list.
pub fn local(context: &ToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadSource {
            context: context.clone(),
        }),
        Arc::new(GyldQuery {
            context: context.clone(),
        }),
    ]
}

/// Every tool this supplier can offer this desk: the local ones over
/// `context`'s build, and the network ones of 11.7 under `policy`.
///
/// Offered is not enabled. A network tool is constructed whatever the
/// configuration says, so that a desk which NAMES it in `tools` gets a tool
/// that refuses as data and names the setting it is missing — rather than a
/// note saying this supplier has never heard of it. What an absent `tools` key
/// enables is the separate question [`on_by_default`] answers.
pub fn offered(context: &ToolContext, policy: &ToolPolicy, token: &Token) -> Vec<Arc<dyn Tool>> {
    let mut held = local(context);
    held.push(crate::fetch::FetchUrl::offering(
        policy.fetch.clone(),
        policy.budgets.timeout,
    ));
    held.push(crate::github::GitHub::offering(
        token.clone(),
        policy.budgets.timeout,
        policy.budgets.bytes,
    ));
    held
}

/// The tools a desk that wrote no `tools` key gets.
///
/// The local ones always — they read the build this answer is already grounded
/// in. A network one only when the thing it needs is already there, which is
/// how "local tools are on by default, network tools are off until configured"
/// stays one rule rather than a second allow-list (11.2).
pub fn on_by_default(policy: &ToolPolicy, token: &Token) -> Vec<String> {
    let mut held = vec![READ_SOURCE.to_string(), GYLD_QUERY.to_string()];
    if !policy.fetch.hosts.is_empty() {
        held.push(crate::fetch::FETCH_URL.to_string());
    }
    // A token, because GitHub rate-limits an unauthenticated caller to sixty
    // requests an hour and refuses code search outright: a `github` that is on
    // by default and spends its trickle on the first question is worse than one
    // a desk turns on deliberately.
    if token.found() {
        held.push(crate::github::GITHUB.to_string());
    }
    held
}

/// The one line the attach log carries about tools: what this desk gets when it
/// named none, and why each network tool is or is not among them.
///
/// It names hosts and counts, never a credential and never a page.
pub fn says(policy: &ToolPolicy, token: &Token) -> String {
    let mut held = format!("tools on by default {:?}", on_by_default(policy, token));
    if policy.fetch.hosts.is_empty() {
        held.push_str("; fetch_url off (`fetch_hosts` names no host)");
    } else {
        held.push_str(&format!(
            "; fetch_url may reach {:?} (up to {} bytes a page)",
            policy.fetch.hosts, policy.fetch.bytes
        ));
    }
    held.push_str(&format!(
        "; github has {}{}",
        token.source.says(),
        if token.found() {
            ""
        } else {
            ", so it is off by default"
        }
    ));
    held
}

/// Read one cited passage, or a document's passages, out of the build's index.
struct ReadSource {
    context: ToolContext,
}

impl Tool for ReadSource {
    fn name(&self) -> &str {
        READ_SOURCE
    }

    /// **No `strict`.** The input is a CHOICE of two shapes — a tag, or a
    /// document with an optional heading — which a closed `required` list
    /// cannot express, so the supplier checks it and refuses as data. A
    /// `required: []` with `strict: true` would guarantee nothing at all while
    /// costing a rejection on every endpoint that dislikes the field.
    fn schema(&self) -> Value {
        json!({
            "name": READ_SOURCE,
            "description": "\
        Read a source passage out of THIS build's emitted source index. Call it when \
        the reader asks about a tag the context above does not carry a passage for, or \
        when you want the rest of a document the context quotes one row of. Give \
        either a `tag`, or a `document` with an optional `heading`. It reads the index \
        only: a tag the index does not resolve has no passage, and the refusal names \
        what the index does know.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "tag": {
                        "type": "string",
                        "description": "One source tag, exactly as the records cite \
        it — `Q11`, `GDL-049`, `IrohReview §11`.",
                    },
                    "document": {
                        "type": "string",
                        "description": "One document the index was pointed at, by \
        its id or by its path.",
                    },
                    "heading": {
                        "type": "string",
                        "description": "With `document`: only the passages under a \
        heading whose text contains this.",
                    },
                },
                "additionalProperties": false,
            },
        })
    }

    fn run(&self, input: &Value) -> Result<ToolOutput, ToolRefusal> {
        let index = read_index(&self.context)?;
        let tag = string(input, "tag");
        let document = string(input, "document");
        if !tag.is_empty() {
            return one_tag(&index, &tag);
        }
        if !document.is_empty() {
            return a_document(&index, &document, &string(input, "heading"));
        }
        Err(ToolRefusal::says(
            "read_source needs either a `tag` or a `document`; it was given neither",
        ))
    }
}

/// One tag, as the index resolved it — or the reason there is no passage.
fn one_tag(index: &SourceIndex, tag: &str) -> Result<ToolOutput, ToolRefusal> {
    if let Some(found) = index.tag(tag) {
        return Ok(ToolOutput::text(passage(found)));
    }
    // An unresolved tag is a fact the index EMITTED, with its own reason: it is
    // said, never hidden (MDV-7), and it is said with the reason the emitting
    // run gave rather than one invented here.
    if let Some(held) = index.unresolved.iter().find(|held| held.tag == tag) {
        return Err(ToolRefusal::says(format!(
            "this build's index resolves the tag {tag:?} to nothing: {}. There is no passage for \
             it; say so rather than guessing at one",
            held.reason
        )));
    }
    Err(ToolRefusal::says(format!(
        "this build's index does not list the tag {tag:?}. {}",
        named("It lists", index.tags.iter().map(|t| t.tag.clone()))
    )))
}

/// Every passage of one document, optionally under one heading.
fn a_document(
    index: &SourceIndex,
    document: &str,
    heading: &str,
) -> Result<ToolOutput, ToolRefusal> {
    let found = index
        .documents
        .iter()
        .find(|held| held.id == document || held.path == document)
        .ok_or_else(|| {
            ToolRefusal::says(format!(
                "this build's index was not pointed at a document {document:?}. {}",
                named(
                    "It was pointed at",
                    index.documents.iter().map(|d| d.id.clone())
                )
            ))
        })?;
    let wanted = heading.trim().to_lowercase();
    let held: Vec<String> = index
        .tags
        .iter()
        .filter(|tag| tag.document == found.id)
        .filter(|tag| wanted.is_empty() || tag.heading.to_lowercase().contains(&wanted))
        .map(passage)
        .collect();
    if held.is_empty() {
        return Err(ToolRefusal::says(format!(
            "this build's index resolves no tag in {:?}{}",
            found.id,
            if heading.trim().is_empty() {
                String::new()
            } else {
                format!(" under a heading containing {heading:?}")
            }
        )));
    }
    Ok(ToolOutput::text(held.join("\n")))
}

/// One resolved tag, as the passage a model may quote.
fn passage(tag: &crate::sources::IndexedTag) -> String {
    let lines = match tag.lines.as_slice() {
        [first, last] => format!("lines {first}-{last}"),
        _ => "lines not emitted".to_string(),
    };
    format!(
        "## {} — {} ({}), {}, under {:?}{}\n\n{}\n",
        tag.tag,
        tag.document,
        tag.path,
        lines,
        tag.heading,
        if tag.truncated {
            " (the index capped this passage: it is a prefix)"
        } else {
            ""
        },
        tag.passage,
    )
}

/// Read the build itself: a record, a decide-now list, the rulings on a slot, a
/// diff, or the census.
struct GyldQuery {
    context: ToolContext,
}

impl Tool for GyldQuery {
    fn name(&self) -> &str {
        GYLD_QUERY
    }

    fn schema(&self) -> Value {
        json!({
            "name": GYLD_QUERY,
            "description": "\
        Read THIS build's emitted decision streams. Call it when the reader asks about \
        a stream, a record or a ruling the context above does not carry — another \
        stream's ruling on the same question, what a stream can answer now, how two \
        streams differ, or what streams exist at all. It is read-only and it never \
        writes, rules or submits anything. Answers are compact JSON emitted by the \
        build; a stream or a record it does not list is a refusal that names what it \
        does list.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["record", "decide_now", "rulings", "diff", "streams"],
                        "description": "`record` needs `stream` and `slot`; \
        `decide_now` needs `stream`; `rulings` needs `slot`; `diff` needs `left` and \
        `right`; `streams` needs nothing.",
                    },
                    "stream": {"type": "string", "description": "A stream id."},
                    "slot": {
                        "type": "string",
                        "description": "A QUALIFIED SLOT, as the context and the \
        answers spell it: `glade_decisions:GladeDecisions.key_custody`.",
                    },
                    "left": {"type": "string", "description": "`diff`: the left stream."},
                    "right": {"type": "string", "description": "`diff`: the right stream."},
                },
                "required": ["kind"],
                "additionalProperties": false,
            },
        })
    }

    fn run(&self, input: &Value) -> Result<ToolOutput, ToolRefusal> {
        let kind = string(input, "kind");
        let answered = match kind.as_str() {
            "streams" => self.streams(),
            "decide_now" => self.decide_now(&self.stream(input)?),
            "record" => self.record(&self.stream(input)?, &self.slot(input)?),
            "rulings" => self.rulings(&self.slot(input)?),
            "diff" => self.diff(
                &self.named_stream(input, "left")?,
                &self.named_stream(input, "right")?,
            ),
            "" => {
                return Err(ToolRefusal::says(
                    "gyld_query needs a `kind`: \"record\", \"decide_now\", \"rulings\", \"diff\" \
                     or \"streams\"",
                ));
            }
            other => {
                return Err(ToolRefusal::says(format!(
                    "gyld_query has no kind {other:?}; it answers \"record\", \"decide_now\", \
                     \"rulings\", \"diff\" and \"streams\""
                )));
            }
        }?;
        Ok(ToolOutput::text(answered.to_string()))
    }
}

impl GyldQuery {
    /// The census: every stream the build lists, and what it is.
    fn streams(&self) -> Result<Value, ToolRefusal> {
        let listed = self.read(&self.context.build.join("streams.json"))?;
        let streams: Vec<Value> = array(&listed, "streams")
            .iter()
            .map(|held| {
                json!({
                    "id": held.get("id"),
                    "kind": held.get("kind"),
                    "parent": held.get("parent"),
                    "lineage": held.get("lineage"),
                    "revision": held.get("revision"),
                    "status": held.get("status"),
                    "note": held.get("note"),
                    "questions": array(held, "questions").len(),
                })
            })
            .collect();
        Ok(json!({
            "kind": "streams",
            "lineages": listed.get("lineages"),
            "written": listed.get("written"),
            "streams": streams,
        }))
    }

    /// One stream's decide-now list: every row with the one emitted reason it
    /// is not answerable now.
    fn decide_now(&self, stream: &str) -> Result<Value, ToolRefusal> {
        let held = self.decide_now_of(stream)?;
        let rows: Vec<Value> = array(&held, "questions").iter().map(row).collect();
        Ok(json!({
            "kind": "decide_now",
            "stream": stream,
            "snapshot": held.get("snapshot"),
            "limits": held.get("limits"),
            "rows": rows,
        }))
    }

    /// One record, as this stream emitted it: its row, the ruling that decides
    /// it, and what its own definition declares.
    fn record(&self, stream: &str, slot: &str) -> Result<Value, ToolRefusal> {
        let held = self.decide_now_of(stream)?;
        let rows = array(&held, "questions");
        let found = rows
            .iter()
            .find(|row| text(row, "slot") == slot)
            .ok_or_else(|| {
                ToolRefusal::says(format!(
                    "this build's decide-now list for {stream:?} carries no row for {slot:?}. {}",
                    named("It lists", rows.iter().map(|row| text(row, "slot")))
                ))
            })?;
        let ruling = array(&held, "rulings")
            .iter()
            .find(|held| text(held, "decides") == slot)
            .cloned();
        let declared = self.declared(stream, slot);
        Ok(json!({
            "kind": "record",
            "stream": stream,
            "snapshot": held.get("snapshot"),
            "row": row(found),
            "ruling": ruling,
            "declared": declared,
        }))
    }

    /// Which streams rule this slot, and how.
    ///
    /// Every stream the build lists is asked, because that is the question:
    /// *what did some OTHER stream decide about this*, which no one stream's
    /// own list can answer.
    fn rulings(&self, slot: &str) -> Result<Value, ToolRefusal> {
        let listed = self.read(&self.context.build.join("streams.json"))?;
        let mut out: Vec<Value> = Vec::new();
        let mut asked: Vec<String> = Vec::new();
        for held in array(&listed, "streams").iter() {
            let stream = text(held, "id");
            if !valid_stream_id(&stream) {
                continue;
            }
            let decide = match self.decide_now_of(&stream) {
                Ok(decide) => decide,
                // A stream that emitted no decide-now list emits no rulings
                // either. It is not an error and it is not silence: it is named
                // in `asked_but_emitted_nothing` below.
                Err(_) => {
                    asked.push(stream);
                    continue;
                }
            };
            let status = array(&decide, "questions")
                .iter()
                .find(|row| text(row, "slot") == slot)
                .map(|row| {
                    json!({
                        "effective_status": row.get("effective_status"),
                        "preferred": row.get("preferred"),
                        "ruling": row.get("ruling"),
                    })
                });
            for ruling in array(&decide, "rulings").iter() {
                let mentions = text(ruling, "decides") == slot
                    || text(ruling, "selects") == slot
                    || text(ruling, "reopens") == slot
                    || array(ruling, "occurred")
                        .iter()
                        .any(|held| held.as_str() == Some(slot));
                if !mentions {
                    continue;
                }
                out.push(json!({
                    "in_stream": stream,
                    "ruling": ruling,
                    "this_records_status_there": status,
                }));
            }
        }
        Ok(json!({
            "kind": "rulings",
            "slot": slot,
            "rules_it": out,
            "streams_that_emitted_no_decide_now_list": asked,
        }))
    }

    /// The emitted diff of two streams, or the command that writes one.
    fn diff(&self, left: &str, right: &str) -> Result<Value, ToolRefusal> {
        let path = self.contained(
            &self
                .context
                .build
                .join(format!("diffs/{left}..{right}.json")),
        )?;
        if !path.is_file() {
            return Err(ToolRefusal::says(format!(
                "this build emits no diff of {left:?} against {right:?}. A diff is written by the \
                 `diff` verb — `manage_decision_streams.py diff {left} {right}` — and until \
                 somebody runs it there is nothing to read. {}",
                named("This build emits", self.emitted_diffs())
            )));
        }
        self.read(&path)
    }

    /// The diffs this build actually has, for the refusal above.
    fn emitted_diffs(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.context.build.join("diffs"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter_map(|name| name.strip_suffix(".json").map(str::to_string))
            .collect();
        out.sort();
        out
    }

    /// What a record's own definition declares, out of the projection: its
    /// description, and the tags it cites.
    ///
    /// Absence is a VALUE here and not an error. A build whose projection does
    /// not carry the authoring sidecar emits no `matrix` and no `sources` for a
    /// question at all (section 5), and a record that says "not emitted" is the
    /// truth about that build; failing the whole query over it would hide a
    /// row the reader did ask for.
    fn declared(&self, stream: &str, slot: &str) -> Value {
        let path = match self.contained(&self.stream_dir(stream).join("projection.json")) {
            Ok(path) => path,
            Err(_) => {
                return Value::Null;
            }
        };
        let projection = match self.read(&path) {
            Ok(projection) => projection,
            Err(_) => {
                return Value::Null;
            }
        };
        let occurrence = array(&projection, "occurrences")
            .iter()
            .find(|held| {
                held.pointer("/source/qualified_slot")
                    .and_then(|v| v.as_str())
                    == Some(slot)
            })
            .cloned();
        let definition = occurrence
            .as_ref()
            .and_then(|held| held.get("definition"))
            .and_then(|v| v.as_str())
            .and_then(|id| projection.pointer(&format!("/definitions/{id}")))
            .cloned();
        let authoring = definition
            .as_ref()
            .and_then(|held| held.pointer("/source/authoring/properties"))
            .cloned();
        json!({
            "label": definition.as_ref().and_then(|d| d.get("label")),
            "description": definition.as_ref().and_then(|d| d.get("description")),
            "matrix": authoring.as_ref().and_then(|a| a.get("matrix")),
            "sources": authoring.as_ref().and_then(|a| a.get("sources")),
            "source": occurrence.as_ref().and_then(|o| o.get("source")),
        })
    }

    /// One stream's `decide-now.json`, or a refusal naming the streams that
    /// have one.
    fn decide_now_of(&self, stream: &str) -> Result<Value, ToolRefusal> {
        let path = self.contained(&self.stream_dir(stream).join("decide-now.json"))?;
        if !path.is_file() {
            return Err(ToolRefusal::says(format!(
                "this build emits no decide-now list for {stream:?}. {}",
                named("It emits one for", self.listed_streams())
            )));
        }
        self.read(&path)
    }

    /// The streams this build emits a decide-now list for.
    fn listed_streams(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.context.build.join("streams"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().join("decide-now.json").is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        out.sort();
        out
    }

    fn stream_dir(&self, stream: &str) -> PathBuf {
        self.context.build.join("streams").join(stream)
    }

    /// `input`'s `stream`, checked against Gyld's own id pattern.
    fn stream(&self, input: &Value) -> Result<String, ToolRefusal> {
        self.named_stream(input, "stream")
    }

    fn named_stream(&self, input: &Value, field: &str) -> Result<String, ToolRefusal> {
        let id = string(input, field);
        if id.is_empty() {
            return Err(ToolRefusal::says(format!(
                "this kind of gyld_query needs a `{field}`, and none was given"
            )));
        }
        if !valid_stream_id(&id) {
            return Err(ToolRefusal::says(format!(
                "{id:?} is not a stream id: Gyld's ids are lowercase words joined by hyphens. {}",
                named("This build lists", self.listed_streams())
            )));
        }
        Ok(id)
    }

    fn slot(&self, input: &Value) -> Result<String, ToolRefusal> {
        let slot = string(input, "slot");
        if slot.is_empty() {
            return Err(ToolRefusal::says(
                "this kind of gyld_query needs a `slot`, and none was given",
            ));
        }
        if slot.len() > MAX_SLOT {
            return Err(ToolRefusal::says(format!(
                "that slot is {} bytes; the limit is {MAX_SLOT}",
                slot.len()
            )));
        }
        Ok(slot)
    }

    /// Containment, unconditional, on every path a model's input reached.
    ///
    /// The planner applies the same check to every verb a requester sends
    /// (`verbs::plan`). It matters more here: the id came from a model, which
    /// may have read a page that told it what to ask for.
    fn contained(&self, path: &Path) -> Result<PathBuf, ToolRefusal> {
        if !bundle::contained(&self.context.build, path) {
            return Err(ToolRefusal::says(format!(
                "{} is outside this build, so it is not read",
                path.display()
            )));
        }
        Ok(path.to_path_buf())
    }

    /// One emitted document, bounded and as data.
    fn read(&self, path: &Path) -> Result<Value, ToolRefusal> {
        let path = self.contained(path)?;
        let meta = std::fs::metadata(&path).map_err(|e| {
            ToolRefusal::says(format!(
                "cannot read {}: {e}",
                named_path(&path, &self.context)
            ))
        })?;
        if meta.len() > MAX_READ_BYTES {
            return Err(ToolRefusal::says(format!(
                "{} is {} bytes; this tool reads at most {MAX_READ_BYTES}",
                named_path(&path, &self.context),
                meta.len()
            )));
        }
        let bytes = std::fs::read(&path).map_err(|e| {
            ToolRefusal::says(format!(
                "cannot read {}: {e}",
                named_path(&path, &self.context)
            ))
        })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            ToolRefusal::says(format!(
                "{} did not decode: {e}",
                named_path(&path, &self.context)
            ))
        })
    }
}

/// One decide-now row, whole and uninterpreted: the fields the emitter wrote,
/// under the names it wrote them.
fn row(held: &Value) -> Value {
    json!({
        "slot": held.get("slot"),
        "label": held.get("label"),
        "declared_status": held.get("declared_status"),
        "effective_status": held.get("effective_status"),
        "tier": held.get("tier"),
        "answerable_now": held.get("answerable_now"),
        "blocked_by": held.get("blocked_by"),
        "gated_by": held.get("gated_by"),
        "induced_by": held.get("induced_by"),
        "offers": held.get("offers"),
        "preferred": held.get("preferred"),
        "ruling": held.get("ruling"),
    })
}

/// A path as a refusal names it: relative to the build, so no absolute path of
/// the owner's machine reaches a record a page draws.
fn named_path(path: &Path, context: &ToolContext) -> String {
    path.strip_prefix(&context.build)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// "It lists a, b and c" — bounded, because a refusal is meant to make the next
/// call a better one rather than to spend the byte budget.
fn named(opening: &str, names: impl IntoIterator<Item = String>) -> String {
    let held: Vec<String> = names.into_iter().collect();
    if held.is_empty() {
        return format!("{opening} nothing.");
    }
    let shown = held
        .iter()
        .take(NAMED_IN_A_REFUSAL)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if held.len() > NAMED_IN_A_REFUSAL {
        return format!(
            "{opening} {shown}, and {} more.",
            held.len() - NAMED_IN_A_REFUSAL
        );
    }
    format!("{opening} {shown}.")
}

/// Read the build's source index, as a refusal rather than an error.
fn read_index(context: &ToolContext) -> Result<SourceIndex, ToolRefusal> {
    sources::read(&context.sources).map_err(ToolRefusal::says)
}

fn string(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn text(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    /// A bundle in the shape a build emits, small enough to read whole and wide
    /// enough to cover every kind and every refusal: two streams, one of them
    /// with no decide-now list, one ruling in the other, one emitted diff, and
    /// the fixture source index.
    fn fixture(tag: &str) -> ToolContext {
        let root =
            std::env::temp_dir().join(format!("glade-gyld-toolset-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let build = root.join("builds/build-1");
        std::fs::create_dir_all(build.join("diffs")).unwrap();
        for stream in ["base", "stream-a", "architecture"] {
            std::fs::create_dir_all(build.join("streams").join(stream)).unwrap();
        }
        write(
            &build.join("sources.json"),
            &serde_json::to_value(crate::sources::tests::index()).unwrap(),
        );
        write(
            &build.join("streams.json"),
            &json!({
                "format": "gyld.streams.v1",
                "written": "2026-09-17T09:00:00Z",
                "lineages": ["glade-decision-graph"],
                "streams": [
                    {"id": "base", "kind": "fork", "parent": null,
                     "lineage": "glade-decision-graph", "revision": "v1", "status": "ok",
                     "note": "the base", "questions": [{"slot": "s1"}, {"slot": "s2"}]},
                    {"id": "stream-a", "kind": "link", "parent": "base",
                     "lineage": "glade-decision-graph", "revision": "stream-a@1",
                     "status": "ok", "note": "version pin ruled",
                     "questions": [{"slot": "s1"}]},
                    {"id": "architecture", "kind": "declaration", "parent": null,
                     "lineage": "glade-architecture-candidate-1", "revision": "v1",
                     "status": "ok", "note": "no decide-now list", "questions": []}
                ]
            }),
        );
        write(
            &build.join("streams/base/decide-now.json"),
            &json!({
                "format": "gyld.decide-now.v1",
                "snapshot": {"digest": "abc"},
                "stream": "base",
                "limits": ["no ruling"],
                "questions": [
                    {"slot": "glade_decisions:GladeDecisions.key_custody", "label": "key_custody",
                     "declared_status": "Open", "effective_status": "Open", "tier": "roots",
                     "answerable_now": true, "blocked_by": [], "gated_by": [], "induced_by": [],
                     "offers": ["glade_decisions:GladeDecisions.owner_held_only"],
                     "preferred": null, "ruling": null},
                    {"slot": "glade_decisions:GladeDecisions.version_pin", "label": "version_pin",
                     "declared_status": "Lean", "effective_status": "Lean", "tier": "roots",
                     "answerable_now": true, "blocked_by": [], "gated_by": [], "induced_by": [],
                     "offers": [], "preferred": "glade_decisions:GladeDecisions.bump_to_current",
                     "ruling": null}
                ],
                "rulings": []
            }),
        );
        write(
            &build.join("streams/stream-a/decide-now.json"),
            &json!({
                "format": "gyld.decide-now.v1",
                "snapshot": {"digest": "def"},
                "stream": "stream-a",
                "limits": [],
                "questions": [
                    {"slot": "glade_decisions:GladeDecisions.version_pin", "label": "version_pin",
                     "declared_status": "Lean", "effective_status": "Decided", "tier": "roots",
                     "answerable_now": false, "blocked_by": [], "gated_by": [], "induced_by": [],
                     "offers": [], "preferred": "glade_decisions:GladeDecisions.bump_to_current",
                     "ruling": "glade_decisions_stream_a:GladeDecisionsStreamA.version_pin_ruling"}
                ],
                "rulings": [
                    {"slot": "glade_decisions_stream_a:GladeDecisionsStreamA.version_pin_ruling",
                     "label": "version_pin_ruling", "stream": "stream-a",
                     "decides": "glade_decisions:GladeDecisions.version_pin",
                     "selects": "glade_decisions:GladeDecisions.bump_to_current",
                     "reopens": null, "occurred": [], "live": true,
                     "text": "2026-09-13, owner: take iroh 1.2.0 now.",
                     "principal": "gianni", "stamp": "2026-09-13T01:00:00Z",
                     "sources": ["IrohReview §1", "IrohReview §11"]}
                ]
            }),
        );
        write(
            &build.join("streams/base/projection.json"),
            &json!({
                "format": "gyld.projection.v1",
                "occurrences": [
                    {"id": "occ1", "definition": "def1", "label": "key_custody",
                     "source": {"module": "glade_decisions", "line": 467,
                                "qualified_slot": "glade_decisions:GladeDecisions.key_custody"}}
                ],
                "definitions": {
                    "def1": {
                        "kind": "entity", "label": "KeyCustody",
                        "description": "Root, device, node and operator key custody.",
                        "source": {"qualified_slot": "glade_decisions:KeyCustody",
                                   "authoring": {"properties": {
                                       "matrix": "Q11",
                                       "sources": ["WD-1", "AZ-7", "SEC-55-D5"]}}}
                    }
                }
            }),
        );
        write(
            &build.join("diffs/base..stream-a.json"),
            &json!({
                "format": "gyld.stream-diff.v1",
                "left": {"stream": "base"}, "right": {"stream": "stream-a"},
                "effective_status": [
                    {"slot": "glade_decisions:GladeDecisions.version_pin",
                     "left": "Lean", "right": "Decided"}
                ]
            }),
        );
        ToolContext {
            sources: build.join("sources.json"),
            build,
        }
    }

    fn write(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn held(context: &ToolContext) -> ToolRegistry {
        ToolRegistry::of(local(context), crate::tools::ToolBudgets::default())
    }

    /// Run one tool and answer with the text it produced, or the refusal.
    fn run(context: &ToolContext, name: &str, input: Value) -> Result<String, String> {
        let registry = held(context);
        let tool = registry.find(name).expect("the tool is offered").clone();
        tool.run(&input)
            .map(|output| output.text)
            .map_err(|refusal| refusal.reason)
    }

    fn answered(context: &ToolContext, input: Value) -> Value {
        serde_json::from_str(&run(context, GYLD_QUERY, input).expect("an answer"))
            .expect("compact JSON")
    }

    #[test]
    fn both_local_tools_are_offered_and_declare_a_schema_each() {
        let context = fixture("offered");
        let registry = held(&context);
        assert_eq!(registry.names(), vec![GYLD_QUERY, READ_SOURCE]);
        for declared in registry.declarations().iter() {
            assert!(declared.get("description").is_some(), "{declared}");
            assert_eq!(declared["input_schema"]["type"], "object", "{declared}");
        }
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn read_source_answers_a_tag_with_the_indexs_own_passage() {
        let context = fixture("tag");
        let said = run(&context, READ_SOURCE, json!({"tag": "Q11"})).expect("Q11");
        assert!(said.contains("## Q11 — GladeBuyBuildMatrix"), "{said}");
        assert!(said.contains("dev-docs/GladeBuyBuildMatrix.md"), "{said}");
        assert!(said.contains("lines 158-158"), "{said}");
        assert!(said.contains("\"4. The questions\""), "{said}");
        assert!(
            said.contains("| Q11 | Key custody and recovery posture | buy |"),
            "the index's own passage, whole: {said}"
        );

        // A capped passage SAYS it is a prefix rather than reading as whole.
        let capped = run(&context, READ_SOURCE, json!({"tag": "IrohReview §11"})).expect("§11");
        assert!(capped.contains("the index capped this passage"), "{capped}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn read_source_answers_a_document_and_narrows_it_by_heading() {
        let context = fixture("doc");
        let whole = run(
            &context,
            READ_SOURCE,
            json!({"document": "GladeBuyBuildMatrix"}),
        )
        .expect("the document");
        assert!(whole.contains("## Q11"), "{whole}");
        assert!(
            !whole.contains("## IrohReview"),
            "one document only: {whole}"
        );

        // By path as well as by id, because the index emits both.
        let by_path = run(
            &context,
            READ_SOURCE,
            json!({"document": "dev-docs/GladeBuyBuildMatrix.md"}),
        )
        .expect("the same document");
        assert_eq!(by_path, whole);

        let narrowed = run(
            &context,
            READ_SOURCE,
            json!({"document": "GladeBuyBuildMatrix", "heading": "the questions"}),
        )
        .expect("the heading");
        assert!(narrowed.contains("## Q11"), "{narrowed}");

        let e = run(
            &context,
            READ_SOURCE,
            json!({"document": "GladeBuyBuildMatrix", "heading": "nothing like this"}),
        )
        .expect_err("a refusal");
        assert!(e.contains("resolves no tag"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn read_source_refuses_an_unknown_tag_and_an_unresolved_one_differently() {
        let context = fixture("unknown");

        // The index resolves AZ-7 to nothing and SAYS why — the emitting run's
        // own reason, not one invented here (MDV-7).
        let e = run(&context, READ_SOURCE, json!({"tag": "AZ-7"})).expect_err("unresolved");
        assert!(e.contains("resolves the tag \"AZ-7\" to nothing"), "{e}");
        assert!(e.contains("no document in this index declares it"), "{e}");
        assert!(e.contains("say so rather than guessing"), "{e}");

        // A tag it has never heard of is a different fact, and the refusal
        // names what it DOES list so the next call is a better one.
        let e = run(&context, READ_SOURCE, json!({"tag": "ZZ-9"})).expect_err("unknown");
        assert!(e.contains("does not list the tag \"ZZ-9\""), "{e}");
        assert!(e.contains("It lists Q11"), "{e}");

        let e = run(&context, READ_SOURCE, json!({"document": "Nope"})).expect_err("no document");
        assert!(e.contains("was not pointed at a document \"Nope\""), "{e}");
        assert!(e.contains("It was pointed at GladeBuyBuildMatrix"), "{e}");

        let e = run(&context, READ_SOURCE, json!({})).expect_err("neither");
        assert!(e.contains("either a `tag` or a `document`"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn read_source_refuses_a_build_with_no_index_as_data() {
        let context = fixture("no-index");
        std::fs::remove_file(&context.sources).unwrap();
        let e = run(&context, READ_SOURCE, json!({"tag": "Q11"})).expect_err("no index");
        assert!(e.contains("cannot read the source index"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn gyld_query_streams_is_the_census() {
        let context = fixture("census");
        let said = answered(&context, json!({"kind": "streams"}));
        assert_eq!(said["kind"], "streams");
        let streams = said["streams"].as_array().cloned().unwrap_or_default();
        assert_eq!(streams.len(), 3);
        assert_eq!(streams[0]["id"], "base");
        assert_eq!(streams[0]["questions"], 2, "the count, not the list");
        assert_eq!(streams[1]["parent"], "base");
        assert_eq!(streams[2]["lineage"], "glade-architecture-candidate-1");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn gyld_query_decide_now_is_the_rows_with_their_reasons() {
        let context = fixture("decide");
        let said = answered(&context, json!({"kind": "decide_now", "stream": "base"}));
        assert_eq!(said["stream"], "base");
        assert_eq!(said["snapshot"]["digest"], "abc");
        assert_eq!(said["limits"][0], "no ruling");
        let rows = said["rows"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["label"], "key_custody");
        assert_eq!(rows[0]["answerable_now"], true);
        assert_eq!(
            rows[0]["offers"][0],
            "glade_decisions:GladeDecisions.owner_held_only"
        );
        assert_eq!(
            rows[1]["preferred"],
            "glade_decisions:GladeDecisions.bump_to_current"
        );

        // A stream with no list is a refusal naming the streams that have one.
        let e = run(
            &context,
            GYLD_QUERY,
            json!({"kind": "decide_now", "stream": "architecture"}),
        )
        .expect_err("no list");
        assert!(
            e.contains("emits no decide-now list for \"architecture\""),
            "{e}"
        );
        assert!(e.contains("base, stream-a"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn gyld_query_record_carries_the_row_the_ruling_and_what_the_definition_declares() {
        let context = fixture("record");
        let said = answered(
            &context,
            json!({"kind": "record", "stream": "base",
                   "slot": "glade_decisions:GladeDecisions.key_custody"}),
        );
        assert_eq!(said["row"]["label"], "key_custody");
        assert_eq!(said["row"]["tier"], "roots");
        assert_eq!(said["ruling"], Value::Null, "base rules nothing");
        assert_eq!(
            said["declared"]["description"],
            "Root, device, node and operator key custody."
        );
        assert_eq!(said["declared"]["matrix"], "Q11");
        assert_eq!(said["declared"]["sources"][1], "AZ-7");

        // The same slot in the stream that RULED it carries the ruling.
        let ruled = answered(
            &context,
            json!({"kind": "record", "stream": "stream-a",
                   "slot": "glade_decisions:GladeDecisions.version_pin"}),
        );
        assert_eq!(ruled["row"]["effective_status"], "Decided");
        assert_eq!(
            ruled["ruling"]["text"],
            "2026-09-13, owner: take iroh 1.2.0 now."
        );
        assert_eq!(ruled["ruling"]["sources"][0], "IrohReview §1");
        assert_eq!(
            ruled["declared"],
            Value::Null,
            "stream-a emits no projection here, and that is said rather than failed"
        );

        let e = run(
            &context,
            GYLD_QUERY,
            json!({"kind": "record", "stream": "base", "slot": "nope"}),
        )
        .expect_err("no row");
        assert!(e.contains("carries no row for \"nope\""), "{e}");
        assert!(e.contains("key_custody"), "it names what it lists: {e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn gyld_query_rulings_says_which_stream_ruled_a_slot_and_how() {
        let context = fixture("rulings");
        let said = answered(
            &context,
            json!({"kind": "rulings", "slot": "glade_decisions:GladeDecisions.version_pin"}),
        );
        let rules = said["rules_it"].as_array().cloned().unwrap_or_default();
        assert_eq!(rules.len(), 1, "{said}");
        assert_eq!(rules[0]["in_stream"], "stream-a");
        assert_eq!(rules[0]["ruling"]["live"], true);
        assert_eq!(
            rules[0]["ruling"]["selects"],
            "glade_decisions:GladeDecisions.bump_to_current"
        );
        assert_eq!(
            rules[0]["this_records_status_there"]["effective_status"],
            "Decided"
        );
        assert_eq!(
            said["streams_that_emitted_no_decide_now_list"][0], "architecture",
            "a stream that could not be asked is named, never silently skipped"
        );

        // A slot nothing rules is an empty list and not a refusal: "nothing
        // rules it" is the answer to the question.
        let none = answered(
            &context,
            json!({"kind": "rulings", "slot": "glade_decisions:GladeDecisions.key_custody"}),
        );
        assert!(none["rules_it"].as_array().unwrap().is_empty(), "{none}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn gyld_query_diff_reads_the_emitted_one_and_names_the_command_when_there_is_none() {
        let context = fixture("diff");
        let said = answered(
            &context,
            json!({"kind": "diff", "left": "base", "right": "stream-a"}),
        );
        assert_eq!(said["format"], "gyld.stream-diff.v1");
        assert_eq!(said["effective_status"][0]["right"], "Decided");

        let e = run(
            &context,
            GYLD_QUERY,
            json!({"kind": "diff", "left": "stream-a", "right": "base"}),
        )
        .expect_err("not emitted");
        assert!(
            e.contains("emits no diff of \"stream-a\" against \"base\""),
            "{e}"
        );
        assert!(
            e.contains("manage_decision_streams.py diff stream-a base"),
            "the refusal names the command that writes one: {e}"
        );
        assert!(e.contains("base..stream-a"), "and what IS emitted: {e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_stream_id_a_model_invented_never_reaches_a_path() {
        let context = fixture("guards");
        for id in ["../../etc", "base/../..", "Base", "--force", ""] {
            let e = run(
                &context,
                GYLD_QUERY,
                json!({"kind": "decide_now", "stream": id}),
            )
            .expect_err("refused");
            assert!(
                e.contains("is not a stream id") || e.contains("needs a `stream`"),
                "{id:?} was not refused: {e}"
            );
        }
        // And the slot, which reaches no path but must still be bounded.
        let e = run(
            &context,
            GYLD_QUERY,
            json!({"kind": "rulings", "slot": "x".repeat(MAX_SLOT + 1)}),
        )
        .expect_err("refused");
        assert!(e.contains("the limit is"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_kind_nobody_has_heard_of_names_the_five_that_exist() {
        let context = fixture("kind");
        let e = run(&context, GYLD_QUERY, json!({"kind": "grep"})).expect_err("refused");
        assert!(e.contains("no kind \"grep\""), "{e}");
        assert!(
            e.contains("\"rulings\"") && e.contains("\"streams\""),
            "{e}"
        );

        let e = run(&context, GYLD_QUERY, json!({})).expect_err("refused");
        assert!(e.contains("needs a `kind`"), "{e}");
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn every_answer_is_compact_json_under_the_default_byte_cap() {
        let context = fixture("compact");
        let registry = held(&context);
        for input in [
            json!({"kind": "streams"}),
            json!({"kind": "decide_now", "stream": "base"}),
            json!({"kind": "record", "stream": "base",
                   "slot": "glade_decisions:GladeDecisions.key_custody"}),
            json!({"kind": "rulings", "slot": "glade_decisions:GladeDecisions.version_pin"}),
            json!({"kind": "diff", "left": "base", "right": "stream-a"}),
        ] {
            let answered = registry.call("toolu_1", GYLD_QUERY, &input);
            assert!(answered.ok, "{input}: {answered:?}");
            assert!(!answered.truncated, "{input} was capped: {answered:?}");
            assert!(
                !answered.summary.contains("\n  "),
                "{input} was pretty-printed, which spends the budget on whitespace"
            );
        }
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_network_tool_is_offered_always_and_enabled_only_once_it_is_configured() {
        let context = fixture("network");
        let bare = ToolPolicy::default();
        // No token, whatever this machine's own `gh` would say: the discovery
        // is threaded in rather than reached for, so the test is the same on
        // every desk.
        let none = crate::github::Token::default();
        assert_eq!(
            on_by_default(&bare, &none),
            vec![READ_SOURCE.to_string(), GYLD_QUERY.to_string()],
            "nobody wrote `tools` and nobody wrote `fetch_hosts`, so the local two"
        );

        // Offered is not enabled: the tool EXISTS so a desk can name it, and
        // the default allow-list simply does not carry it.
        let names: Vec<String> = offered(&context, &bare, &none)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert!(
            names.contains(&crate::fetch::FETCH_URL.to_string()),
            "{names:?}"
        );
        let (registry, notes) = ToolRegistry::build(
            &bare.allowing(on_by_default(&bare, &none)),
            offered(&context, &bare, &none),
        );
        assert_eq!(registry.names(), vec![GYLD_QUERY, READ_SOURCE]);
        assert!(notes.is_empty(), "{notes:?}");

        // `fetch_hosts` is the switch: writing it turns the tool on for a desk
        // that named no `tools` at all.
        let configured = ToolPolicy {
            fetch: crate::tools::FetchPolicy {
                hosts: vec!["docs.rs".to_string()],
                bytes: 4096,
            },
            ..Default::default()
        };
        assert!(on_by_default(&configured, &none).contains(&crate::fetch::FETCH_URL.to_string()));
        let (registry, notes) = ToolRegistry::build(
            &configured.allowing(on_by_default(&configured, &none)),
            offered(&context, &configured, &none),
        );
        assert_eq!(
            registry.names(),
            vec![crate::fetch::FETCH_URL, GYLD_QUERY, READ_SOURCE]
        );
        assert!(notes.is_empty(), "{notes:?}");

        // A desk that NAMES it with no hosts gets it, and it refuses as data
        // naming the setting — not a note saying the tool does not exist.
        let named = ToolPolicy {
            allow: Some(vec![crate::fetch::FETCH_URL.to_string()]),
            ..Default::default()
        };
        let (registry, notes) = ToolRegistry::build(
            &named.allowing(on_by_default(&named, &none)),
            offered(&context, &named, &none),
        );
        assert_eq!(registry.names(), vec![crate::fetch::FETCH_URL]);
        assert!(notes.is_empty(), "{notes:?}");
        let answered = registry.call(
            "toolu_1",
            crate::fetch::FETCH_URL,
            &json!({"url": "https://docs.rs/"}),
        );
        assert!(!answered.ok);
        assert!(answered.summary.contains("`fetch_hosts`"), "{answered:?}");

        // A token is github's own switch, and it is the only one: a desk with
        // no token does not get it by default, and a desk with one does.
        let held = crate::github::Token::of(crate::github::Source::Env, Some("t".to_string()));
        assert!(!on_by_default(&bare, &none).contains(&crate::github::GITHUB.to_string()));
        assert!(on_by_default(&bare, &held).contains(&crate::github::GITHUB.to_string()));

        // The attach line says which, and where — hosts, counts and the token's
        // SOURCE, never a secret.
        let bare_says = says(&bare, &none);
        assert!(bare_says.contains("fetch_url off"), "{bare_says}");
        assert!(
            bare_says.contains("github has no token (unauthenticated), so it is off by default"),
            "{bare_says}"
        );
        let said = says(&configured, &none);
        assert!(said.contains("docs.rs") && said.contains("4096"), "{said}");
        let with_token = says(&configured, &held);
        assert!(
            with_token.contains("github has a token from GITHUB_TOKEN"),
            "{with_token}"
        );
        let _ = std::fs::remove_dir_all(context.build.parent().unwrap().parent().unwrap());
    }
}
