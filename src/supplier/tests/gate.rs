use super::*;

/// Writing verbs run ONE AT A TIME. Without this a second `answer` arriving
/// mid-run would write its notebook and the first run's refusal would put the
/// FIRST notebook back over it.
#[test]
fn one_writing_verb_at_a_time_and_the_second_is_told_which() {
    let gate = Arc::new(WriteGate::default());
    let held = WriteGate::try_hold(&gate, "run-1").expect("a free gate is taken");

    let said = WriteGate::try_hold(&gate, "run-2").expect_err("the second is refused");
    assert!(
        said.contains("already in flight") && said.contains("run run-1"),
        "the refusal names the run to wait for: {said}"
    );

    // The run has settled: the next write may go, and the gate is free again.
    drop(held);
    let next = WriteGate::try_hold(&gate, "run-2").expect("the gate is free");
    drop(next);
    WriteGate::try_hold(&gate, "run-3").expect("and free again");
}
