use super::*;

#[test]
fn the_conversation_budget_refuses_the_turn_that_would_cross_it() {
    let mut request = request();
    request.config.max_conversation_tokens = 5_000;
    let client = Scripted {
        counted: Ok(1_200),
        transcript: NORMAL.into(),
        ..Default::default()
    };

    // Room for it: the turn goes.
    let outcome = consult(&client, &request, 3_000, &none(), &mut |_| {}).expect("within budget");
    assert!(outcome.complete());

    // One more turn would cross it, so nothing is sent.
    let e = consult(&client, &request, 3_900, &none(), &mut |_| {}).expect_err("a refusal");
    assert_eq!(
        e,
        AskRefusal::OverConversationBudget {
            spent: 3_900,
            counted: 1_200,
            budget: 5_000
        }
    );
    let said = e.says();
    assert!(
        said.contains("3900") && said.contains("1200") && said.contains("5000"),
        "{said}"
    );
    assert!(said.contains("--agent-max-conversation-tokens"), "{said}");
    assert_eq!(
        client.count(),
        2,
        "the count happened, the second call did not"
    );

    // Zero is no ceiling.
    request.config.max_conversation_tokens = 0;
    assert!(consult(&client, &request, u64::MAX, &none(), &mut |_| {}).is_ok());
}

#[test]
fn the_ladder_drops_one_feature_at_a_time_and_then_runs_out() {
    // Named: it goes straight to the rung the endpoint blamed.
    let (shape, note) =
        degrade(Shape::full(), "system.0: unexpected field `cache_control`").expect("a rung");
    assert_eq!(
        shape,
        Shape::full().without_cache_control(),
        "`strict` was not what it complained about"
    );
    assert!(
        note.contains("`cache_control`") && note.contains("400"),
        "{note}"
    );
    assert!(note.contains("pays for its passages"), "{note}");

    // Unnamed: the rungs are taken in order, cheapest loss first.
    let (shape, note) = degrade(Shape::full(), "invalid request").expect("a rung");
    assert_eq!(shape, Shape::full().without_strict());
    assert!(note.contains("`strict`"), "{note}");
    assert!(note.contains("checked by this supplier"), "{note}");

    let (shape, _) = degrade(shape, "invalid request").expect("the second rung");
    assert_eq!(
        shape,
        Shape {
            strict: false,
            cache_control: false
        }
    );

    // And then there is nothing left to drop: a 400 is the answer.
    assert_eq!(degrade(shape, "invalid request"), None);
    assert_eq!(degrade(shape, "unexpected field `strict`"), None);

    // A rung already taken is not taken twice.
    let lax = Shape::full().without_strict();
    let (shape, note) = degrade(lax, "unexpected field `strict`").expect("a rung");
    assert_eq!(
        shape,
        Shape {
            strict: false,
            cache_control: false
        }
    );
    assert!(note.contains("`cache_control`"), "{note}");

    // An endpoint's own words reach the note, bounded and on one line.
    let (_, note) = degrade(Shape::full(), &format!("a\n{}", "x".repeat(900))).expect("a rung");
    assert!(!note.contains('\n'), "{note}");
    assert!(note.len() < 500, "{} chars", note.len());
}

#[test]
fn an_estimate_is_of_the_body_about_to_be_sent_and_grows_with_it() {
    let first = request();
    let small = estimate_tokens(&first);
    assert!(small > 0);

    let mut bigger = request();
    bigger.prompt.system = format!("{} {}", first.prompt.system, "passage ".repeat(1000));
    assert!(
        estimate_tokens(&bigger) > small + 1000,
        "it moves with the prompt it is an estimate of"
    );

    // Pessimistic on purpose: an estimate that under-counts is not a bound.
    let body = serde_json::to_string(&first.count_body()).unwrap();
    assert_eq!(small, (body.chars().count() as u64).div_ceil(3));
    assert!(small >= (body.chars().count() as u64) / 4);
}

#[test]
fn the_input_budget_refuses_before_the_call_and_costs_nothing() {
    let mut request = request();
    request.config.max_input_tokens = 1000;
    let client = Scripted {
        counted: Ok(1001),
        transcript: NORMAL.into(),
        ..Default::default()
    };
    let e = consult(&client, &request, 0, &none(), &mut |_| {}).expect_err("a refusal");
    let said = e.says();
    assert!(said.contains("1001") && said.contains("1000"), "{said}");
    assert_eq!(client.count(), 1, "the count happened, the call did not");

    request.config.max_input_tokens = 1001;
    let outcome = consult(&client, &request, 0, &none(), &mut |_| {}).expect("within budget");
    assert!(outcome.complete());
}

#[test]
fn a_transport_failure_is_data_not_a_panic() {
    let client = Scripted {
        counted: Ok(10),
        transport: Some("connection reset".into()),
        ..Default::default()
    };
    let e = consult(&client, &request(), 0, &none(), &mut |_| {}).expect_err("a refusal");
    assert!(e.says().contains("connection reset"), "{e}");

    let counting = Scripted {
        counted: Err("dns failure".into()),
        ..Default::default()
    };
    let e = consult(&counting, &request(), 0, &none(), &mut |_| {}).expect_err("a refusal");
    assert!(e.says().contains("dns failure"), "{e}");
}
