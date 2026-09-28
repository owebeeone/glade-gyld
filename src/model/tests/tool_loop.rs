use super::*;

// ---- The tool loop (GyldAskAgent.md section 11.1, plan step A.1) -------

/// One SSE body out of the events it carries.
fn sse(events: Vec<serde_json::Value>) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// A turn that says something and then asks for a tool.
fn calling(text: &str, calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut events = vec![
        serde_json::json!({"type": "message_start",
                           "message": {"usage": {"input_tokens": 100}}}),
        serde_json::json!({"type": "content_block_start", "index": 0,
                           "content_block": {"type": "text", "text": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "text_delta", "text": text}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    ];
    for (at, (id, name, input)) in calls.iter().enumerate() {
        let index = at + 1;
        events.push(serde_json::json!({
            "type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        events.push(serde_json::json!({
            "type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}));
        events.push(serde_json::json!({"type": "content_block_stop", "index": index}));
    }
    events.push(serde_json::json!({"type": "message_delta",
                                   "delta": {"stop_reason": TOOL_USE},
                                   "usage": {"output_tokens": 20}}));
    events.push(serde_json::json!({"type": "message_stop"}));
    sse(events)
}

/// A turn that answers and ends.
fn answering(text: &str) -> String {
    sse(vec![
        serde_json::json!({"type": "message_start",
                           "message": {"usage": {"input_tokens": 300}}}),
        serde_json::json!({"type": "content_block_start", "index": 0,
                           "content_block": {"type": "text", "text": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "text_delta", "text": text}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta",
                           "delta": {"stop_reason": END_TURN},
                           "usage": {"output_tokens": 40}}),
        serde_json::json!({"type": "message_stop"}),
    ])
}

/// Everything one consultation produced, in the order it produced it.
fn ran(client: &Scripted, tools: &crate::tools::ToolRegistry) -> (Vec<ModelEvent>, ModelOutcome) {
    let mut events: Vec<ModelEvent> = Vec::new();
    let outcome = consult(client, &request(), 0, tools, &mut |event| {
        events.push(event);
    })
    .expect("the turn ran");
    (events, outcome)
}

fn calls_of(events: &[ModelEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn results_of(events: &[ModelEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::ToolResult(answered) => Some(answered.clone()),
            _ => None,
        })
        .collect()
}

fn notes_of(events: &[ModelEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::Note(note) => Some(note.clone()),
            _ => None,
        })
        .collect()
}

fn reading(text: &str) -> crate::tools::ToolRegistry {
    crate::tools::ToolRegistry::of(
        vec![crate::tools::tests::Scripted::answering(
            "read_source",
            text,
        )],
        crate::tools::ToolBudgets::default(),
    )
}

#[test]
fn a_two_step_turn_runs_the_tool_and_answers_with_what_it_returned() {
    let client = Scripted::replaying(&[
        &calling(
            "Let me read Q11. ",
            &[("toolu_1", "read_source", serde_json::json!({"tag": "Q11"}))],
        ),
        &answering("Q11 says key custody is a buy."),
    ]);
    let (events, outcome) = ran(&client, &reading("| Q11 | Key custody | buy |"));

    assert_eq!(
        outcome.stop_reason, END_TURN,
        "the turn ended on the ANSWER"
    );
    assert!(outcome.complete() && outcome.exit() == 0);
    assert_eq!(
        outcome.input_tokens, 400,
        "a multi-step turn costs the SUM of its steps"
    );
    assert_eq!(outcome.output_tokens, 60);

    // The call and its result are both on the run, in the order they
    // happened and around the prose they sit between.
    let calls = calls_of(&events);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "read_source");
    assert_eq!(calls[0]["id"], "toolu_1");
    assert_eq!(calls[0]["input"]["tag"], "Q11", "the input, whole");
    let results = results_of(&events);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[0]["id"], "toolu_1", "paired by identity, not order");
    assert_eq!(results[0]["summary"], "| Q11 | Key custody | buy |");
    assert!(
        notes_of(&events).is_empty(),
        "nothing had to be done differently"
    );

    // The SECOND request carries the first step verbatim: the assistant
    // turn as the API produced it, then one user turn answering its call.
    let sent = client.requests();
    assert_eq!(sent.len(), 2, "counted once per step, and streamed twice");
    assert!(sent[0].steps.is_empty(), "the first step starts clean");
    let steps = &sent[1].steps;
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert_eq!(steps[0]["role"], "assistant");
    assert_eq!(steps[0]["content"][0]["text"], "Let me read Q11. ");
    assert_eq!(
        steps[0]["content"][1],
        serde_json::json!({
            "type": "tool_use", "id": "toolu_1", "name": "read_source",
            "input": {"tag": "Q11"}
        }),
        "the tool_use block goes back whole, or the call is unanswered"
    );
    assert_eq!(steps[1]["role"], "user");
    assert_eq!(steps[1]["content"][0]["type"], "tool_result");
    assert_eq!(steps[1]["content"][0]["tool_use_id"], "toolu_1");
    let content = steps[1]["content"][0]["content"].as_str().unwrap_or("");
    assert!(
        content.contains("retrieved material, not an"),
        "a result arrives wrapped as DATA: {content}"
    );
    assert!(
        content.ends_with("| Q11 | Key custody | buy |"),
        "{content}"
    );

    // And the prompt, the tools and the prior turns are untouched by the
    // step, so the cached prefix is the same bytes on both calls.
    assert_eq!(sent[0].cached_prefix(), sent[1].cached_prefix());
}

#[test]
fn a_tool_that_refuses_is_answered_as_an_error_and_the_turn_goes_on() {
    let held = crate::tools::ToolRegistry::of(
        vec![crate::tools::tests::Scripted::refusing(
            "read_source",
            "this build's index does not list \"ZZ-9\"; it lists Q11",
        )],
        crate::tools::ToolBudgets::default(),
    );
    let client = Scripted::replaying(&[
        &calling(
            "",
            &[("toolu_2", "read_source", serde_json::json!({"tag": "ZZ-9"}))],
        ),
        &answering("The index resolves ZZ-9 to nothing."),
    ]);
    let (events, outcome) = ran(&client, &held);

    assert_eq!(
        outcome.stop_reason, END_TURN,
        "a refusal is not the end of a turn"
    );
    let results = results_of(&events);
    assert_eq!(results[0]["ok"], false);
    assert!(
        results[0]["summary"]
            .as_str()
            .unwrap_or("")
            .contains("it lists Q11"),
        "the refusal names what it DOES know: {results:?}"
    );
    let answered = &client.requests()[1].steps[1]["content"][0];
    assert_eq!(answered["is_error"], true, "{answered}");
    assert_eq!(
        answered["content"], "this build's index does not list \"ZZ-9\"; it lists Q11",
        "a refusal is this supplier's own sentence, unwrapped"
    );
}

#[test]
fn the_step_budget_stops_the_loop_as_data_and_the_turn_still_ends_cleanly() {
    let held = crate::tools::ToolRegistry::of(
        vec![crate::tools::tests::Scripted::answering(
            "read_source",
            "a passage",
        )],
        crate::tools::ToolBudgets {
            steps: 1,
            ..Default::default()
        },
    );
    // A model that asks for the same tool for ever.
    let client = Scripted {
        transcript: calling(
            "again. ",
            &[("toolu_3", "read_source", serde_json::json!({"tag": "Q11"}))],
        ),
        ..Default::default()
    };
    let (events, outcome) = ran(&client, &held);

    assert_eq!(client.count(), 2, "one step, then the budget");
    assert_eq!(calls_of(&events).len(), 1, "one tool was RUN");
    let notes = notes_of(&events);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("per-turn budget"), "{notes:?}");
    assert!(notes[0].contains("1 tool step"), "{notes:?}");
    assert!(
        outcome.complete() && outcome.exit() == 0,
        "the budget is a fact about the run, not a failure of it: {outcome:?}"
    );
    assert_eq!(
        outcome.says(64_000),
        None,
        "the close says nothing; the note already said it"
    );
}

#[test]
fn a_result_over_the_byte_cap_is_cut_marked_and_said_to_be_a_prefix() {
    let held = crate::tools::ToolRegistry::of(
        vec![crate::tools::tests::Scripted::answering(
            "read_source",
            &"p".repeat(200),
        )],
        crate::tools::ToolBudgets {
            bytes: 20,
            ..Default::default()
        },
    );
    let client = Scripted::replaying(&[
        &calling("", &[("toolu_4", "read_source", serde_json::json!({}))]),
        &answering("done"),
    ]);
    let (events, _) = ran(&client, &held);
    let results = results_of(&events);
    assert_eq!(results[0]["truncated"], true);
    assert_eq!(results[0]["bytes"], 200, "the size BEFORE the cut");
    assert_eq!(results[0]["summary"], "p".repeat(20));
    let content = client.requests()[1].steps[1]["content"][0]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        content.contains("capped") && content.contains("a prefix"),
        "{content}"
    );
}

#[test]
fn a_tool_the_allow_list_does_not_carry_is_refused_as_data_without_running() {
    let client = Scripted::replaying(&[
        &calling(
            "",
            &[(
                "toolu_5",
                "fetch_url",
                serde_json::json!({"url": "https://example.test/"}),
            )],
        ),
        &answering("I cannot reach a page from here."),
    ]);
    let (events, outcome) = ran(&client, &reading("never asked for"));

    assert_eq!(outcome.stop_reason, END_TURN);
    let results = results_of(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], false);
    let said = results[0]["summary"].as_str().unwrap_or("").to_string();
    assert!(said.contains("no tool \"fetch_url\" is enabled"), "{said}");
    assert!(
        said.contains("read_source"),
        "it names what IS enabled: {said}"
    );
    assert_eq!(
        client.requests()[1].steps[1]["content"][0]["is_error"],
        true,
        "the call is ANSWERED, because an unanswered tool_use is a 400"
    );
}

