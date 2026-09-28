use super::*;

#[test]
fn a_planned_write_refuses_to_clobber_unless_forced() {
    let dir = root("write");
    let layout = Layout::new(dir.join("gyld"), dir.clone());
    let path = layout.overlays().join("glade-decisions-a.gyld.py");

    let mut plan = forced_write(&path, "one\n");
    plan.write.as_mut().unwrap().force = false;
    write_overlay(&layout, &plan).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

    let e = write_overlay(&layout, &plan).unwrap_err();
    assert!(e.contains("nothing is overwritten"), "{e}");

    let forced = forced_write(&path, "two\n");
    write_overlay(&layout, &forced).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");

    // A seed LINK is something at that name too: an unforced write refuses
    // it rather than asking what it points at.
    let seeded = layout.overlays().join("glade-decisions-b.gyld.py");
    bundle::link(&path, &seeded).unwrap();
    let mut unforced = forced_write(&seeded, "three\n");
    unforced.write.as_mut().unwrap().force = false;
    let e = write_overlay(&layout, &unforced).unwrap_err();
    assert!(e.contains("nothing is overwritten"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The write's OWN check, past the planner's lexical one: a directory link
/// in the tree must not redirect a ruling out of the overlay home.
#[test]
fn a_write_refuses_a_path_that_leads_outside_the_overlay_home() {
    let dir = root("home");
    let decisions = dir.join("decisions");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&decisions).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"))
        .with_decisions_root(Some(decisions.clone()));

    // A path in another tree altogether: refused lexically, and nothing is
    // laid down on the way to finding out.
    let outside = elsewhere.join("glade-decisions-a.gyld.py");
    let e = write_overlay(&layout, &forced_write(&outside, "no\n")).unwrap_err();
    assert!(e.contains("not in the overlay home"), "{e}");
    assert!(!outside.exists());

    // A DIRECTORY link inside the home, pointing out of it. Lexically the
    // path is in the home; the filesystem says otherwise, and the filesystem
    // is who the writer asks.
    bundle::link(&elsewhere, &decisions.join("sub")).unwrap();
    let through = decisions.join("sub/glade-decisions-a.gyld.py");
    let e = write_overlay(&layout, &forced_write(&through, "no\n")).unwrap_err();
    assert!(e.contains("leads outside the overlay home"), "{e}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

    // And a home that is ITSELF reached through a link still writes: both
    // sides are canonicalized, so /tmp and /private/tmp are one home.
    let linked = dir.join("linked-decisions");
    bundle::link(&decisions, &linked).unwrap();
    let through_home =
        Layout::new(dir.join("gyld"), dir.join("bundle")).with_decisions_root(Some(linked.clone()));
    let notebook = linked.join("glade-decisions-a.gyld.py");
    write_overlay(&through_home, &forced_write(&notebook, "yes\n")).expect("the write lands");
    assert_eq!(
        std::fs::read_to_string(decisions.join("glade-decisions-a.gyld.py")).unwrap(),
        "yes\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The defect this test exists for: `overlays/` holds one SEED LINK per file
/// of the Gyld checkout's `examples`, and a write through one of them lands
/// in the checkout. `std::fs::write` opens the target and follows the link;
/// the containment check is lexical and sees only a path under the bundle
/// root. An `answer` on a shipped sample stream therefore rewrote
/// `gyld/examples/glade-decisions-stream-a.gyld.py` in the owner's checkout.
#[test]
fn a_write_through_a_seed_link_never_reaches_the_checkout() {
    let dir = root("seed-link");
    let examples = dir.join("checkout/examples");
    std::fs::create_dir_all(&examples).unwrap();
    let shipped = examples.join("glade-decisions-stream-a.gyld.py");
    std::fs::write(&shipped, "shipped sample\n").unwrap();

    let layout = Layout::new(dir.join("checkout"), dir.join("bundle"));
    let overlays = layout.overlays();
    std::fs::create_dir_all(&overlays).unwrap();
    let staged = overlays.join("glade-decisions-stream-a.gyld.py");
    bundle::link(&shipped, &staged).unwrap();

    let plan = forced_write(&staged, "the owner's ruling\n");
    write_overlay(&layout, &plan).expect("the write lands");

    assert_eq!(
        std::fs::read_to_string(&shipped).unwrap(),
        "shipped sample\n",
        "the checkout is read-only to the supplier, seed link or not"
    );
    assert_eq!(
        std::fs::read_to_string(&staged).unwrap(),
        "the owner's ruling\n"
    );
    assert!(
        !staged.symlink_metadata().unwrap().file_type().is_symlink(),
        "the write replaced the link rather than following it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Copy-on-write, end to end: the owner answers a question in a sample that
/// ships with Gyld, and gets a file of his own without the sample changing.
#[test]
fn an_answer_on_a_shipped_sample_leaves_the_owners_copy_and_points_the_stage_at_it() {
    let dir = root("copy-on-write");
    let gyld = dir.join("gyld");
    let name = "glade-decisions-stream-a.gyld.py";
    std::fs::create_dir_all(gyld.join("examples")).unwrap();
    let shipped = gyld.join("examples").join(name);
    std::fs::write(&shipped, "the shipped sample\n").unwrap();
    let layout =
        Layout::new(gyld, dir.join("bundle")).with_decisions_root(Some(dir.join("decisions")));
    bundle::ensure_stage(&layout).unwrap();

    // The staging tree starts out pointing into the checkout — the exact
    // arrangement a write used to follow.
    let staged = layout.overlays().join(name);
    assert_eq!(std::fs::read_link(&staged).unwrap(), shipped);

    let notebook = layout.overlay_home().join(name);
    let plan = forced_write(&notebook, "the owner's ruling\n");
    write_overlay(&layout, &plan).expect("the write lands");
    stage_notebook(&layout, &plan).expect("the stage is pointed at it");

    assert_eq!(
        std::fs::read_to_string(&shipped).unwrap(),
        "the shipped sample\n",
        "the sample that ships with Gyld is never edited"
    );
    assert_eq!(
        std::fs::read_to_string(&notebook).unwrap(),
        "the owner's ruling\n"
    );
    assert_eq!(
        std::fs::read_link(&staged).unwrap(),
        notebook,
        "the staging tree reads the owner's copy from here on"
    );
    assert_eq!(
        std::fs::read_to_string(layout.stage_examples().join(name)).unwrap(),
        "the owner's ruling\n"
    );

    // And the answer names the file, because the file is there.
    let named = plan.clone();
    assert_eq!(
        overlay_left(&Plan {
            overlay: Some(notebook.clone()),
            ..named
        }),
        Some(notebook.display().to_string())
    );
    // A notebook that is not there yet — a streamed fork, before its host has
    // run — is named by nobody.
    assert_eq!(
        overlay_left(&Plan {
            overlay: Some(
                layout
                    .overlay_home()
                    .join("glade-decisions-not-yet.gyld.py")
            ),
            ..plan
        }),
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}
