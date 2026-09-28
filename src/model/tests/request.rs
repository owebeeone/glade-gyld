use super::*;

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
        let mut records =
            crate::conversation::tests::turn("run-1", "why is this blocked?", &["It is."], None);
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
fn the_draft_tool_is_declared_on_every_request_and_sits_in_the_cached_prefix() {
    let body = request().body(true);
    let tools = body["tools"].as_array().expect("one tool");
    assert_eq!(tools.len(), 1, "one tool, and only one");
    assert_eq!(tools[0]["name"], DRAFT_TOOL);
    assert_eq!(
        tools[0]["strict"], true,
        "strict is what guarantees the input validates"
    );
    assert_eq!(tools[0]["input_schema"]["additionalProperties"], false);
    assert_eq!(
        tools[0]["input_schema"]["required"],
        serde_json::json!(["alternative", "ruling_text", "sources"])
    );
    assert!(
        body.get("tool_choice").is_none(),
        "auto: an agent that must always propose is an agent that rules"
    );
    assert!(
        tools[0].get("eager_input_streaming").is_none(),
        "a draft is small and arrives whole or not at all"
    );
    assert!(
        body.get("output_config").is_none(),
        "the response is PROSE with an offer beside it, not one JSON document"
    );

    // Tools render before the system block, so they are part of what the
    // system breakpoint caches — and part of what must not move.
    let prefix = request().cached_prefix();
    assert_eq!(prefix["tools"], body["tools"]);
    assert_eq!(request().count_body()["tools"], body["tools"]);
}

#[test]
fn a_degraded_shape_drops_strict_and_the_breakpoints_and_changes_nothing_else() {
    let mut request = request();
    request.turns = crate::conversation::turns(&crate::conversation::tests::turn(
        "run-1",
        "why is this blocked?",
        &["It is."],
        None,
    ));
    let full = request.body(true);
    assert_eq!(full["tools"][0]["strict"], true);
    assert_eq!(
        serde_json::to_string(&full)
            .unwrap()
            .matches("cache_control")
            .count(),
        2
    );

    request.config.shape = Shape::full().without_strict();
    let lax = request.body(true);
    assert!(
        lax["tools"][0].get("strict").is_none(),
        "an endpoint that rejects `strict` gets the tool without it"
    );
    assert_eq!(
        lax["tools"][0]["input_schema"], full["tools"][0]["input_schema"],
        "the schema itself is untouched: only the guarantee goes"
    );
    assert_eq!(lax["messages"], full["messages"]);

    request.config.shape = Shape::full().without_cache_control();
    let uncached = request.body(true);
    assert_eq!(
        serde_json::to_string(&uncached)
            .unwrap()
            .matches("cache_control")
            .count(),
        0,
        "no breakpoint survives, in the system block or the history"
    );
    assert_eq!(uncached["tools"][0]["strict"], true, "the other stays on");
    assert_eq!(
        uncached["system"][0]["text"], full["system"][0]["text"],
        "the stance and the passages are the same bytes"
    );
    assert_eq!(uncached["max_tokens"], full["max_tokens"]);

    // The count body degrades with it: counting a body that is not the one
    // about to be sent is not a budget.
    request.config.shape = Shape {
        strict: false,
        cache_control: false,
    };
    let counted = request.count_body();
    assert!(counted["tools"][0].get("strict").is_none());
    assert_eq!(
        serde_json::to_string(&counted)
            .unwrap()
            .matches("cache_control")
            .count(),
        0
    );
}
