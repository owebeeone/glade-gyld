use super::*;

/// A runner standing in for the merge host: it records the argv it was given
/// and answers with the document that host prints on stdout.
struct Merging {
    answer: String,
    exit: i32,
    argv: std::sync::Mutex<Vec<Vec<String>>>,
    /// Was the fragment file really there when the host ran?
    read: std::sync::Mutex<Option<String>>,
}

impl Merging {
    fn new(answer: serde_json::Value, exit: i32) -> Arc<Merging> {
        Arc::new(Merging {
            answer: answer.to_string(),
            exit,
            argv: std::sync::Mutex::new(Vec::new()),
            read: std::sync::Mutex::new(None),
        })
    }
}

impl crate::exec::Runner for Merging {
    fn run(
        &self,
        plan: &Plan,
        _limits: crate::exec::Limits,
        _on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        self.argv.lock().unwrap().push(plan.argv.clone());
        // The host's ONE operand: the fragment, as the plan laid it down.
        if let Some(path) = plan.argv.last() {
            *self.read.lock().unwrap() = std::fs::read_to_string(path).ok();
        }
        Ok(RunOutput {
            exit: self.exit,
            stdout: format!("{}\n", self.answer),
            stderr: String::new(),
            truncated: false,
        })
    }
}

/// A fragment `answer` plan whose notebook is the owner's, with the staging
/// tree pointing at the checkout's sample of that name.
fn folding(layout: &Layout, stream: &str, fragment: &str) -> Plan {
    let notebook = layout.overlay_home().join(verbs::overlay_file(stream));
    let mut plan = answering(layout, stream, "build-0000000000002");
    plan.write = Some(verbs::PlannedWrite {
        path: notebook,
        text: String::new(),
        force: true,
    });
    plan.merge = Some(verbs::Merge {
        fragment: verbs::PlannedWrite {
            path: layout.requests().join("run-1.json"),
            text: fragment.to_string(),
            force: true,
        },
        argv: vec![
            "/g/scripts/manage_decision_streams.py".into(),
            "--repository".into(),
            layout.stage().display().to_string(),
            "merge".into(),
            stream.to_string(),
            "--fragment".into(),
            layout.requests().join("run-1.json").display().to_string(),
        ],
    });
    plan
}

/// A merge the host accepted: the notebook holds the module it printed, the
/// staging tree reads it, and the fragment is gone again.
#[test]
fn a_fragment_is_folded_by_the_host_and_its_answer_becomes_the_notebook() {
    let dir = root("fold");
    let name = verbs::overlay_file("stream-a");
    std::fs::create_dir_all(dir.join("gyld/examples")).unwrap();
    let shipped = dir.join("gyld/examples").join(&name);
    std::fs::write(&shipped, "the shipped sample\n").unwrap();
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"))
        .with_decisions_root(Some(dir.join("decisions")));
    bundle::ensure_stage(&layout).unwrap();
    let mut config = GyldConfig::new(
        "ws://x",
        layout.gyld_root.clone(),
        layout.bundle_root.clone(),
    );
    config.layout = layout.clone();

    let plan = folding(&layout, "stream-a", r#"{"classes":"class R: pass"}"#);
    let runner = Merging::new(
        serde_json::json!({
            "ok": true,
            "stream": "stream-a",
            "file": shipped.display().to_string(),
            "added": { "classes": ["ScopeModelRuling"], "members": ["scope_model_ruling"] },
            "text": "the sample plus the ruling\n",
        }),
        0,
    );
    let (written, said) = fold(&config, runner.as_ref(), &plan).expect("the merge lands");

    assert_eq!(
        said.as_deref(),
        Some(format!("merged ScopeModelRuling into {name}").as_str()),
        "the module the host printed never reaches the log; one line does"
    );
    let notebook = plan.overlay.clone().unwrap();
    assert_eq!(
        written.write.as_ref().unwrap().text,
        "the sample plus the ruling\n"
    );
    assert_eq!(
        std::fs::read_to_string(&notebook).unwrap(),
        "the sample plus the ruling\n"
    );
    assert_eq!(
        std::fs::read_to_string(&shipped).unwrap(),
        "the shipped sample\n",
        "the checkout's sample is read-only to the supplier, fragment or not"
    );
    assert_eq!(
        std::fs::read_link(layout.overlays().join(&name)).unwrap(),
        notebook,
        "the staging tree reads the owner's copy from here on"
    );
    // The host was handed the fragment, and the fragment is gone again.
    assert_eq!(
        runner.read.lock().unwrap().as_deref(),
        Some(r#"{"classes":"class R: pass"}"#)
    );
    assert!(
        !layout.requests().join("run-1.json").exists(),
        "a request document left lying about is a request nobody made"
    );
    assert_eq!(runner.argv.lock().unwrap().len(), 1);
    assert_eq!(runner.argv.lock().unwrap()[0][3], "merge");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A merge the host REFUSED: its own code and message, and nothing written.
#[test]
fn a_refused_merge_carries_the_hosts_code_and_writes_nothing() {
    let dir = root("fold-refused");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"))
        .with_decisions_root(Some(dir.join("decisions")));
    bundle::ensure_stage(&layout).unwrap();
    let mut config = GyldConfig::new(
        "ws://x",
        layout.gyld_root.clone(),
        layout.bundle_root.clone(),
    );
    config.layout = layout.clone();
    let plan = folding(&layout, "stream-a", "{}");

    let runner = Merging::new(
        serde_json::json!({
            "ok": false,
            "code": "NOTEBOOK_ALREADY_HAS",
            "message": "this notebook already has ScopeModelRuling; to change that answer, \
                        edit glade-decisions-stream-a.gyld.py and press Rebuild",
        }),
        1,
    );
    let refusal = fold(&config, runner.as_ref(), &plan).expect_err("a refused merge");
    assert_eq!(refusal.code, "NOTEBOOK_ALREADY_HAS");
    assert!(
        refusal.message.contains("already has ScopeModelRuling"),
        "{refusal:?}"
    );
    assert_eq!(refusal.stream, "stream-a");
    assert!(
        refusal.restored,
        "nothing was written, so the notebook IS as it was; a desk reads false as \
         `could NOT be put back - check it`, which is an alarm and not a fact here"
    );
    assert!(
        plan.overlay.as_ref().unwrap().symlink_metadata().is_err(),
        "a refused merge leaves no notebook"
    );
    assert!(!layout.requests().join("run-1.json").exists());

    // A host that answered nothing at all is refused by its last word instead.
    let mute = Merging::new(serde_json::json!("not a document"), 1);
    let refusal = fold(&config, mute.as_ref(), &plan).expect_err("a refused merge");
    assert_eq!(refusal.code, outcome::RUN_FAILED);
    assert!(refusal.message.contains("exited 1"), "{refusal:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_plan_with_no_merge_step_passes_through_untouched() {
    let dir = root("fold-none");
    let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
    let mut config = GyldConfig::new(
        "ws://x",
        layout.gyld_root.clone(),
        layout.bundle_root.clone(),
    );
    config.layout = layout.clone();
    let plan = answering(&layout, "stream-a", "build-0000000000002");
    let runner = Merging::new(serde_json::json!({ "ok": true }), 0);

    let (same, said) = fold(&config, runner.as_ref(), &plan).expect("no merge, no refusal");
    assert_eq!(same, plan);
    assert_eq!(said, None);
    assert!(
        runner.argv.lock().unwrap().is_empty(),
        "the whole-module path runs no merge host"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
