//! The conversation: the prior turns a follow-up replays, and what one
//! conversation has spent (GyldAskAgent.md sections 6 and 7).
//!
//! **The transcript is the log share.** `gyld.ask` is keyed by the CONVERSATION
//! rather than by the run id, so every record of every turn of one conversation
//! folds to one place — and the supplier reads back what it wrote there rather
//! than keeping a second copy of the same words somewhere else. A turn's
//! records are appended by [`crate::supplier`] as they arrive and folded here
//! before the NEXT turn is composed.
//!
//! That is why the question is a record. The reply table of section 4 carries
//! what the model produced; the question is what the reader produced, and
//! without it on the surface the log is not the transcript section 6 says it
//! is — a follow-up could be replayed only as a list of answers to questions
//! nobody kept. `question` is a fifth record stream and costs a consumer that
//! never heard of it nothing: absent records are absent lines, never blank
//! ones.
//!
//! Everything here is PURE except [`Ledger`], which is a counter behind a lock.
//! Folding records into turns, and turns into the request's `messages`, is a
//! function of bytes the supplier already has.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::envelope::{
    GyldAskRecord, ASK_ANSWER, ASK_DRAFT, ASK_END, ASK_QUESTION, ASK_TOOL_CALL, ASK_TOOL_RESULT,
};
use crate::tools::{capped, REPLAYED_RESULT_CHARS};

/// One thing a prior turn did, in the order it did it.
///
/// A multi-step turn is prose, then a tool call, then its result, then more
/// prose — and a follow-up that was replayed the prose alone would be reading a
/// transcript in which the agent knew things it was never told. So the parts
/// are ORDERED and the replay renders them in order (GyldAskAgent.md 11.1, and
/// the A.1 step of the plan).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnPart {
    /// A run of the answer's own prose, as the `answer` chunks joined it.
    Prose(String),
    /// A tool this turn called: the name and its input, as the `tool_call`
    /// record carried them.
    Called { name: String, input: String },
    /// What that call answered: the summary, as the `tool_result` record
    /// carried it, and whether it refused.
    Returned {
        name: String,
        ok: bool,
        summary: String,
    },
}

/// One turn of this conversation as its own records tell it: what was asked,
/// what came back, and how it closed when the close said anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// The run that answered it. Each turn keeps its own, for the audit trail.
    pub run_id: String,
    /// The question, as the reader typed it.
    pub question: String,
    /// What the turn produced, in the order it produced it: the prose the model
    /// streamed and the tools it called along the way.
    pub parts: Vec<TurnPart>,
    /// What this turn's `end` record said, when it said anything: a partial
    /// answer, a decline, a refusal, a transport failure.
    pub closed: Option<String>,
    /// The offer this turn made, when it made one: the alternative it named and
    /// the ruling it drafted, in one line.
    pub drafted: Option<String>,
}

impl Turn {
    /// The prose alone: the `answer` chunks, joined in the order they arrived.
    pub fn answer(&self) -> String {
        let mut out = String::new();
        for part in self.parts.iter() {
            if let TurnPart::Prose(text) = part {
                out.push_str(text);
            }
        }
        out
    }

