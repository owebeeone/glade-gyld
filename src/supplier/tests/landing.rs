use super::*;

/// A build stand-in: the directory and the `streams.json` that makes it one,
/// plus one stream's validation document.
fn built(dir: &std::path::Path, stream: &str, ok: bool) -> PathBuf {
    let streams = dir.join("streams").join(stream);
    std::fs::create_dir_all(&streams).unwrap();
    std::fs::write(dir.join("streams.json"), r#"{"format":"gyld.streams.v1"}"#).unwrap();
    let body = serde_json::json!({
        "format": "gyld.validation.v1", "stream": stream, "built": "b", "ok": ok,
        "code": "SELECTION_NOT_OFFERED", "message": "VersionPin does not offer SdaxRs",
        "details": {}, "findings": [],
    });
    std::fs::write(streams.join("validation.json"), format!("{body}\n")).unwrap();
    dir.to_path_buf()
}

fn ok_run(exit: i32) -> RunOutput {
    RunOutput {
        exit,
        stdout: String::new(),
        stderr: "ValueError: Selects[NoSuchAlternative]\n".into(),
        truncated: false,
    }
}

/// The defect, as one test: a write Gyld rejects is put back and the
/// half-written build it made is gone.
#[test]
fn a_refused_write_is_put_back_and_its_half_written_build_removed() {
    let dir = root("refused");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
    bundle::ensure_stage(&layout).unwrap();
    let plan = answering(&layout, "stream-a", "build-0000000000002");
    let notebook = plan.overlay.clone().unwrap();
    std::fs::write(&notebook, "the ruling as it was\n").unwrap();

    let snapshot = outcome::Snapshot::take(&layout, &plan).unwrap().unwrap();
    write_overlay(&layout, &plan).unwrap();
    // The abandoned directory a structural failure leaves: a `streams/` with
    // a diagnostic in it and no `streams.json` at all.
    let half = plan.output_dir.clone().unwrap();
    std::fs::create_dir_all(half.join("streams/stream-a")).unwrap();

    let refused = land(&layout, &plan, Ok(&ok_run(1)), Some(&snapshot), None)
        .expect("a failed run is refused");
    assert_eq!(refused.0.stream, "stream-a");
    assert_eq!(refused.0.code, outcome::RUN_FAILED);
    assert!(refused.0.restored, "the notebook is back");
    assert_eq!(
        std::fs::read_to_string(&notebook).unwrap(),
        "the ruling as it was\n"
    );
    assert!(!half.exists(), "the half-written build is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reason the directory must GO and not merely be left unpublished:
/// [`bundle::latest_build`] falls back to the newest `builds/` directory
/// holding a `streams.json`, so a refused-but-COMPLETE build left behind
/// would become the current one by itself.
#[test]
fn a_refused_but_complete_build_is_removed_and_the_previous_one_still_answers() {
    let dir = root("refused-complete");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
    bundle::ensure_stage(&layout).unwrap();
    let before = built(
        &layout.new_build_dir("build-0000000000001"),
        "stream-a",
        true,
    );
    bundle::write_latest(&layout, &before).unwrap();

    let plan = answering(&layout, "stream-a", "build-0000000000002");
    let notebook = plan.overlay.clone().unwrap();
    let snapshot = outcome::Snapshot::take(&layout, &plan).unwrap().unwrap();
    write_overlay(&layout, &plan).unwrap();
    // Exit 0, a complete bundle, and the written stream invalid in it: the
    // findings class.
    let after = built(&plan.output_dir.clone().unwrap(), "stream-a", false);

    let refused = land(
        &layout,
        &plan,
        Ok(&ok_run(0)),
        Some(&snapshot),
        Some(&before),
    )
    .expect("the findings class is refused");
    assert_eq!(refused.0.code, "SELECTION_NOT_OFFERED");
    assert_eq!(
        refused.1.as_ref().and_then(|d| d.get("format")),
        Some(&serde_json::json!("gyld.validation.v1")),
        "Gyld's own document travels verbatim"
    );
    assert!(!after.exists(), "the refused build is gone");
    assert_eq!(
        bundle::latest_build(&layout).as_deref(),
        Some(before.as_path()),
        "the build that stood before the refused write is still the current one"
    );
    assert!(
        notebook.symlink_metadata().is_err(),
        "a first ruling that is refused leaves no notebook at all"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stream already `ok:false` before the write is NOT refused: the owner may
/// be part-way through repairing a notebook.
#[test]
fn a_write_onto_an_already_invalid_stream_is_not_refused() {
    let dir = root("repairing");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
    bundle::ensure_stage(&layout).unwrap();
    let before = built(
        &layout.new_build_dir("build-0000000000001"),
        "stream-a",
        false,
    );
    let plan = answering(&layout, "stream-a", "build-0000000000002");
    let snapshot = outcome::Snapshot::take(&layout, &plan).unwrap().unwrap();
    write_overlay(&layout, &plan).unwrap();
    let after = built(&plan.output_dir.clone().unwrap(), "stream-a", false);

    assert!(land(
        &layout,
        &plan,
        Ok(&ok_run(0)),
        Some(&snapshot),
        Some(&before)
    )
    .is_none());
    assert!(
        after.exists(),
        "an accepted build is left where it was built"
    );
    assert_eq!(
        std::fs::read_to_string(plan.overlay.as_ref().unwrap()).unwrap(),
        "the ruling\n",
        "the repair the owner is making stands"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plain `rebuild` has no notebook to put back, and its failed directory
/// goes all the same.
#[test]
fn a_failed_rebuild_leaves_no_directory_and_nothing_to_restore() {
    let dir = root("rebuild");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
    bundle::ensure_stage(&layout).unwrap();
    let mut plan = answering(&layout, "stream-a", "build-0000000000002");
    plan.verb = "rebuild".into();
    plan.write = None;
    plan.overlay = None;
    plan.stream = None;
    let half = plan.output_dir.clone().unwrap();
    std::fs::create_dir_all(half.join("streams")).unwrap();

    let refused = land(&layout, &plan, Err("timed out after 30s"), None, None).expect("refused");
    assert_eq!(refused.0.stream, "");
    assert_eq!(refused.0.message, "timed out after 30s");
    assert!(!refused.0.restored);
    assert!(!half.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
