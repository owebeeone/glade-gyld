use super::*;

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