    /// The assistant text this turn is replayed as.
    ///
    /// A turn that ended in anything but a clean end is replayed SAYING so: the
    /// model is reading its own transcript, and a partial answer passed off as
    /// a whole one would have it build on ground that is not there. A turn that
    /// produced no prose at all is replayed as that fact rather than as an
    /// empty assistant turn, which is not a message the API accepts.
    ///
    /// **Tool calls are replayed as TEXT, not as `tool_use` blocks.** Two
    /// reasons, both structural. The log carries what happened, not the API
    /// objects it happened in — there is no `tool_use_id` and no thinking
    /// signature on a record — and a replayed `tool_use` that could not be
    /// paired with its `tool_result` is a 400 rather than a transcript. And a
    /// replayed RESULT is a prefix ([`REPLAYED_RESULT_CHARS`]): what one call
    /// may hand a model now is the byte budget's business, and what every call
    /// of every earlier turn hands it for ever after is this one's.
    pub fn assistant(&self) -> String {
        let answer = self.answer();
        let answer = answer.trim();
        let steps = self.steps();
        let body = match (answer.is_empty(), steps.is_empty()) {
            (true, true) => String::new(),
            (true, false) => steps,
            (false, true) => answer.to_string(),
            (false, false) => format!("{answer}\n\n{steps}"),
        };
        let mut said = match (body.is_empty(), self.closed.as_deref()) {
            (true, Some(closed)) => format!("[this turn produced no answer: {closed}]"),
            (true, None) => "[this turn produced no answer]".to_string(),
            (false, Some(closed)) => format!("{body}\n\n[this turn ended: {closed}]"),
            (false, None) => body,
        };
        // The offer is part of the turn that made it. Replaying the prose alone
        // would have the model reading a transcript in which it never proposed
        // anything, and proposing the same thing again into the same window.
        if let Some(drafted) = self.drafted.as_deref() {
            said.push_str(&format!("\n\n[this turn drafted: {drafted}]"));
        }
        said
    }

    /// The tool calls and results of this turn, in order, as the lines a
    /// replayed transcript carries them on.
    fn steps(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for part in self.parts.iter() {
            match part {
                TurnPart::Prose(_) => {}
                TurnPart::Called { name, input } => {
                    lines.push(format!("[this turn called the tool `{name}` with {input}]"));
                }
                TurnPart::Returned { name, ok, summary } => {
                    let (held, cut) = capped(summary, REPLAYED_RESULT_CHARS);
                    lines.push(format!(
                        "[the tool `{name}` {}: {held}{}]",
                        if *ok { "returned" } else { "refused" },
                        if cut {
                            " …(a prefix, for replay)"
                        } else {
                            ""
                        },
                    ));
                }
            }
        }
        lines.join("\n")
    }
}

/// Fold one conversation's own records into its prior turns, oldest first.
///
/// The records arrive as the log folded them — deterministic order, this
/// supplier's own appends — and a payload that does not decode is skipped
/// rather than guessed at. Turns are grouped by `run_id` in the order their
/// first record appeared, so a turn is a turn whatever its records' seq
/// numbers are.
///
/// A turn with no `question` record is NOT replayed. It cannot be: a reply
/// with no question to pair it with is half a turn, and inventing the missing
/// half is exactly what this agent must not do.
pub fn turns(records: &[Vec<u8>]) -> Vec<Turn> {
    let mut order: Vec<String> = Vec::new();
    let mut held: HashMap<String, Turn> = HashMap::new();
    for payload in records.iter() {
        let record: GyldAskRecord = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(_) => {
                continue;
            }
        };
        let turn = match held.get_mut(&record.run_id) {
            Some(turn) => turn,
            None => {
                order.push(record.run_id.clone());
                held.entry(record.run_id.clone()).or_insert(Turn {
                    run_id: record.run_id.clone(),
                    ..Default::default()
                })
            }
        };
        match record.stream.as_str() {
            ASK_QUESTION => {
                if turn.question.is_empty() {
                    turn.question = record.line.unwrap_or_default();
                }
            }
            ASK_ANSWER => {
                let chunk = record.line.unwrap_or_default();
                // Two chunks in a row are one run of prose; a chunk after a
                // tool step opens a new one, so the order survives the fold.
                match turn.parts.last_mut() {
                    Some(TurnPart::Prose(held)) => {
                        held.push_str(&chunk);
                    }
                    _ => {
                        turn.parts.push(TurnPart::Prose(chunk));
                    }
                }
            }
            ASK_TOOL_CALL => {
                if let Some(call) = record.record.as_ref() {
                    turn.parts.push(TurnPart::Called {
                        name: text_of(call, "name"),
                        input: call
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "no input".to_string()),
                    });
                }
            }
            ASK_TOOL_RESULT => {
                if let Some(answered) = record.record.as_ref() {
                    turn.parts.push(TurnPart::Returned {
                        name: text_of(answered, "name"),
                        ok: answered
                            .get("ok")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        summary: text_of(answered, "summary"),
                    });
                }
            }
            ASK_DRAFT => {
                turn.drafted = record.record.as_ref().map(drafted);
            }
            ASK_END => {
                turn.closed = record.line;
            }
            // A `citation` is already in the stable prefix, whole and from the
            // index itself; replaying the model's turn does not need it again.
            _ => {}
        }
    }
    order
        .iter()
        .filter_map(|run_id| held.remove(run_id))
        .filter(|turn| !turn.question.trim().is_empty())
        .collect()
}

