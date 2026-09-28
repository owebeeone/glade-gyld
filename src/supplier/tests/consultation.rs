use super::*;

#[test]
fn grounding_resolves_the_cited_tags_and_refuses_a_missing_index_as_data() {
    let dir = root("ground");
    let index = dir.join("sources.json");
    let context =
        ask::AskContext::parse(Some(&ask::tests::envelope())).expect("the fixture envelope");
    let consult = Consultation {
        context,
        sources: index.clone(),
        conversation: "conv-tab1-key_custody-1789".into(),
    };

    // No index: a readable refusal, and nothing composed.
    let e = ground(&consult).unwrap_err();
    assert!(e.contains("cannot read the source index"), "{e}");

    std::fs::write(
        &index,
        serde_json::to_vec(&sources::tests::index()).unwrap(),
    )
    .unwrap();
    let (resolved, prompt) = ground(&consult).expect("grounding");
    assert_eq!(sources::counted(&resolved), (1, 3));
    assert!(
        prompt.system.contains("| Q11 | Key custody"),
        "the index's own passage is the quotable material"
    );
    assert!(
        prompt
            .system
            .contains("UNRESOLVED: no document in this index declares it"),
        "an unresolved tag reaches the prompt, with its reason"
    );
    assert_eq!(prompt.user, "why is this blocked?");
    let _ = std::fs::remove_dir_all(&dir);
}
