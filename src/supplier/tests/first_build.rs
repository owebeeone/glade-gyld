use super::*;

#[test]
fn the_no_bundle_refusal_names_the_first_build_while_it_is_running() {
    let runs = Runs::new();
    let first = FirstBuild::new(runs.boot());
    // Before it starts, and after it ends, the plain refusal stands: there
    // really is no bundle and nothing is coming.
    assert_eq!(first.explain(verbs::NO_BUNDLE.into()), verbs::NO_BUNDLE);

    first.begin();
    let said = first.explain(verbs::NO_BUNDLE.into());
    assert!(
        said.contains("the first build is in progress")
            && said.contains(&format!("run {}", runs.boot())),
        "{said}"
    );
    assert_ne!(said, verbs::NO_BUNDLE);

    // Every other refusal passes through untouched, running or not.
    let other = "verb `forall` not in the allow-list".to_string();
    assert_eq!(first.explain(other.clone()), other);

    first.ended();
    assert_eq!(first.explain(verbs::NO_BUNDLE.into()), verbs::NO_BUNDLE);
}