/// One string field of a record, or an empty one.
fn text_of(record: &serde_json::Value, field: &str) -> String {
    record
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// One draft record, in the one line a replayed turn carries it as. The
/// ALTERNATIVE is whatever the model named — the record never corrects it — and
/// an unresolved one says it was unresolved rather than passing as an offer the
/// envelope stands behind.
fn drafted(record: &serde_json::Value) -> String {
    let named = record
        .get("alternative")
        .and_then(|v| v.as_str())
        .unwrap_or("an alternative it did not name");
    let ruling = record
        .get("ruling_text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resolved = record
        .get("resolved")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    format!(
        "{named}{} — {ruling}",
        if resolved {
            ""
        } else {
            " (which this record does not offer)"
        }
    )
}

/// The `messages` array one turn is sent with: the prior turns in order, then
/// the question this turn asks.
///
/// **Where the breakpoint goes, and why.** The system block is the first
/// cached prefix and never moves. The second goes on the LAST PRIOR assistant
/// turn — history that is already written to the log and cannot change — and
/// NOT on the question this turn asks. Marking the question would write a cache
/// entry whose bytes end in the one thing that differs every turn; marking the
/// settled history writes an entry the next turn reads whole. The skill's
/// multi-turn pattern and its shared-prefix-varying-suffix warning agree on
/// this boundary; it is the boundary between what is settled and what is not.
///
/// At most two breakpoints are ever spent, of the four a request may carry.
pub fn messages(turns: &[Turn], question: &str, cached: bool) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        out.push(said("user", &turn.question, false));
        out.push(said(
            "assistant",
            &turn.assistant(),
            cached && i + 1 == turns.len(),
        ));
    }
    out.push(said("user", question, false));
    out
}

