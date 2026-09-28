use super::*;

#[test]
fn a_bundle_root_that_already_holds_a_build_is_published_at_attach() {
    let bundle = root("has-build");
    let layout = Layout::new(bundle.join("gyld"), bundle.clone());
    let build = layout.new_build_dir("build-0000000000001");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(build.join("streams.json"), "{}").unwrap();

    // No pointer yet — the hand-seeded case. The build is still the build.
    assert_eq!(at_attach(&layout), AtAttach::Publish(build.clone()));

    bundle::write_latest(&layout, &build).unwrap();
    assert_eq!(at_attach(&layout), AtAttach::Publish(build.clone()));
    assert_eq!(named(&layout, &build), "builds/build-0000000000001");
    let _ = std::fs::remove_dir_all(&bundle);
}

#[test]
fn a_bundle_root_with_no_build_is_bootstrapped() {
    let bundle = root("no-build");
    let layout = Layout::new(bundle.join("gyld"), bundle.clone());
    assert_eq!(at_attach(&layout), AtAttach::Bootstrap);

    // A build directory with no `streams.json` in it is not a build: the
    // pointer names it, and it is still nothing to publish.
    let empty = layout.new_build_dir("build-0000000000002");
    std::fs::create_dir_all(&empty).unwrap();
    bundle::write_latest(&layout, &empty).unwrap();
    assert_eq!(at_attach(&layout), AtAttach::Bootstrap);
    let _ = std::fs::remove_dir_all(&bundle);
}
