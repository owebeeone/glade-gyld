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

use crate::envelope::{GyldAskRecord, ASK_ANSWER, ASK_END, ASK_QUESTION};

/// One turn of this conversation as its own records tell it: what was asked,
/// what came back, and how it closed when the close said anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// The run that answered it. Each turn keeps its own, for the audit trail.
    pub run_id: String,
    /// The question, as the reader typed it.
    pub question: String,
    /// The prose, as the model streamed it — the `answer` chunks joined in the
    /// order they were appended.
    pub answer: String,
    /// What this turn's `end` record said, when it said anything: a partial
    /// answer, a decline, a refusal, a transport failure.
    pub closed: Option<String>,
}

impl Turn {
    /// The assistant text this turn is replayed as.
    ///
    /// A turn that ended in anything but a clean end is replayed SAYING so: the
    /// model is reading its own transcript, and a partial answer passed off as
    /// a whole one would have it build on ground that is not there. A turn that
    /// produced no prose at all is replayed as that fact rather than as an
    /// empty assistant turn, which is not a message the API accepts.
    pub fn assistant(&self) -> String {
        let answer = self.answer.trim();
        match (answer.is_empty(), self.closed.as_deref()) {
            (true, Some(said)) => format!("[this turn produced no answer: {said}]"),
            (true, None) => "[this turn produced no answer]".to_string(),
            (false, Some(said)) => format!("{answer}\n\n[this turn ended: {said}]"),
            (false, None) => answer.to_string(),
        }
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
                turn.answer.push_str(record.line.as_deref().unwrap_or(""));
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
pub fn messages(turns: &[Turn], question: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        out.push(said("user", &turn.question, false));
        out.push(said("assistant", &turn.assistant(), i + 1 == turns.len()));
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
        out.push(
            GyldAskRecord::end(
                run_id,
                3 + chunks.len() as u64,
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
        assert_eq!(folded[0].answer, "It is blocked.", "the chunks rejoin");
        assert_eq!(folded[0].closed, None);
        assert_eq!(folded[1].run_id, "run-2");
        assert_eq!(folded[1].question, "by what?");
        assert_eq!(folded[1].answer, "By proof_family.");
    }

    #[test]
    fn a_payload_that_does_not_decode_is_skipped_and_the_rest_still_fold() {
        let mut records = two_turns();
        records.insert(2, b"not a record at all".to_vec());
        let folded = turns(&records);
        assert_eq!(folded.len(), 2, "{folded:?}");
        assert_eq!(folded[0].answer, "It is blocked.");
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
        let messages = messages(&[], "why is this blocked?");
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
        let messages = messages(&prior, "and what unlocks it?");
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