/// One message, as a single text block. A block rather than a bare string
/// because a block is what `cache_control` goes on.
fn said(role: &str, text: &str, cached: bool) -> serde_json::Value {
    let mut block = serde_json::json!({"type": "text", "text": text});
    if cached {
        block["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
    serde_json::json!({"role": role, "content": [block]})
}

/// What each conversation has spent, across its turns.
///
/// The per-conversation budget of section 7 is a running total of
/// `response.usage`, and this is where the running happens: one counter per
/// conversation id, held for the supplier's lifetime. It is deliberately NOT on
/// the log — a token count is the supplier's own accounting, not a fact about
/// the decision graph, and nothing a reader or a page can act on.
#[derive(Debug, Default)]
pub struct Ledger {
    spent: Mutex<HashMap<String, u64>>,
}

impl Ledger {
    /// What this conversation has spent so far. An unknown conversation has
    /// spent nothing.
    pub fn spent(&self, conversation: &str) -> u64 {
        self.held().get(conversation).copied().unwrap_or_default()
    }

    /// Add one turn's cost and answer with the new total.
    pub fn spend(&self, conversation: &str, tokens: u64) -> u64 {
        let mut held = self.held();
        let total = held.entry(conversation.to_string()).or_insert(0);
        *total = total.saturating_add(tokens);
        *total
    }

    /// The counters, through a poisoned lock as well as a healthy one: a panic
    /// somewhere else must not take the agent down with it.
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.spent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The records one turn appends, in the order the supplier appends them.
    pub(crate) fn turn(
        run_id: &str,
        question: &str,
        chunks: &[&str],
        closed: Option<&str>,
    ) -> Vec<Vec<u8>> {
        drafting(run_id, question, chunks, closed, None)
    }

    /// The same, with the `draft` record a turn that made an offer appends.
    pub(crate) fn drafting(
        run_id: &str,
        question: &str,
        chunks: &[&str],
        closed: Option<&str>,
        draft: Option<serde_json::Value>,
    ) -> Vec<Vec<u8>> {
        let who = Some("gianni".to_string());
        let conversation = "conv-tab1-key_custody-1789";
        let mut out =
            vec![
                GyldAskRecord::question(run_id, 1, &who, conversation, question.to_string())
                    .to_bytes(),
            ];
        out.push(
            GyldAskRecord::citation(
                run_id,
                2,
                &who,
                conversation,
                serde_json::json!({"tag": "Q11"}),
            )
            .to_bytes(),
        );
        for (i, chunk) in chunks.iter().enumerate() {
            out.push(
                GyldAskRecord::answer(
                    run_id,
                    3 + i as u64,
                    &who,
                    conversation,
                    (*chunk).to_string(),
                )
                .to_bytes(),
            );
        }
        if let Some(draft) = draft {
            out.push(
                GyldAskRecord::draft(run_id, 3 + chunks.len() as u64, &who, conversation, draft)
                    .to_bytes(),
            );
        }
        out.push(
            GyldAskRecord::end(
                run_id,
                4 + chunks.len() as u64,
                &who,
                conversation,
                if closed.is_some() { 1 } else { 0 },
                closed.map(|s| s.to_string()),
            )
            .to_bytes(),
        );
        out
    }

    fn two_turns() -> Vec<Vec<u8>> {
        let mut records = turn(
            "run-1",
            "why is this blocked?",
            &["It is ", "blocked."],
            None,
        );
        records.extend(turn("run-2", "by what?", &["By proof_family."], None));
        records
    }

    #[test]
    fn a_conversations_own_records_fold_back_into_its_turns_in_order() {
        let folded = turns(&two_turns());
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].run_id, "run-1");
        assert_eq!(folded[0].question, "why is this blocked?");
        assert_eq!(folded[0].answer(), "It is blocked.", "the chunks rejoin");
        assert_eq!(folded[0].closed, None);
        assert_eq!(folded[1].run_id, "run-2");
        assert_eq!(folded[1].question, "by what?");
        assert_eq!(folded[1].answer(), "By proof_family.");
    }

    #[test]
    fn a_payload_that_does_not_decode_is_skipped_and_the_rest_still_fold() {
        let mut records = two_turns();
        records.insert(2, b"not a record at all".to_vec());
        let folded = turns(&records);
        assert_eq!(folded.len(), 2, "{folded:?}");
        assert_eq!(folded[0].answer(), "It is blocked.");
    }

    #[test]
    fn a_turn_with_no_question_is_not_replayed_as_half_a_turn() {
        let mut records = turn("run-1", "why?", &["Because."], None);
        records.remove(0);
        assert!(turns(&records).is_empty(), "half a turn is not a turn");
    }

    #[test]
    fn a_turn_that_did_not_end_clean_is_replayed_saying_so() {
        let partial = turn(
            "run-1",
            "why?",
            &["It is blo"],
            Some("the answer stopped at the output budget of 64000 tokens and is partial"),
        );
        let folded = turns(&partial);
        let said = folded[0].assistant();
        assert!(said.starts_with("It is blo"), "the partial text is KEPT");
        assert!(said.contains("[this turn ended:"), "{said}");
        assert!(said.contains("is partial"), "{said}");

        let refused = turn("run-1", "why?", &[], Some("connection reset"));
        let folded = turns(&refused);
        assert_eq!(
            folded[0].assistant(),
            "[this turn produced no answer: connection reset]",
            "never an empty assistant turn"
        );

        let silent = Turn {
            run_id: "run-9".into(),
            question: "why?".into(),
            ..Default::default()
        };
        assert_eq!(silent.assistant(), "[this turn produced no answer]");
    }

    #[test]
    fn the_first_turn_carries_the_question_alone_and_marks_nothing() {
        let messages = messages(&[], "why is this blocked?", true);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "why is this blocked?");
        assert!(
            messages[0]["content"][0].get("cache_control").is_none(),
            "the question is this turn's volatile tail and is never the breakpoint"
        );
    }

    #[test]
    fn a_follow_up_replays_every_prior_turn_in_order_and_caches_the_settled_history() {
        let prior = turns(&two_turns());
        let messages = messages(&prior, "and what unlocks it?", true);
        assert_eq!(
            messages
                .iter()
                .map(|m| m["role"].as_str().unwrap_or(""))
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "user", "assistant", "user"],
            "two prior turns, then this one"
        );
        assert_eq!(messages[0]["content"][0]["text"], "why is this blocked?");
        assert_eq!(messages[1]["content"][0]["text"], "It is blocked.");
        assert_eq!(messages[2]["content"][0]["text"], "by what?");
        assert_eq!(messages[3]["content"][0]["text"], "By proof_family.");
        assert_eq!(messages[4]["content"][0]["text"], "and what unlocks it?");

        // ONE breakpoint in the messages, on the last SETTLED block.
        let marked: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m["content"][0].get("cache_control").is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, vec![3], "{messages:?}");
        assert_eq!(
            messages[3]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );

        // An endpoint that rejects `cache_control` gets the same transcript
        // with no breakpoint in it: the history is unchanged, the marker is
        // what goes.
        let plain = super::messages(&prior, "and what unlocks it?", false);
        assert!(
            plain
                .iter()
                .all(|m| m["content"][0].get("cache_control").is_none()),
            "{plain:?}"
        );
        assert_eq!(
            plain
                .iter()
                .map(|m| m["content"][0]["text"].clone())
                .collect::<Vec<_>>(),
            messages
                .iter()
                .map(|m| m["content"][0]["text"].clone())
                .collect::<Vec<_>>(),
            "only the marker differs"
        );
    }

    #[test]
    fn a_turn_that_made_an_offer_is_replayed_with_it() {
        let records = drafting(
            "run-1",
            "what should we do?",
            &["Two are offered."],
            None,
            Some(serde_json::json!({
                "slot": "glade_decisions:GladeDecisions.key_custody",
                "alternative": "a1",
                "alternative_slot": "a1",
                "ruling_text": "2026-09-16, owner: gianni: keys stay on device.",
                "resolved": true
            })),
        );
        let folded = turns(&records);
        let said = folded[0].assistant();
        assert!(said.starts_with("Two are offered."), "{said}");
        assert!(
            said.contains("[this turn drafted: a1 — 2026-09-16, owner:"),
            "{said}"
        );
        assert!(
            !said.contains("does not offer"),
            "a resolved draft is not flagged: {said}"
        );

        // An unresolved offer is replayed AS unresolved, never as one the
        // record stands behind.
        let records = drafting(
            "run-2",
            "what about hsm?",
            &["Not offered."],
            None,
            Some(serde_json::json!({
                "alternative": "hsm",
                "ruling_text": "2026-09-16, owner: gianni: use an HSM.",
                "resolved": false
            })),
        );
        let said = turns(&records)[0].assistant();
        assert!(
            said.contains("hsm (which this record does not offer)"),
            "{said}"
        );
    }

    /// The records a turn that used a tool appends, in the supplier's order:
    /// the question, the citations, prose, the call, its result, more prose.
    fn with_a_tool(run_id: &str, ok: bool, summary: &str) -> Vec<Vec<u8>> {
        let who = Some("gianni".to_string());
        let conversation = "conv-tab1-key_custody-1789";
        let mut out = vec![GyldAskRecord::question(
            run_id,
            1,
            &who,
            conversation,
            "what did stream-a decide?".into(),
        )
        .to_bytes()];
        out.push(
            GyldAskRecord::answer(run_id, 2, &who, conversation, "Let me look. ".into()).to_bytes(),
        );
        out.push(
            GyldAskRecord::tool_call(
                run_id,
                3,
                &who,
                conversation,
                serde_json::json!({"id": "toolu_1", "name": "gyld_query",
                                   "input": {"kind": "rulings", "slot": "s"}}),
            )
            .to_bytes(),
        );
        out.push(
            GyldAskRecord::tool_result(
                run_id,
                4,
                &who,
                conversation,
                serde_json::json!({"id": "toolu_1", "name": "gyld_query", "ok": ok,
                                   "summary": summary, "bytes": summary.len(),
                                   "truncated": false}),
            )
            .to_bytes(),
        );
        out.push(
            GyldAskRecord::answer(run_id, 5, &who, conversation, "stream-a took 1.2.0.".into())
                .to_bytes(),
        );
        out.push(GyldAskRecord::end(run_id, 6, &who, conversation, 0, None).to_bytes());
        out
    }

    #[test]
    fn a_turn_that_used_a_tool_folds_into_its_parts_in_the_order_they_happened() {
        let folded = turns(&with_a_tool("run-1", true, "the version_pin ruling"));
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].parts,
            vec![
                TurnPart::Prose("Let me look. ".into()),
                TurnPart::Called {
                    name: "gyld_query".into(),
                    input: "{\"kind\":\"rulings\",\"slot\":\"s\"}".into(),
                },
                TurnPart::Returned {
                    name: "gyld_query".into(),
                    ok: true,
                    summary: "the version_pin ruling".into(),
                },
                TurnPart::Prose("stream-a took 1.2.0.".into()),
            ],
        );
        assert_eq!(
            folded[0].answer(),
            "Let me look. stream-a took 1.2.0.",
            "the prose alone is still the prose alone"
        );
    }

    #[test]
    fn a_follow_up_replays_the_tool_calls_and_their_results() {
        let folded = turns(&with_a_tool("run-1", true, "the version_pin ruling"));
        let said = folded[0].assistant();
        assert!(
            said.starts_with("Let me look. stream-a took 1.2.0."),
            "{said}"
        );
        assert!(
            said.contains("[this turn called the tool `gyld_query` with {\"kind\":\"rulings\""),
            "{said}"
        );
        assert!(
            said.contains("[the tool `gyld_query` returned: the version_pin ruling]"),
            "{said}"
        );
        let at_call = said.find("called the tool").expect("the call");
        let at_result = said.find("returned:").expect("the result");
        assert!(at_call < at_result, "in the order they happened: {said}");

        // A refusal is replayed AS a refusal: a follow-up that read "returned"
        // over a tool that would not run would build on ground that is not
        // there.
        let refused = turns(&with_a_tool("run-2", false, "no such stream"))[0].assistant();
        assert!(
            refused.contains("[the tool `gyld_query` refused: no such stream]"),
            "{refused}"
        );
    }

    #[test]
    fn a_replayed_result_is_a_prefix_and_says_so() {
        let long = "x".repeat(REPLAYED_RESULT_CHARS + 500);
        let said = turns(&with_a_tool("run-1", true, &long))[0].assistant();
        assert!(said.contains("…(a prefix, for replay)"), "{said}");
        assert!(
            !said.contains(&"x".repeat(REPLAYED_RESULT_CHARS + 1)),
            "a whole conversation of whole results would spend the input budget re-reading"
        );
    }

    #[test]
    fn the_ledger_runs_a_total_per_conversation() {
        let ledger = Ledger::default();
        assert_eq!(ledger.spent("conv-a"), 0, "an unknown conversation is zero");
        assert_eq!(ledger.spend("conv-a", 1200), 1200);
        assert_eq!(ledger.spend("conv-a", 800), 2000);
        assert_eq!(ledger.spent("conv-a"), 2000);
        assert_eq!(ledger.spent("conv-b"), 0, "one total per conversation");
        assert_eq!(ledger.spend("conv-a", u64::MAX), u64::MAX, "no overflow");
    }
}