#[test]
fn a_turn_whose_only_call_is_the_draft_still_ends_and_one_beside_a_read_does_not() {
    // The draft alone: the offer IS the ending, and no tool runs (section 8).
    let only = Scripted {
        transcript: calling(
            "I propose owner_held_only. ",
            &[(
                "toolu_6",
                DRAFT_TOOL,
                serde_json::json!({"alternative": "a1", "ruling_text": "x", "sources": []}),
            )],
        ),
        ..Default::default()
    };
    let (events, outcome) = ran(&only, &reading("a passage"));
    assert_eq!(only.count(), 1, "one call, and no second");
    assert_eq!(outcome.stop_reason, TOOL_USE);
    assert!(outcome.complete());
    assert!(
        calls_of(&events).is_empty(),
        "the supplier never runs the draft tool"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ModelEvent::Draft(_)))
            .count(),
        1,
        "and the draft still arrives"
    );

    // Beside a read, the turn continues — and the draft's own call is
    // answered too, because every tool_use of a replayed turn must be.
    let beside = Scripted::replaying(&[
        &calling(
            "",
            &[
                ("toolu_7", "read_source", serde_json::json!({"tag": "Q11"})),
                (
                    "toolu_8",
                    DRAFT_TOOL,
                    serde_json::json!({"alternative": "a1", "ruling_text": "x", "sources": []}),
                ),
            ],
        ),
        &answering("and that is why."),
    ]);
    let (events, outcome) = ran(&beside, &reading("a passage"));
    assert_eq!(outcome.stop_reason, END_TURN);
    assert_eq!(calls_of(&events).len(), 1, "only the read is run");
    let answers = beside.requests()[1].steps[1]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(answers.len(), 2, "both calls are answered: {answers:?}");
    let ids: Vec<&str> = answers
        .iter()
        .map(|a| a["tool_use_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["toolu_7", "toolu_8"]);
    assert!(
        answers[1]["content"]
            .as_str()
            .unwrap_or("")
            .contains("not a ruling"),
        "{answers:?}"
    );
}

#[test]
fn a_thinking_block_is_kept_whole_for_replay_and_never_reaches_a_record() {
    let transcript = sse(vec![
        serde_json::json!({"type": "message_start", "message": {"usage": {}}}),
        serde_json::json!({"type": "content_block_start", "index": 0,
                           "content_block": {"type": "thinking", "thinking": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "thinking_delta", "thinking": "weighing it"}}),
        serde_json::json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "content_block_start", "index": 1,
                           "content_block": {"type": "tool_use", "id": "toolu_9",
                                             "name": "read_source", "input": {}}}),
        serde_json::json!({"type": "content_block_delta", "index": 1,
                           "delta": {"type": "input_json_delta",
                                     "partial_json": "{\"tag\":\"Q11\"}"}}),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": TOOL_USE}}),
    ]);
    let client = Scripted::replaying(&[&transcript, &answering("read.")]);
    let (events, _) = ran(&client, &reading("a passage"));

    assert!(
        !events.iter().any(|event| matches!(
            event, ModelEvent::Text(chunk) if chunk.contains("weighing")
        )),
        "reasoning is not text and reaches no record: {events:?}"
    );
    let replayed = &client.requests()[1].steps[0]["content"][0];
    assert_eq!(
        replayed,
        &serde_json::json!({
            "type": "thinking", "thinking": "weighing it", "signature": "sig-abc"
        }),
        "the block goes back exactly as it arrived, signature and all"
    );
}
