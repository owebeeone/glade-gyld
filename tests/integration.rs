//! Integration: the glade-gyld supplier against a SPAWNED glade-node booted with
//! an app file that declares the gyld exchange and output surfaces (never the
//! real `~/.glade`: temp GLADE_HOME/HOME and a temp store). The crate holds no
//! node internals; the tests talk to the shipped binaries exactly as a
//! deployment would. Coverage step 4.1 names:
//!
//!   1. a RECORDING runner double sees exactly one invocation per verb, with the
//!      argv the planner promised, and `answer` writes its overlay first.
//!   2. a refused verb, a bad envelope, a malformed stream id and a path that
//!      leaves the bundle root are failure as DATA, and no host is invoked.
//!   3. a streaming run's output appends reach a log subscriber in sequence,
//!      closed by a `done:true` marker carrying the exit code.
//!   4. ONE real subprocess: `emit_decision_streams.py --help` out of a Gyld
//!      checkout, skipped loudly when that checkout or its interpreter is absent.
//!   5. the `glade-gyld` BINARY attaches, answers, and shuts down on SIGTERM,
//!      with a first build that fails staying failure as data under it.
//!   6. a build ALREADY in the bundle root reaches the value shares when the
//!      supplier attaches, with no verb issued and no host invoked at all.
//!   7. a bundle root with NO build gets its first build for itself, keeps
//!      answering while it runs, and lands its own census.
//!   8. `explain` refuses as DATA — a bad envelope, a stream the build does not
//!      list, a build with no source index — with no host invoked and the
//!      supplier still answering afterwards.
//!   9. the WHOLE consult path against a scripted model double: a normal
//!      stream, a refusal, a budget stop and a transport error, each reaching a
//!      subscriber as records closed by a terminal marker.
//!  10. a CONVERSATION of three turns on one key: the supplier reads its own
//!      records back as prior turns, the cached prefix is byte-identical across
//!      them, and the per-conversation budget refuses the turn that crosses it.
//!  11. the DRAFT: a well-formed offer with its model id, a malformed one that
//!      offers nothing and says why, and one naming an alternative the envelope
//!      does not offer, emitted unresolved rather than corrected.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use glade_client::GladeClient;
use glade_gyld::{
    serve_with, Declined, GyldAskRecord, GyldConfig, GyldOutputRecord, GyldResponse, Limits,
    ModelClient, ModelEvent, ModelOutcome, ModelRequest, Plan, RunOutput, Runner,
};

// ---- harness --------------------------------------------------------------

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn node_bin() -> PathBuf {
    manifest().join("../glade/node/target/debug/glade-node")
}

fn app_file() -> PathBuf {
    manifest().join("tests/fixtures/gyld-test-app.glade")
}

/// The gate pre-builds the node; build once if absent so the suite is
/// self-sufficient (the node has its own target dir — no lock clash).
fn ensure_node_built() {
    let bin = node_bin();
    if bin.exists() {
        return;
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "glade-node"])
        .current_dir(manifest().join("../glade/node"))
        .status()
        .expect("build glade-node");
    assert!(
        status.success() && bin.exists(),
        "glade-node missing after build"
    );
}

/// A temp dir that removes itself on drop (never the real `~/.glade`).
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        static N: AtomicU64 = AtomicU64::new(0);
        let uniq = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let p = std::env::temp_dir().join(format!("glade-gyld-{tag}-{uniq}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Read the node's `listening <port>` line (bounded), then drain stdout.
async fn wait_listening(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let port = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.ok().flatten() {
            if let Some(rest) = line.strip_prefix("listening ") {
                if let Ok(p) = rest.trim().parse::<u16>() {
                    return Some(p);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .expect("node printed a listening port");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    port
}

/// Boot the node with the gyld-test app under a temp GLADE_HOME/HOME.
async fn boot(tmp: &Tmp) -> (Child, u16) {
    ensure_node_built();
    let mut child = Command::new(node_bin())
        .args(["--profile", "local", "--name", "gyldit", "--app"])
        .arg(app_file())
        .arg("0")
        .arg(tmp.path().join("store"))
        .env("GLADE_HOME", tmp.path().join("gh"))
        .env("HOME", tmp.path().join("h"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn booted glade-node");
    let port = wait_listening(&mut child).await;
    (child, port)
}

/// The two roots the supplier is configured with: a Gyld checkout stand-in that
/// the supplier only ever reads, and the app-owned bundle root it owns.
fn roots(tmp: &Tmp) -> (PathBuf, PathBuf) {
    let gyld = tmp.path().join("gyld");
    std::fs::create_dir_all(gyld.join("scripts")).unwrap();
    std::fs::create_dir_all(gyld.join("examples")).unwrap();
    std::fs::write(gyld.join("examples/glade-decisions.gyld.py"), "# base\n").unwrap();
    let bundle = tmp.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    (gyld, bundle)
}

/// Seed a bundle so the verbs that need one have one. Returns its directory.
fn seed_bundle(bundle_root: &Path) -> PathBuf {
    let dir = bundle_root.join("builds/build-0000000000001");
    std::fs::create_dir_all(dir.join("streams/base")).unwrap();
    std::fs::write(
        dir.join("streams.json"),
        r#"{"format":"gyld.streams.v1","lineage":"glade-decision-graph","streams":[{"id":"base"}]}"#,
    )
    .unwrap();
    dir
}

/// Seed a WHOLE bundle — a listing, a stream's two documents and one lens —
/// plus the `latest.json` that names it, so the bundle root looks exactly like
/// one a previous session (or a hand seed) left behind. Returns its directory.
fn seed_whole_bundle(bundle_root: &Path) -> PathBuf {
    let dir = bundle_root.join("builds/build-0000000000009");
    let lenses = dir.join("streams/base/lenses");
    std::fs::create_dir_all(&lenses).unwrap();
    std::fs::write(
        dir.join("streams.json"),
        br#"{"format":"gyld.streams.v1","streams":[{"id":"base"},{"id":"stream-a"}]}"#,
    )
    .unwrap();
    std::fs::write(dir.join("streams/base/stream.json"), br#"{"id":"base"}"#).unwrap();
    std::fs::write(
        dir.join("streams/base/decide-now.json"),
        br#"{"format":"gyld.decide-now.v1","questions":[]}"#,
    )
    .unwrap();
    std::fs::write(lenses.join("decisions.lens.json"), b"abc").unwrap();
    std::fs::write(
        bundle_root.join("latest.json"),
        br#"{"output_dir":"builds/build-0000000000009"}"#,
    )
    .unwrap();
    dir
}

/// A well-formed `gyld.ask-context.v1` envelope over a seeded bundle's `base`
/// stream, as the page's `askEnvelope()` composes one.
fn ask_envelope(stream: &str, question: &str) -> serde_json::Value {
    serde_json::json!({
        "format": "gyld.ask-context.v1",
        "stream": stream,
        "perspective": "decisions",
        "snapshot": null,
        "record": {
            "slot": "glade_decisions:GladeDecisions.key_custody",
            "label": "key_custody",
            "lines": ["Key custody and recovery posture"],
            "kind": "question", "definition": "Question", "description": ""
        },
        "status": {
            "emitted": true, "listed": true, "declared": "open",
            "effective": "blocked", "tier": "now", "answerable_now": false,
            "reason": "waits on proof_family"
        },
        "alternatives": [], "lean": null, "ruling": null,
        "sources": [{"tag": "Q11", "cites": "record"}],
        "requires": [], "unlocks": [], "gates": [], "neighbourhood": null,
        "principal": "gianni",
        "conversation": "conv-tab1-key_custody-1789",
        "question": question
    })
}

/// The `explain` request that carries one.
fn explain(stream: &str, question: &str) -> String {
    serde_json::json!({
        "verb": "explain",
        "stream_output": true,
        "args": { "context": ask_envelope(stream, question) }
    })
    .to_string()
}

fn config_for(url: &str, gyld: PathBuf, bundle: PathBuf) -> GyldConfig {
    let mut c = GyldConfig::new(url, gyld, bundle);
    c.principal = Some("gianni".into());
    c.limits = Limits {
        timeout: Duration::from_secs(30),
        max_output_bytes: 64 * 1024,
    };
    c
}

/// A runner double: records every plan it is handed, replays scripted output
/// lines, and (when asked) leaves a `streams.json` in the plan's output
/// directory so the success path is reachable with no Python in sight.
#[derive(Default)]
struct Recorder {
    plans: Mutex<Vec<Plan>>,
    lines: Vec<(&'static str, &'static str)>,
    exit: i32,
    build: bool,
    fail: Option<String>,
    /// Held closed until the test has a subscriber on the output surface, so no
    /// line can be appended before there is anybody to see it (the log surface
    /// is `from-cursor`: a late subscriber misses what it did not ask for).
    gate: Option<Arc<Barrier>>,
    /// How long the FIRST run dawdles inside the host; the runs behind it go
    /// straight through. Two overlapping writing verbs need exactly that — the
    /// first still in flight while the second arrives.
    hold_first: Option<Duration>,
    ran: AtomicU64,
    /// One exit code per run, in order. `exit` serves every run past the end, so
    /// a test that does not script them is unaffected.
    exits: Mutex<Vec<i32>>,
    /// `(path, text)` the run WRITES, the way the real `fork` and `link` host
    /// writes its generated module into `<repository>/examples` — so there is
    /// something in the staging tree for the adoption to adopt.
    wrote: Option<(PathBuf, &'static str)>,
}

impl Recorder {
    fn argvs(&self) -> Vec<Vec<String>> {
        self.plans
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.argv.clone())
            .collect()
    }

    fn count(&self) -> usize {
        self.plans.lock().unwrap().len()
    }
}

impl Runner for Recorder {
    fn run(
        &self,
        plan: &Plan,
        _limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        self.plans.lock().unwrap().push(plan.clone());
        let first = self.ran.fetch_add(1, Ordering::SeqCst) == 0;
        if let Some(gate) = self.gate.as_ref() {
            gate.wait();
        }
        if let (true, Some(held)) = (first, self.hold_first) {
            std::thread::sleep(held);
        }
        if let Some(e) = self.fail.as_deref() {
            return Err(e.to_string());
        }
        for (stream, line) in self.lines.iter() {
            on_line(stream, line);
        }
        if let Some((path, text)) = self.wrote.as_ref() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        if self.build {
            if let Some(dir) = plan.output_dir.as_ref() {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join("streams.json"), "{\"format\":\"gyld.streams.v1\"}")
                    .unwrap();
            }
        }
        let mut exits = self.exits.lock().unwrap();
        let exit = match exits.is_empty() {
            true => self.exit,
            false => exits.remove(0),
        };
        Ok(RunOutput {
            exit,
            stdout: "out\n".into(),
            stderr: String::new(),
            truncated: false,
        })
    }
}

/// A scripted model: it answers a token count and replays chunks, so the whole
/// consult path runs with no network in sight. The SSE vocabulary itself is
/// folded and asserted in the crate's own unit tests.
struct ScriptedModel {
    counted: u64,
    chunks: Vec<&'static str>,
    /// The tool input a drafting turn calls back with — the RAW input, as the
    /// SSE fold hands it over: a string here is one that was not even JSON.
    draft: Option<serde_json::Value>,
    stop_reason: &'static str,
    declined: Option<Declined>,
    input_tokens: u64,
    output_tokens: u64,
    transport: Option<&'static str>,
    /// What the call had to do differently, said before anything else — the
    /// compatibility fallbacks, as the real client emits them.
    notes: Vec<&'static str>,
    calls: AtomicU64,
    /// Every request it was handed, in order — so a conversation's prefix can
    /// be diffed between turns rather than taken on trust.
    seen: Mutex<Vec<ModelRequest>>,
}

impl Default for ScriptedModel {
    fn default() -> ScriptedModel {
        ScriptedModel {
            counted: 1200,
            chunks: Vec::new(),
            draft: None,
            stop_reason: glade_gyld::END_TURN,
            declined: None,
            input_tokens: 1200,
            output_tokens: 42,
            transport: None,
            notes: Vec::new(),
            calls: AtomicU64::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl ScriptedModel {
    /// The requests it was handed, oldest first.
    fn seen(&self) -> Vec<ModelRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl ModelClient for ScriptedModel {
    fn count_tokens(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<u64, String> {
        // Recorded HERE, not in `stream`: a turn the budget refuses is counted
        // and never sent, and it is still a turn this double was asked about.
        self.seen.lock().unwrap().push(request.clone());
        for note in self.notes.iter() {
            on_event(ModelEvent::Note((*note).to_string()));
        }
        Ok(self.counted)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            request.prompt.system.contains("You are the Gyld ask agent"),
            "the stance reaches the model: {}",
            request.prompt.system
        );
        if let Some(e) = self.transport {
            return Err(e.to_string());
        }
        for chunk in self.chunks.iter() {
            on_event(ModelEvent::Text((*chunk).to_string()));
        }
        if let Some(draft) = self.draft.clone() {
            on_event(ModelEvent::Draft(draft));
        }
        Ok(ModelOutcome {
            stop_reason: self.stop_reason.into(),
            declined: self.declined.clone(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            ..Default::default()
        })
    }
}

/// The model for every test that is not about the model: it panics if a verb
/// ever reaches it, because no other verb should.
struct NoModel;

impl ModelClient for NoModel {
    fn count_tokens(
        &self,
        _request: &ModelRequest,
        _on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<u64, String> {
        panic!("no verb but `explain` counts tokens");
    }

    fn stream(
        &self,
        _request: &ModelRequest,
        _on_event: &mut dyn FnMut(ModelEvent),
    ) -> Result<ModelOutcome, String> {
        panic!("no verb but `explain` calls a model");
    }
}

async fn poll<F, Fut>(mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Issue an envelope and decode the `GyldResponse`. Asserts the WIRE ok is true
/// (the exchange always produces a structured answer).
async fn request(requester: &GladeClient, envelope: &str) -> GyldResponse {
    let out = requester
        .exchange("ws-razel", "gyld.ops", envelope.as_bytes().to_vec())
        .await
        .expect("exchange");
    assert!(
        out.ok,
        "wire ExchangeRes.ok is always true (failure is in the payload); error={:?}",
        out.error
    );
    serde_json::from_slice(&out.payload.expect("payload")).expect("GyldResponse json")
}

/// Wait until the supplier is the attached provider.
async fn attached(requester: &GladeClient) {
    let r = requester.clone();
    let ready = poll(|| {
        let r = r.clone();
        async move {
            r.exchange("ws-razel", "gyld.ops", br#"{"verb":"list"}"#.to_vec())
                .await
                .map(|o| o.ok)
                .unwrap_or(false)
        }
    })
    .await;
    assert!(ready, "the gyld supplier attached and answered");
}

// ---- 1. one host invocation per verb, with the promised argv ---------------

#[tokio::test(flavor = "multi_thread")]
async fn each_verb_maps_to_one_recorded_host_invocation() {
    let tmp = Tmp::new("argv");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let seeded = seed_bundle(&bundle);

    let runner = Arc::new(Recorder {
        build: true,
        ..Default::default()
    });
    let _sup = serve_with(
        config_for(&url, gyld.clone(), bundle.clone()),
        runner.clone(),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    // `list` is the one verb with no subprocess: it reads the bundle listing.
    let listed = request(&requester, r#"{"verb":"list"}"#).await;
    assert!(listed.ok, "{listed:?}");
    assert!(listed.stdout.contains("gyld.streams.v1"), "{listed:?}");
    assert_eq!(runner.count(), 0, "list ran no host");
    assert_eq!(listed.attributed_to.as_deref(), Some("gianni"));

    let host = gyld
        .join("scripts/manage_decision_streams.py")
        .display()
        .to_string();
    let stage = bundle.join("stage").display().to_string();

    let forked = request(
        &requester,
        r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a","note":"why"}}"#,
    )
    .await;
    assert!(forked.ok, "{forked:?}");
    assert_eq!(
        runner.argvs().last().unwrap(),
        &vec![
            host.clone(),
            "--repository".into(),
            stage.clone(),
            "fork".into(),
            "base".into(),
            "keys-a".into(),
            "--note".into(),
            "why".into()
        ]
    );

    let linked = request(
        &requester,
        r#"{"verb":"link","args":{"parent":"base","stream":"keys-b"}}"#,
    )
    .await;
    assert!(linked.ok, "{linked:?}");
    assert_eq!(runner.argvs().last().unwrap()[3], "link");

    let diffed = request(
        &requester,
        r#"{"verb":"diff","args":{"left":"base","right":"keys-a"}}"#,
    )
    .await;
    assert!(diffed.ok, "{diffed:?}");
    let argv = runner.argvs().last().unwrap().clone();
    assert_eq!(
        argv[3..8].to_vec(),
        vec![
            "diff",
            "base",
            "keys-a",
            "--bundle",
            &seeded.display().to_string()
        ]
    );

    // `answer` writes the exported overlay module into the bundle root's
    // overlays tree FIRST, then rebuilds into a directory that did not exist.
    let answered = request(
        &requester,
        r##"{"verb":"answer","args":{"stream":"keys-a","overlay":"# ruled\n"}}"##,
    )
    .await;
    assert!(answered.ok, "{answered:?}");
    let overlay = bundle.join("overlays/glade-decisions-keys-a.gyld.py");
    assert_eq!(std::fs::read_to_string(&overlay).unwrap(), "# ruled\n");
    let built = answered.output_dir.clone().expect("a new build directory");
    assert!(
        built.starts_with(&bundle.join("builds").display().to_string()),
        "{built}"
    );
    assert_ne!(
        built,
        seeded.display().to_string(),
        "never built over the previous bundle"
    );

    // and the build becomes the bundle root's latest, so the next verb uses it.
    let rebuilt = request(&requester, r#"{"verb":"rebuild"}"#).await;
    assert!(rebuilt.ok, "{rebuilt:?}");
    let argv = runner.argvs().last().unwrap().clone();
    assert_eq!(
        argv[5], built,
        "the rebuild started from the build answer left behind"
    );

    // `ask` appends the question fragment to the exported module.
    let asked = request(
        &requester,
        r##"{"verb":"ask","args":{"stream":"keys-a","overlay":"# ruled","question":"class Q: pass"}}"##,
    )
    .await;
    assert!(asked.ok, "{asked:?}");
    assert_eq!(
        std::fs::read_to_string(&overlay).unwrap(),
        "# ruled\n\nclass Q: pass\n"
    );

    // The Gyld checkout is untouched throughout: one example file, no writes.
    let examples: Vec<_> = std::fs::read_dir(gyld.join("examples")).unwrap().collect();
    assert_eq!(
        examples.len(),
        1,
        "the Gyld checkout's examples are read only"
    );

    requester.close().await;
    node.kill().await.ok();
}

// ---- 1b. a configured decisions root owns every written notebook ----------

/// The owner's ruling: a notebook the desk writes lives in a git-tracked folder
/// he commits when he chooses. Over the wire, both ways it can arrive — a module
/// a HOST wrote into the staging tree (`fork`), and one the SUPPLIER wrote
/// itself (`answer`, here on a sample that ships with the checkout).
#[tokio::test(flavor = "multi_thread")]
async fn a_decisions_root_holds_the_notebooks_and_the_stage_links_to_them() {
    let tmp = Tmp::new("decisions");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);
    // A sample that ships with the checkout, which the owner is about to answer
    // a question in.
    let sample = "glade-decisions-stream-a.gyld.py";
    let shipped = gyld.join("examples").join(sample);
    std::fs::write(&shipped, "# the shipped sample\n").unwrap();

    let decisions = tmp.path().join("decisions");
    let forked = "glade-decisions-keys-a.gyld.py";
    let runner = Arc::new(Recorder {
        build: true,
        wrote: Some((bundle.join("overlays").join(forked), "# forked\n")),
        ..Default::default()
    });
    let mut config = config_for(&url, gyld.clone(), bundle.clone());
    config.layout = config
        .layout
        .clone()
        .with_decisions_root(Some(decisions.clone()));
    let _sup = serve_with(config, runner.clone(), Arc::new(NoModel))
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    // A fork: the host writes the module into the staging tree and the supplier
    // adopts it afterwards, leaving a link where the host put it.
    let out = request(
        &requester,
        r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a"}}"#,
    )
    .await;
    assert!(out.ok, "{out:?}");
    assert_eq!(
        out.overlay_file.as_deref(),
        Some(decisions.join(forked).display().to_string().as_str()),
        "the answer names the file the owner commits, not the staging path"
    );
    assert_eq!(
        std::fs::read_to_string(decisions.join(forked)).unwrap(),
        "# forked\n"
    );
    assert_eq!(
        std::fs::read_link(bundle.join("overlays").join(forked)).unwrap(),
        decisions.join(forked)
    );

    // An answer on the SHIPPED sample: the owner's copy lands in his folder and
    // the seed link into the checkout is re-pointed at it.
    let out = request(
        &requester,
        r##"{"verb":"answer","args":{"stream":"stream-a","overlay":"# ruled"}}"##,
    )
    .await;
    assert!(out.ok, "{out:?}");
    assert_eq!(
        out.overlay_file.as_deref(),
        Some(decisions.join(sample).display().to_string().as_str())
    );
    assert_eq!(
        std::fs::read_to_string(decisions.join(sample)).unwrap(),
        "# ruled\n"
    );
    assert_eq!(
        std::fs::read_link(bundle.join("overlays").join(sample)).unwrap(),
        decisions.join(sample)
    );
    assert_eq!(
        std::fs::read_to_string(&shipped).unwrap(),
        "# the shipped sample\n",
        "the sample that ships with Gyld is never edited"
    );

    requester.close().await;
    node.kill().await.ok();
}

// ---- 2. refusals are data, and no host is invoked --------------------------

#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_data_and_never_reach_a_host() {
    let tmp = Tmp::new("deny");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);

    let runner = Arc::new(Recorder::default());
    let _sup = serve_with(
        config_for(&url, gyld, bundle),
        runner.clone(),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let cases: &[(&str, &str)] = &[
        (r#"{"verb":"capture"}"#, "allow-list"),
        (r#"{"verb":"occurred"}"#, "allow-list"),
        ("not json", "bad envelope"),
        (
            r#"{"verb":"fork","args":{"parent":"base","stream":"../../etc"}}"#,
            "not a stream id",
        ),
        (
            r#"{"verb":"fork","args":{"parent":"base","stream":"keys a"}}"#,
            "not a stream id",
        ),
        (
            r#"{"verb":"diff","args":{"left":"base"}}"#,
            "`right` is required",
        ),
        (
            r#"{"verb":"answer","args":{"stream":"keys-a"}}"#,
            "`overlay`",
        ),
    ];
    for (envelope, expected) in cases {
        let r = request(&requester, envelope).await;
        assert!(!r.ok, "{envelope} must be refused: {r:?}");
        assert!(
            r.error.as_deref().unwrap_or("").contains(expected),
            "{envelope} -> {r:?} (wanted {expected})"
        );
        assert!(r.exit.is_none(), "{envelope}: no host ran: {r:?}");
    }
    assert_eq!(runner.count(), 0, "not one refusal reached a host");

    // A host failure is data too, not a hang and not a panic.
    requester.close().await;
    node.kill().await.ok();
}

// ---- 2b. `explain` refuses as data, and runs no host -----------------------

#[tokio::test(flavor = "multi_thread")]
async fn explain_refusals_are_data_and_the_supplier_keeps_answering() {
    let tmp = Tmp::new("explain");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let seeded = seed_bundle(&bundle);

    // A key file makes the key PRESENT whatever this process's environment
    // holds, so the refusals under test are the ones this test is about. Its
    // contents are never read here: the supplier only asks whether it exists.
    std::fs::create_dir_all(bundle.join("agent")).unwrap();
    std::fs::write(bundle.join("agent/api-key"), "not-a-key\n").unwrap();

    let runner = Arc::new(Recorder::default());
    let _sup = serve_with(
        config_for(&url, gyld, bundle.clone()),
        runner.clone(),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    // The seeded bundle carries no `sources.json`: grounding was ruled in from
    // day one, so an ungrounded answer is refused rather than given.
    let r = request(&requester, &explain("base", "why is this blocked?")).await;
    assert!(!r.ok, "{r:?}");
    let said = r.error.clone().unwrap_or_default();
    assert!(
        said.contains("sources.json") && said.contains("--sources-root"),
        "{said}"
    );
    assert!(r.run_id.is_none(), "nothing started: {r:?}");

    // With an index there, a stream the build does not list is still refused.
    std::fs::write(
        seeded.join("sources.json"),
        br#"{"format":"gyld.sources.v1"}"#,
    )
    .unwrap();
    let r = request(&requester, &explain("stream-a", "why?")).await;
    assert!(!r.ok, "{r:?}");
    assert!(
        r.error
            .clone()
            .unwrap_or_default()
            .contains("this build lists"),
        "{r:?}"
    );

    // A malformed envelope names the field, not a flat `bad envelope`.
    let r = request(&requester, r#"{"verb":"explain","args":{"stream":"base"}}"#).await;
    assert!(
        !r.ok && r.error.clone().unwrap_or_default().contains("`context`"),
        "{r:?}"
    );

    // Not one of them reached a host, and the supplier is still answering.
    assert_eq!(runner.count(), 0, "`explain` runs no host, ever");
    let listed = request(&requester, r#"{"verb":"list"}"#).await;
    assert!(listed.ok, "the supplier stays up: {listed:?}");

    requester.close().await;
    node.kill().await.ok();
}

// ---- 2c. the whole consult path, against a scripted model ------------------

/// The conversation every `explain` in these tests is keyed by.
const CONVERSATION: &str = "conv-tab1-key_custody-1789";

/// Seed a build's source index and an app-owned key file, so `explain` gets
/// past every guard. The key is never read by the double — the supplier only
/// asks whether a key EXISTS until the model client reads one — and the file is
/// mode 600, the only mode the real client accepts.
fn seed_agent(bundle_root: &Path, build: &Path) {
    std::fs::write(
        build.join("sources.json"),
        serde_json::json!({
            "format": "gyld.sources.v1",
            "written": "2026-09-16T10:37:26Z",
            "root": {"option": "--sources-root", "given": "../../glade-wz",
                     "name": "glade-wz", "found": true},
            "documents": [],
            "tags": [{"tag": "Q11", "family": "matrix-row",
                      "document": "GladeBuyBuildMatrix",
                      "path": "dev-docs/GladeBuyBuildMatrix.md",
                      "resolver": "table-row-id", "heading": "4. The questions",
                      "lines": [158, 158],
                      "passage": "| Q11 | Key custody and recovery posture | buy |",
                      "digest": "d8eb379e", "truncated": false}],
            "cited_by": [{"stream": "base",
                          "slot": "glade_decisions:GladeDecisions.key_custody",
                          "field": "sources", "tags": ["AZ-7"]}],
            "unresolved": [{"tag": "AZ-7", "family": "row-id",
                            "reason": "no document in this index declares it",
                            "cited_by": ["glade_decisions:GladeDecisions.key_custody"]}]
        })
        .to_string(),
    )
    .unwrap();
    let agent = bundle_root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let key = agent.join("api-key");
    std::fs::write(&key, "unused-by-the-double\n").unwrap();
    key_mode_600(&key);
}

/// Fold the ASK surface for one conversation, waiting until `turns` of them
/// have closed. A conversation is one key and one fold however many turns it
/// holds, which is the whole point of keying by it.
async fn ask_turns(sub: &GladeClient, conversation: &str, turns: usize) -> Vec<GyldAskRecord> {
    let s = sub.clone();
    let key = conversation.to_string();
    let closed = poll(|| {
        let s = s.clone();
        let key = key.clone();
        async move {
            s.fold_log("ws-razel", "gyld.ask", Some(key.as_bytes()))
                .await
                .iter()
                .filter(|e| {
                    serde_json::from_slice::<GyldAskRecord>(e)
                        .map(|r| r.done == Some(true))
                        .unwrap_or(false)
                })
                .count()
                >= turns
        }
    })
    .await;
    assert!(closed, "{turns} turn(s) on {conversation} closed");
    sub.fold_log("ws-razel", "gyld.ask", Some(conversation.as_bytes()))
        .await
        .iter()
        .filter_map(|e| serde_json::from_slice(e).ok())
        .collect()
}

/// Fold the ASK surface for one conversation, waiting for one turn's close.
async fn ask_records(sub: &GladeClient, conversation: &str) -> Vec<GyldAskRecord> {
    ask_turns(sub, conversation, 1).await
}

/// A whole conversation against one supplier: each question asked in turn, each
/// answered before the next is sent. Answers the records and the requests the
/// model was handed, so the prefix can be diffed between turns.
async fn conversed(
    model: Arc<ScriptedModel>,
    tag: &str,
    config: impl FnOnce(&mut GyldConfig),
    questions: &[&str],
) -> (Vec<GyldAskRecord>, Vec<ModelRequest>) {
    let tmp = Tmp::new(tag);
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let build = seed_bundle(&bundle);
    seed_agent(&bundle, &build);

    let mut settings = config_for(&url, gyld, bundle);
    config(&mut settings);
    let runner = Arc::new(Recorder::default());
    let _sup = serve_with(settings, runner.clone(), model.clone())
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    sub.subscribe("ws-razel", "gyld.ask", Some(CONVERSATION.as_bytes()))
        .await
        .unwrap();

    for (i, question) in questions.iter().enumerate() {
        let accepted = request(&requester, &explain("base", question)).await;
        assert!(accepted.ok, "turn {} was accepted: {accepted:?}", i + 1);
        // One turn at a time: the follow-up is composed from what the previous
        // turn WROTE, so it must have finished writing it.
        ask_turns(&sub, CONVERSATION, i + 1).await;
    }
    let records = ask_turns(&sub, CONVERSATION, questions.len()).await;

    assert_eq!(runner.count(), 0, "`explain` runs no host, ever");
    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
    (records, model.seen())
}

/// One consultation, end to end, against a scripted model.
async fn consulted(model: Arc<ScriptedModel>, tag: &str) -> Vec<GyldAskRecord> {
    let tmp = Tmp::new(tag);
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let build = seed_bundle(&bundle);
    seed_agent(&bundle, &build);

    let runner = Arc::new(Recorder::default());
    let _sup = serve_with(
        config_for(&url, gyld, bundle.clone()),
        runner.clone(),
        model.clone(),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    // The reply is keyed by the CONVERSATION, so one mount is up before the
    // turn is asked for and stays up across turns.
    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    sub.subscribe("ws-razel", "gyld.ask", Some(CONVERSATION.as_bytes()))
        .await
        .unwrap();

    let accepted = request(&requester, &explain("base", "why is this blocked?")).await;
    assert!(
        accepted.ok && accepted.done == Some(false),
        "a consultation is accepted at once: {accepted:?}"
    );
    assert_eq!(accepted.attributed_to.as_deref(), Some("gianni"));
    let run_id = accepted.run_id.clone().expect("a run id");

    let records = ask_records(&sub, CONVERSATION).await;

    assert_eq!(runner.count(), 0, "`explain` runs no host, ever");
    assert!(
        records.iter().all(|r| r.conversation == CONVERSATION),
        "every record is keyed by the conversation: {records:?}"
    );
    assert!(
        records.iter().all(|r| r.run_id == run_id),
        "and carries this turn's own run id for the audit trail: {records:?}"
    );
    assert!(records
        .iter()
        .all(|r| r.principal.as_deref() == Some("gianni")));
    assert_eq!(
        records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        (1..=records.len() as u64).collect::<Vec<_>>(),
        "sequence numbers run 1..n: {records:?}"
    );

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
    records
}

#[tokio::test(flavor = "multi_thread")]
async fn a_normal_consultation_cites_then_answers_and_closes_clean() {
    let model = Arc::new(ScriptedModel {
        chunks: vec!["It is blocked ", "by proof_family (Q11)."],
        ..Default::default()
    });
    let records = consulted(model.clone(), "consult-ok").await;
    assert_eq!(model.calls.load(Ordering::SeqCst), 1, "one call, one turn");

    // The citations come FIRST, so a reader sees what the answer is grounded in
    // before the prose arrives: the resolved passage the index emitted, and the
    // unresolved tag said to resolve to nothing.
    let citations: Vec<&GyldAskRecord> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_CITATION)
        .collect();
    assert_eq!(citations.len(), 2, "{records:?}");
    let q11 = citations[0].record.as_ref().expect("a citation record");
    assert_eq!(q11["tag"], "Q11");
    assert_eq!(q11["resolved"], true);
    assert_eq!(q11["document"], "GladeBuyBuildMatrix");
    assert_eq!(q11["digest"], "d8eb379e");
    assert!(q11["passage"].as_str().unwrap().contains("Key custody"));
    let az = citations[1].record.as_ref().expect("a citation record");
    assert_eq!(az["tag"], "AZ-7");
    assert_eq!(az["resolved"], false);
    assert_eq!(az["reason"], "no document in this index declares it");

    let answers: Vec<&str> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_ANSWER)
        .filter_map(|r| r.line.as_deref())
        .collect();
    assert_eq!(answers, vec!["It is blocked ", "by proof_family (Q11)."]);

    let end = records.last().expect("a terminal record");
    assert_eq!(end.stream, glade_gyld::ASK_END);
    assert_eq!(end.exit, Some(0));
    assert_eq!(end.done, Some(true));
    assert!(
        end.line.is_none(),
        "a clean end says nothing beyond how it ended"
    );
}

/// A fallback is a RECORD on the turn it weakened, in order, never a silence.
#[tokio::test(flavor = "multi_thread")]
async fn every_compatibility_fallback_lands_on_the_run_beside_the_answer() {
    let model = Arc::new(ScriptedModel {
        chunks: vec!["It is blocked."],
        notes: vec![
            "the ollama endpoint has no /v1/messages/count_tokens, so this turn's input budget \
             is an ESTIMATE of about 900 tokens",
            "the endpoint answered 400; retrying without `strict`",
        ],
        ..Default::default()
    });
    let records = consulted(model.clone(), "consult-notes").await;

    let notes: Vec<&str> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_NOTE)
        .filter_map(|r| r.line.as_deref())
        .collect();
    assert_eq!(notes.len(), 2, "{records:?}");
    assert!(notes[0].contains("ESTIMATE"), "{notes:?}");
    assert!(notes[1].contains("`strict`"), "{notes:?}");

    // They come BEFORE the answer they weakened, which is when they happened.
    let at = |stream: &str| {
        records
            .iter()
            .position(|r| r.stream == stream)
            .unwrap_or(usize::MAX)
    };
    assert!(
        at(glade_gyld::ASK_NOTE) < at(glade_gyld::ASK_ANSWER),
        "{records:?}"
    );

    // And they weaken nothing else: the answer is the answer and the turn is
    // still clean.
    let answers: Vec<&str> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_ANSWER)
        .filter_map(|r| r.line.as_deref())
        .collect();
    assert_eq!(answers, vec!["It is blocked."]);
    let end = records.last().expect("a terminal record");
    assert_eq!(end.stream, glade_gyld::ASK_END);
    assert_eq!(end.exit, Some(0), "a fallback is not a failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_turn_a_budget_stop_and_a_transport_failure_are_all_data() {
    // The model declined: the category and the explanation land as data.
    let records = consulted(
        Arc::new(ScriptedModel {
            stop_reason: glade_gyld::REFUSAL,
            declined: Some(Declined {
                category: "cyber".into(),
                explanation: "declined to continue".into(),
            }),
            ..Default::default()
        }),
        "consult-refuse",
    )
    .await;
    let end = records.last().expect("a terminal record");
    let said = end.line.clone().unwrap_or_default();
    assert!(said.contains("the model declined (cyber)"), "{said}");
    assert!(said.contains("declined to continue"), "{said}");
    assert_eq!(end.exit, Some(1));

    // The output budget stopped it: the partial text is KEPT and said to be.
    let records = consulted(
        Arc::new(ScriptedModel {
            chunks: vec!["It is blo"],
            stop_reason: glade_gyld::MAX_TOKENS,
            ..Default::default()
        }),
        "consult-budget",
    )
    .await;
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stream == glade_gyld::ASK_ANSWER)
            .filter_map(|r| r.line.as_deref())
            .collect::<Vec<_>>(),
        vec!["It is blo"],
        "half an answer that says it is half an answer is data"
    );
    let end = records.last().expect("a terminal record");
    assert!(
        end.line.clone().unwrap_or_default().contains("is partial"),
        "{end:?}"
    );
    assert_eq!(end.exit, Some(1));

    // The call broke: failure as data, not a hang and not a panic — and the
    // grounding is still said.
    let records = consulted(
        Arc::new(ScriptedModel {
            transport: Some("connection reset by peer"),
            ..Default::default()
        }),
        "consult-transport",
    )
    .await;
    let end = records.last().expect("a terminal record");
    assert!(
        end.line
            .clone()
            .unwrap_or_default()
            .contains("connection reset by peer"),
        "{end:?}"
    );
    assert_eq!(end.exit, Some(1));
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stream == glade_gyld::ASK_CITATION)
            .count(),
        2,
        "the grounding is said even when the call fails: {records:?}"
    );
}

// ---- 2d. the conversation: prior turns, one cached prefix, one budget ------

#[tokio::test(flavor = "multi_thread")]
async fn a_third_turn_replays_both_prior_turns_in_order_behind_one_cached_prefix() {
    let model = Arc::new(ScriptedModel {
        chunks: vec!["It is blocked ", "by proof_family (Q11)."],
        ..Default::default()
    });
    let (records, seen) = conversed(
        model.clone(),
        "consult-thread",
        |_| {},
        &[
            "why is this blocked?",
            "by what exactly?",
            "and what unlocks it?",
        ],
    )
    .await;
    assert_eq!(seen.len(), 3, "three turns, three requests");

    // The reader's own turn is on the surface, in order: without it the log is
    // not the transcript, and a follow-up has nothing to replay.
    let asked: Vec<&str> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_QUESTION)
        .filter_map(|r| r.line.as_deref())
        .collect();
    assert_eq!(
        asked,
        vec![
            "why is this blocked?",
            "by what exactly?",
            "and what unlocks it?"
        ]
    );

    // Three turns, three run ids, ONE conversation key and one fold.
    let runs: Vec<&str> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_END)
        .map(|r| r.run_id.as_str())
        .collect();
    assert_eq!(runs.len(), 3, "{records:?}");
    assert!(runs[0] != runs[1] && runs[1] != runs[2], "{runs:?}");
    assert!(records.iter().all(|r| r.conversation == CONVERSATION));

    // The FIRST turn replays nothing; the third replays both prior turns, in
    // order, each question with the answer it got.
    assert!(seen[0].turns.is_empty(), "{:?}", seen[0].turns);
    assert_eq!(seen[1].turns.len(), 1);
    let third = &seen[2];
    assert_eq!(third.turns.len(), 2, "{:?}", third.turns);
    assert_eq!(third.turns[0].question, "why is this blocked?");
    assert_eq!(third.turns[1].question, "by what exactly?");
    assert!(
        third
            .turns
            .iter()
            .all(|t| t.answer() == "It is blocked by proof_family (Q11)."),
        "the chunks rejoin into the prose that was streamed: {:?}",
        third.turns
    );
    assert_eq!(
        third.turns[0].run_id, runs[0],
        "each replayed turn keeps its own run id"
    );
    assert_eq!(third.prompt.user, "and what unlocks it?");

    // The cached prefix is byte-identical across all three turns. If it ever
    // moves, `cache_read_input_tokens` collapses to zero and the passages are
    // paid for again on every follow-up.
    let prefixes: Vec<String> = seen
        .iter()
        .map(|r| serde_json::to_string(&r.cached_prefix()).unwrap())
        .collect();
    assert_eq!(prefixes[0], prefixes[1], "the prefix moved on turn 2");
    assert_eq!(prefixes[1], prefixes[2], "the prefix moved on turn 3");

    // And the turns themselves sit AFTER it, with the one message breakpoint on
    // the settled history rather than on the question just asked.
    let body = third.body(true);
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[4]["content"][0]["text"], "and what unlocks it?");
    assert!(messages[4]["content"][0].get("cache_control").is_none());
    assert_eq!(
        messages[3]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_per_conversation_budget_refuses_the_turn_that_would_cross_it() {
    // The first turn counts 1200 in and costs 1242 all told; a 2000-token
    // conversation has no room for a second.
    let model = Arc::new(ScriptedModel {
        chunks: vec!["It is blocked."],
        ..Default::default()
    });
    let (records, seen) = conversed(
        model.clone(),
        "consult-conv-budget",
        |c| c.agent.max_conversation_tokens = Some(2_000),
        &["why is this blocked?", "and what unlocks it?"],
    )
    .await;

    assert_eq!(seen.len(), 2, "both turns were COUNTED");
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "only the first was sent: an over-budget turn costs nothing"
    );

    // Both turns are on the surface — the refused one as its own turn, closed
    // with the refusal as data and all three numbers.
    let ends: Vec<&GyldAskRecord> = records
        .iter()
        .filter(|r| r.stream == glade_gyld::ASK_END)
        .collect();
    assert_eq!(ends.len(), 2, "{records:?}");
    assert_eq!(ends[0].exit, Some(0));
    assert!(ends[0].line.is_none(), "the first turn ended clean");
    assert_eq!(ends[1].exit, Some(1));
    let said = ends[1].line.clone().unwrap_or_default();
    assert!(said.contains("1242") && said.contains("1200"), "{said}");
    assert!(said.contains("2000"), "{said}");
    assert!(
        said.contains("--agent-max-conversation-tokens"),
        "the refusal says what to do about it: {said}"
    );

    // The refused turn is still a turn: its question is on the surface, and so
    // is the grounding it was refused with.
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stream == glade_gyld::ASK_QUESTION)
            .count(),
        2,
        "{records:?}"
    );
}

// ---- 3a. the draft: an offer, never a ruling -------------------------------

/// An envelope whose record offers TWO alternatives, which is the shape a
/// question worth drafting against has.
fn two_alternatives(question: &str) -> String {
    let mut envelope = ask_envelope("base", question);
    envelope["alternatives"] = serde_json::json!([
        {"slot": "glade_decisions:GladeDecisions.key_custody.device",
         "label": "device", "description": "on the device", "preferred": true},
        {"slot": "glade_decisions:GladeDecisions.key_custody.custodian",
         "label": "custodian", "description": "with a custodian", "preferred": false}
    ]);
    serde_json::json!({
        "verb": "explain",
        "stream_output": true,
        "args": { "context": envelope }
    })
    .to_string()
}

/// One consultation over that envelope, answered by a double that drafts.
async fn drafted(model: Arc<ScriptedModel>, tag: &str) -> Vec<GyldAskRecord> {
    let tmp = Tmp::new(tag);
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let build = seed_bundle(&bundle);
    seed_agent(&bundle, &build);

    let runner = Arc::new(Recorder::default());
    let _sup = serve_with(config_for(&url, gyld, bundle), runner.clone(), model)
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;
    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    sub.subscribe("ws-razel", "gyld.ask", Some(CONVERSATION.as_bytes()))
        .await
        .unwrap();

    let accepted = request(
        &requester,
        &two_alternatives("which alternative would you propose, and how would it read?"),
    )
    .await;
    assert!(accepted.ok, "{accepted:?}");
    let records = ask_records(&sub, CONVERSATION).await;
    assert_eq!(runner.count(), 0, "`explain` runs no host, ever");

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
    records
}

#[tokio::test(flavor = "multi_thread")]
async fn a_well_formed_draft_lands_as_an_offer_with_the_model_that_made_it() {
    let records = drafted(
        Arc::new(ScriptedModel {
            chunks: vec!["Two are offered. "],
            draft: Some(serde_json::json!({
                "alternative": "glade_decisions:GladeDecisions.key_custody.device",
                "ruling_text": "2026-09-16, owner: gianni: keys stay on the device.",
                "sources": ["Q11"]
            })),
            stop_reason: "tool_use",
            ..Default::default()
        }),
        "draft-ok",
    )
    .await;

    let draft = records
        .iter()
        .find(|r| r.stream == "draft")
        .expect("a draft record");
    let offer = draft.record.as_ref().expect("the offer");
    assert_eq!(
        offer["slot"], "glade_decisions:GladeDecisions.key_custody",
        "the record it is FOR is the envelope's own"
    );
    assert_eq!(
        offer["alternative"],
        "glade_decisions:GladeDecisions.key_custody.device"
    );
    assert_eq!(
        offer["alternative_slot"], "glade_decisions:GladeDecisions.key_custody.device",
        "the alternative's QUALIFIED slot"
    );
    assert_eq!(offer["resolved"], true);
    assert_eq!(
        offer["ruling_text"],
        "2026-09-16, owner: gianni: keys stay on the device."
    );
    assert_eq!(offer["sources"], serde_json::json!(["Q11"]));
    assert_eq!(
        offer["drafted_by"], "claude-opus-5",
        "the model id, so a draft can never be mistaken for a person's text"
    );

    // The prose still arrived, and the turn ended CLEAN: the offer is how a
    // turn that was asked to propose ends.
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stream == glade_gyld::ASK_ANSWER)
            .filter_map(|r| r.line.as_deref())
            .collect::<Vec<_>>(),
        vec!["Two are offered. "]
    );
    let end = records.last().expect("a terminal record");
    assert_eq!(end.exit, Some(0), "{end:?}");
    assert!(end.line.is_none(), "{end:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_draft_offers_nothing_and_the_turn_says_why() {
    let records = drafted(
        Arc::new(ScriptedModel {
            chunks: vec!["Two are offered. "],
            // Not even JSON — the fold hands the raw string over rather than
            // drop it, so the close can say what arrived.
            draft: Some(serde_json::json!(
                "{\"alternative\": \"...device\", \"ruling_te"
            )),
            stop_reason: "tool_use",
            ..Default::default()
        }),
        "draft-malformed",
    )
    .await;

    assert!(
        !records.iter().any(|r| r.stream == "draft"),
        "a draft that did not decode is NOT a draft: {records:?}"
    );
    let end = records.last().expect("a terminal record");
    let said = end.line.clone().unwrap_or_default();
    assert!(said.contains("did not decode as an object"), "{said}");
    assert!(said.contains("ruling_te"), "it says what arrived: {said}");
    assert_eq!(
        end.exit,
        Some(1),
        "the prose stands, but the offer the reader asked for did not arrive"
    );
    // And the prose is kept: half a turn that says so is data.
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stream == glade_gyld::ASK_ANSWER)
            .count(),
        1,
        "{records:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_naming_a_foreign_alternative_is_emitted_unresolved_not_corrected() {
    let records = drafted(
        Arc::new(ScriptedModel {
            chunks: vec!["Neither is quite it. "],
            draft: Some(serde_json::json!({
                "alternative": "glade_decisions:GladeDecisions.key_custody.hsm",
                "ruling_text": "2026-09-16, owner: gianni: put them in an HSM.",
                "sources": ["Q11"]
            })),
            stop_reason: "tool_use",
            ..Default::default()
        }),
        "draft-foreign",
    )
    .await;

    let offer = records
        .iter()
        .find(|r| r.stream == "draft")
        .and_then(|r| r.record.clone())
        .expect("a draft record, and an unresolved one");
    assert_eq!(
        offer["alternative"], "glade_decisions:GladeDecisions.key_custody.hsm",
        "the name it GAVE, uncorrected"
    );
    assert_eq!(offer["resolved"], false);
    assert!(
        offer.get("alternative_slot").is_none(),
        "there is no qualified slot for an alternative nothing offers: {offer}"
    );
    let reason = offer["reason"].as_str().unwrap_or("");
    assert!(reason.contains("key_custody.device"), "{reason}");
    assert!(reason.contains("not this one"), "{reason}");

    // It is still a draft record and still a clean turn: the agent made an
    // offer, and whether the offer matches anything emitted is DATA.
    assert_eq!(records.last().expect("an end").exit, Some(0));
}

/// The REAL HTTPS client, driven where the supplier drives it: on a blocking
/// task, against a port nothing is listening on.
///
/// The configuration a RUNNING supplier reads: a file under the app-owned
/// bundle root, with no flag and no restart anywhere in it.
///
/// grazel composes this binary's argv itself and passes none of the `--agent-*`
/// flags, so this file is the only way a desk's model or endpoint can be
/// changed at all. It is read through `GyldConfig` exactly as a call reads it.
#[test]
fn the_bundle_roots_own_config_file_configures_a_supplier_that_was_given_no_flags() {
    let tmp = Tmp::new("agent-config");
    let bundle = tmp.path().join("gyld");
    std::fs::create_dir_all(bundle.join("agent")).unwrap();
    let config = config_for(
        "ws://127.0.0.1:1",
        tmp.path().join("gyld-root"),
        bundle.clone(),
    );

    // The key file is the bundle root's own, with no flag and no file.
    let bare = config.resolve_agent();
    assert!(bare.notes.is_empty(), "{:?}", bare.notes);
    assert_eq!(bare.config.key_file, bundle.join("agent/api-key"));

    // The file lands beside the key file and is read at the next call. This
    // process's OWN environment outranks it (that is the precedence under
    // test), so what it can be asserted to change is what the environment
    // here leaves alone.
    let path = bundle.join(glade_gyld::DEFAULT_CONFIG_FILE);
    std::fs::write(
        &path,
        r#"{"base_url": "http://127.0.0.1:11434", "model": "qwen3.8-96k",
            "max_tokens": 4242}"#,
    )
    .unwrap();
    let local = config.resolve_agent();
    assert!(local.notes.is_empty(), "{:?}", local.notes);
    assert_eq!(local.config.max_output_tokens, 4242);
    if std::env::var(glade_gyld::BASE_URL_ENV).is_err() {
        assert_eq!(local.config.base_url, "http://127.0.0.1:11434");
        assert_eq!(
            local.config.compat,
            glade_gyld::Compat::Ollama,
            "an endpoint that is not Anthropic's takes the ollama profile"
        );
        assert!(!local.config.count_tokens, "there is no count_tokens there");
    } else {
        eprintln!("SKIP: {} is set here", glade_gyld::BASE_URL_ENV);
    }
    if std::env::var(glade_gyld::MODEL_ENV).is_err() {
        assert_eq!(local.config.model, "qwen3.8-96k");
    }

    // A flag that WAS passed still wins over both; one that was not passed
    // sets nothing, which is what makes the file usable at all.
    let mut flagged = config.clone();
    flagged.agent.model = Some("gemma4:26b".into());
    flagged.agent.base_url = Some("http://127.0.0.1:11434".into());
    let over = flagged.resolve_agent();
    assert_eq!(over.config.model, "gemma4:26b");
    assert_eq!(over.config.base_url, "http://127.0.0.1:11434");
    assert_eq!(over.config.compat, glade_gyld::Compat::Ollama);
    assert_eq!(
        over.config.max_output_tokens, 4242,
        "the file still holds everything no flag named"
    );

    // A file nobody can parse is a NOTE and the supplier still has a config.
    std::fs::write(&path, "{,}").unwrap();
    let broken = config.resolve_agent();
    assert_eq!(broken.notes.len(), 1, "{:?}", broken.notes);
    assert!(
        broken.notes[0].contains("did not decode"),
        "{:?}",
        broken.notes
    );
    assert_eq!(
        broken.config.max_output_tokens,
        glade_gyld::DEFAULT_MAX_OUTPUT_TOKENS,
        "nothing is taken from a file that did not decode"
    );

    let said = broken.says();
    assert!(said.contains(&broken.config.model), "{said}");
    assert!(!said.to_lowercase().contains("key"), "{said}");
}

/// The point is not the transport error — it is that there is one. A blocking
/// HTTP client asserts, in debug builds, that it is neither built nor called
/// from inside an async context; building it at attach panicked the supplier at
/// start-up, and this is the regression test for that.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_model_client_answers_with_data_from_a_blocking_task() {
    let tmp = Tmp::new("https");
    let key = tmp.path().join("api-key");
    std::fs::write(&key, "not-a-real-key\n").unwrap();
    key_mode_600(&key);

    let config = glade_gyld::ModelConfig {
        base_url: "http://127.0.0.1:1".into(),
        key_file: key,
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let client = glade_gyld::model::HttpsModelClient::new(config.clone());
    let request = ModelRequest {
        tools: Vec::new(),
        steps: Vec::new(),
        config,
        prompt: glade_gyld::Prompt {
            system: "the stance".into(),
            user: "why?".into(),
        },
        turns: Vec::new(),
    };
    let said = tokio::task::spawn_blocking(move || client.count_tokens(&request, &mut |_| {}))
        .await
        .expect("the blocking task did not panic")
        .expect_err("nothing is listening on port 1");
    assert!(said.contains("the model call failed"), "{said}");
}

// --------------------------------------------------------------------------
// The compatibility profile, against a SCRIPTED ENDPOINT
// --------------------------------------------------------------------------

/// A scripted HTTP endpoint: one thread, one canned answer per request, and
/// every request it was sent kept whole.
///
/// The [`ScriptedModel`] above stands in for the model CLIENT and so cannot say
/// anything about headers, statuses or retries — the very things the
/// compatibility profile is about. This stands in for the ENDPOINT instead, so
/// a 404 on `count_tokens`, a 400 on `strict` and the bearer header are
/// asserted on the bytes that actually went over a socket, with no network
/// beyond loopback and no dependency added.
mod endpoint {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// One request, as it arrived.
    #[derive(Clone, Debug)]
    pub struct Seen {
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: serde_json::Value,
    }

    impl Seen {
        /// One header, lowercased name, or `None` when it was not sent.
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }

        /// Whether the body carries `strict` anywhere in it.
        pub fn strict(&self) -> bool {
            self.body.to_string().contains("\"strict\"")
        }

        /// How many cache breakpoints the body carries.
        pub fn breakpoints(&self) -> usize {
            self.body.to_string().matches("cache_control").count()
        }
    }

    /// What the endpoint answers: status, content type, body.
    pub type Answer = (u16, &'static str, String);

    pub struct Endpoint {
        pub base_url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        addr: String,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Endpoint {
        /// Bind loopback on an OS-assigned port and answer with `reply`, which
        /// is given the request and how many came before it.
        pub fn serve<R>(reply: R) -> Endpoint
        where
            R: Fn(&Seen, usize) -> Answer + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
            let addr = listener.local_addr().expect("an address").to_string();
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let seen = seen.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    for incoming in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let mut socket = match incoming {
                            Ok(socket) => socket,
                            Err(_) => {
                                continue;
                            }
                        };
                        let request = match read_request(&socket) {
                            Some(request) => request,
                            None => {
                                continue;
                            }
                        };
                        let nth = {
                            let mut held = seen.lock().unwrap();
                            held.push(request.clone());
                            held.len() - 1
                        };
                        let (status, kind, body) = reply(&request, nth);
                        let head = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: \
                             {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes());
                        let _ = socket.write_all(body.as_bytes());
                        let _ = socket.flush();
                    }
                })
            };
            Endpoint {
                base_url: format!("http://{addr}"),
                seen,
                stop,
                addr,
                thread: Some(thread),
            }
        }

        /// Every request it was sent, oldest first.
        pub fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // One connection to wake `accept`, which is otherwise blocked.
            let _ = TcpStream::connect(&self.addr);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The request line, the headers and the body — enough HTTP to be an
    /// endpoint and no more.
    fn read_request(socket: &TcpStream) -> Option<Seen> {
        let clone = socket.try_clone().ok()?;
        let mut reader = BufReader::new(clone);
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut length = 0usize;
        loop {
            let mut held = String::new();
            if reader.read_line(&mut held).ok()? == 0 {
                break;
            }
            let held = held.trim_end().to_string();
            if held.is_empty() {
                break;
            }
            if let Some((name, value)) = held.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    length = value.parse().unwrap_or(0);
                }
                headers.push((name, value));
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).ok()?;
        Some(Seen {
            path,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        })
    }
}

/// A clean streamed answer, as an endpoint sends one.
const SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1200}}}"#,
    "\n\nevent: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","#,
    r#""text":"It is blocked."}}"#,
    "\n\nevent: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"#,
    r#""usage":{"output_tokens":42}}"#,
    "\n\n",
);

/// A key file the client can read, in a directory the caller owns.
fn agent_key(tmp: &Tmp, value: &str) -> PathBuf {
    let key = tmp.path().join("api-key");
    std::fs::write(&key, format!("{value}\n")).unwrap();
    key_mode_600(&key);
    key
}

/// A request aimed at `endpoint`, in `compat`.
fn request_to(base_url: &str, compat: glade_gyld::Compat, key: PathBuf) -> ModelRequest {
    ModelRequest {
        tools: Vec::new(),
        steps: Vec::new(),
        config: glade_gyld::ModelConfig {
            base_url: base_url.into(),
            compat,
            count_tokens: compat.counts_tokens(),
            key_file: key,
            timeout: Duration::from_secs(10),
            ..Default::default()
        },
        prompt: glade_gyld::Prompt {
            system: "the stance and the passages".into(),
            user: "why is this blocked?".into(),
        },
        turns: Vec::new(),
    }
}

/// Every note one call produced, in order.
fn notes_of(events: &[ModelEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Note(note) => Some(note.clone()),
            _ => None,
        })
        .collect()
}

/// Every text chunk one call produced.
fn text_of(events: &[ModelEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Text(chunk) => Some(chunk.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_ollama_profile_estimates_the_budget_sends_a_bearer_and_never_asks_for_a_count() {
    let tmp = Tmp::new("compat-ollama");
    let endpoint = endpoint::Endpoint::serve(|_seen, _nth| (200, "text/event-stream", SSE.into()));
    let request = request_to(
        &endpoint.base_url,
        glade_gyld::Compat::Ollama,
        agent_key(&tmp, "ollama"),
    );
    let client = glade_gyld::model::HttpsModelClient::new(request.config.clone());

    let mut events: Vec<ModelEvent> = Vec::new();
    let counted = client
        .count_tokens(&request, &mut |e| events.push(e))
        .expect("an estimate is not a failure");

    // Nothing went over the wire for the count: the profile already knows
    // there is no `count_tokens` there.
    assert!(endpoint.seen().is_empty(), "{:?}", endpoint.seen());
    assert!(counted > 0, "the estimate is of the body about to be sent");
    assert_eq!(counted, glade_gyld::estimate_tokens(&request));
    let notes = notes_of(&events);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("count_tokens"), "{notes:?}");
    assert!(notes[0].contains("ESTIMATE"), "the run SAYS so: {notes:?}");
    assert!(notes[0].contains(&counted.to_string()), "{notes:?}");

    let outcome = client
        .stream(&request, &mut |e| events.push(e))
        .expect("the stream");
    assert!(outcome.complete());
    assert_eq!(text_of(&events), "It is blocked.");

    let seen = endpoint.seen();
    assert_eq!(seen.len(), 1, "one request, and it was the stream");
    assert_eq!(seen[0].path, "/v1/messages");
    assert_eq!(seen[0].header("x-api-key"), Some("ollama"));
    assert_eq!(
        seen[0].header("authorization"),
        Some("Bearer ollama"),
        "Claude-shaped clients authenticate to these endpoints with a bearer"
    );
    assert_eq!(seen[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen[0].body["stream"], true);
    assert!(
        seen[0].strict() && seen[0].breakpoints() == 1,
        "the profile still TRIES both; it discovers a refusal, it does not assume one"
    );
}

#[test]
fn the_anthropic_profile_counts_with_the_endpoint_and_sends_no_bearer() {
    let tmp = Tmp::new("compat-anthropic");
    let endpoint = endpoint::Endpoint::serve(|seen, _nth| {
        if seen.path.ends_with("count_tokens") {
            return (200, "application/json", r#"{"input_tokens":1234}"#.into());
        }
        (200, "text/event-stream", SSE.into())
    });
    let request = request_to(
        &endpoint.base_url,
        glade_gyld::Compat::Anthropic,
        agent_key(&tmp, "sk-test"),
    );
    let client = glade_gyld::model::HttpsModelClient::new(request.config.clone());

    let mut events: Vec<ModelEvent> = Vec::new();
    let counted = client
        .count_tokens(&request, &mut |e| events.push(e))
        .expect("the count");
    assert_eq!(counted, 1234, "the endpoint's own number, not an estimate");
    assert!(notes_of(&events).is_empty(), "nothing was fallen back to");

    client
        .stream(&request, &mut |e| events.push(e))
        .expect("the stream");

    let seen = endpoint.seen();
    assert_eq!(seen.len(), 2, "the count, then the call");
    assert_eq!(seen[0].path, "/v1/messages/count_tokens");
    assert!(seen[0].body.get("max_tokens").is_none());
    assert_eq!(seen[1].path, "/v1/messages");
    for one in seen.iter() {
        assert_eq!(one.header("x-api-key"), Some("sk-test"));
        assert_eq!(
            one.header("authorization"),
            None,
            "the Anthropic path is unchanged, header for header"
        );
        assert!(one.strict(), "and carries `strict`");
    }
    assert_eq!(seen[1].breakpoints(), 1, "and its cache breakpoint");
    assert!(notes_of(&events).is_empty(), "nothing to say: {events:?}");
}

#[test]
fn a_404_on_count_tokens_becomes_an_estimate_and_is_asked_only_once() {
    let tmp = Tmp::new("compat-404");
    let endpoint = endpoint::Endpoint::serve(|seen, _nth| {
        if seen.path.ends_with("count_tokens") {
            return (404, "application/json", r#"{"error":"not found"}"#.into());
        }
        (200, "text/event-stream", SSE.into())
    });
    // The ANTHROPIC profile, so the 404 is discovered rather than assumed.
    let request = request_to(
        &endpoint.base_url,
        glade_gyld::Compat::Anthropic,
        agent_key(&tmp, "sk-test"),
    );
    let client = glade_gyld::model::HttpsModelClient::new(request.config.clone());

    let mut events: Vec<ModelEvent> = Vec::new();
    let counted = client
        .count_tokens(&request, &mut |e| events.push(e))
        .expect("a 404 is not a failure; it is a fact about the endpoint");
    assert_eq!(counted, glade_gyld::estimate_tokens(&request));
    let notes = notes_of(&events);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("404"), "{notes:?}");
    assert!(notes[0].contains("ESTIMATE"), "{notes:?}");

    // Learned: the second turn does not spend a round trip rediscovering it.
    let mut again: Vec<ModelEvent> = Vec::new();
    let second = client
        .count_tokens(&request, &mut |e| again.push(e))
        .expect("an estimate");
    assert_eq!(second, counted);
    assert_eq!(
        endpoint.seen().len(),
        1,
        "asked once, ever: {:?}",
        endpoint.seen()
    );
    let notes = notes_of(&again);
    assert_eq!(notes.len(), 1, "and it still SAYS so every turn: {notes:?}");
}

#[test]
fn a_400_on_strict_and_then_on_cache_control_is_retried_smaller_and_each_drop_is_a_note() {
    let tmp = Tmp::new("compat-400");
    let endpoint = endpoint::Endpoint::serve(|seen, _nth| {
        if seen.strict() {
            return (
                400,
                "application/json",
                r#"{"error":{"message":"tools.0: unexpected field `strict`"}}"#.into(),
            );
        }
        if seen.breakpoints() > 0 {
            return (
                400,
                "application/json",
                r#"{"error":{"message":"system.0: unexpected field `cache_control`"}}"#.into(),
            );
        }
        (200, "text/event-stream", SSE.into())
    });
    let request = request_to(
        &endpoint.base_url,
        glade_gyld::Compat::Ollama,
        agent_key(&tmp, "ollama"),
    );
    let client = glade_gyld::model::HttpsModelClient::new(request.config.clone());

    let mut events: Vec<ModelEvent> = Vec::new();
    let outcome = client
        .stream(&request, &mut |e| events.push(e))
        .expect("the answer still arrives");
    assert!(outcome.complete());
    assert_eq!(text_of(&events), "It is blocked.", "the reader is answered");

    // Three attempts: full, without `strict`, without either.
    let seen = endpoint.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[0].strict() && seen[0].breakpoints() == 1);
    assert!(!seen[1].strict() && seen[1].breakpoints() == 1);
    assert!(!seen[2].strict() && seen[2].breakpoints() == 0);
    assert_eq!(
        seen[2].body["messages"], seen[0].body["messages"],
        "only the markers went: the transcript is the same transcript"
    );
    assert_eq!(
        seen[2].body["tools"][0]["input_schema"], seen[0].body["tools"][0]["input_schema"],
        "and the schema is the same schema"
    );

    // Both drops are said, in the order they happened.
    let notes = notes_of(&events);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].contains("`strict`") && notes[0].contains("400"),
        "{notes:?}"
    );
    assert!(notes[1].contains("`cache_control`"), "{notes:?}");
    assert!(
        notes[1].contains("pays for its passages"),
        "a note says what it COST: {notes:?}"
    );

    // Learned: the next turn starts where the last one ended, with no retries.
    let mut again: Vec<ModelEvent> = Vec::new();
    client
        .stream(&request, &mut |e| again.push(e))
        .expect("the answer");
    assert_eq!(endpoint.seen().len(), 4, "one request, not three");
    let last = endpoint.seen().pop().expect("a request");
    assert!(!last.strict() && last.breakpoints() == 0);
    assert!(
        notes_of(&again).is_empty(),
        "a fact already learned is not re-announced every turn: {:?}",
        notes_of(&again)
    );
}

#[test]
fn an_endpoint_that_400s_whatever_it_is_sent_is_a_transport_failure_with_its_own_words() {
    let tmp = Tmp::new("compat-400-always");
    let endpoint = endpoint::Endpoint::serve(|_seen, _nth| {
        (
            400,
            "application/json",
            r#"{"error":{"message":"model \"nope\" not found"}}"#.into(),
        )
    });
    let request = request_to(
        &endpoint.base_url,
        glade_gyld::Compat::Ollama,
        agent_key(&tmp, "ollama"),
    );
    let client = glade_gyld::model::HttpsModelClient::new(request.config.clone());

    let mut events: Vec<ModelEvent> = Vec::new();
    let said = client
        .stream(&request, &mut |e| events.push(e))
        .expect_err("nothing this call could send was accepted");
    assert!(said.contains("400"), "{said}");
    assert!(
        said.contains("not found"),
        "the endpoint's own words: {said}"
    );
    assert_eq!(
        endpoint.seen().len(),
        glade_gyld::MAX_DEGRADATIONS + 1,
        "it is not retried at forever"
    );
    assert_eq!(notes_of(&events).len(), glade_gyld::MAX_DEGRADATIONS);
}

#[cfg(unix)]
mod key_modes {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    pub fn set_600(path: &Path) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[cfg(not(unix))]
mod key_modes {
    use std::path::Path;

    pub fn set_600(_path: &Path) {}
}

fn key_mode_600(path: &Path) {
    key_modes::set_600(path);
}

// ---- 3. a spawn failure answers as data ------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_host_failure_answers_as_data() {
    let tmp = Tmp::new("hostfail");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);

    let runner = Arc::new(Recorder {
        fail: Some("timed out after 200ms".into()),
        ..Default::default()
    });
    let _sup = serve_with(config_for(&url, gyld, bundle), runner, Arc::new(NoModel))
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let r = request(&requester, r#"{"verb":"rebuild"}"#).await;
    assert!(
        !r.ok && r.error.as_deref().unwrap_or("").contains("timed out"),
        "{r:?}"
    );
    assert!(r.output_dir.is_none(), "a failed run built nothing: {r:?}");

    requester.close().await;
    node.kill().await.ok();
}

// ---- 4. streaming output reaches a subscriber, in sequence -----------------

#[tokio::test(flavor = "multi_thread")]
async fn streaming_output_reaches_a_subscriber_in_sequence() {
    let tmp = Tmp::new("stream");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);

    let gate = Arc::new(Barrier::new(2));
    let runner = Arc::new(Recorder {
        lines: vec![
            ("stdout", "capturing base"),
            ("stdout", "wrote streams.json"),
            ("stderr", "note"),
        ],
        build: true,
        gate: Some(gate.clone()),
        ..Default::default()
    });
    let _sup = serve_with(config_for(&url, gyld, bundle), runner, Arc::new(NoModel))
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let accepted = request(&requester, r#"{"verb":"rebuild","stream_output":true}"#).await;
    assert!(
        accepted.ok && accepted.done == Some(false),
        "streaming accept: {accepted:?}"
    );
    let run_id = accepted.run_id.expect("run_id on the accept");

    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    sub.subscribe("ws-razel", "gyld.output", Some(run_id.as_bytes()))
        .await
        .unwrap();

    // The subscriber is up; let the run produce its lines.
    tokio::task::spawn_blocking(move || gate.wait())
        .await
        .unwrap();

    let s = sub.clone();
    let key = run_id.clone();
    let converged = poll(|| {
        let s = s.clone();
        let key = key.clone();
        async move {
            s.fold_log("ws-razel", "gyld.output", Some(key.as_bytes()))
                .await
                .iter()
                .any(|e| {
                    serde_json::from_slice::<GyldOutputRecord>(e)
                        .map(|r| r.done == Some(true))
                        .unwrap_or(false)
                })
        }
    })
    .await;
    assert!(
        converged,
        "the streaming output and its done marker reached the subscriber"
    );

    let entries = sub
        .fold_log("ws-razel", "gyld.output", Some(run_id.as_bytes()))
        .await;
    let records: Vec<GyldOutputRecord> = entries
        .iter()
        .filter_map(|e| serde_json::from_slice(e).ok())
        .collect();

    // Every record is keyed by the run, attributed, and numbered from one with
    // the terminal marker last.
    assert_eq!(
        records.len(),
        4,
        "three lines and one end marker: {records:?}"
    );
    assert!(records.iter().all(|r| r.run_id == run_id));
    assert!(records
        .iter()
        .all(|r| r.principal.as_deref() == Some("gianni")));
    assert_eq!(
        records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "sequence numbers run 1..n: {records:?}"
    );
    assert_eq!(records[0].line.as_deref(), Some("capturing base"));
    assert_eq!(records[0].stream, "stdout");
    assert_eq!(records[2].stream, "stderr");
    let end = records.last().unwrap();
    assert_eq!(end.stream, "end");
    assert_eq!(end.exit, Some(0));
    assert_eq!(end.done, Some(true));

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
}

// ---- 5. ONE real subprocess against a Gyld checkout ------------------------

/// The Gyld checkout the real-subprocess test runs against, if one is here.
fn gyld_checkout() -> Option<PathBuf> {
    let candidate = match std::env::var_os("GLADE_GYLD_TEST_GYLD_ROOT") {
        Some(v) => PathBuf::from(v),
        None => manifest().join("../../gyld-wz/gyld"),
    };
    if candidate.join("scripts/emit_decision_streams.py").is_file() {
        return Some(candidate);
    }
    None
}

#[test]
fn the_emit_host_answers_help_as_a_real_subprocess() {
    let root = match gyld_checkout() {
        Some(r) => r,
        None => {
            eprintln!("SKIP: no Gyld checkout (set GLADE_GYLD_TEST_GYLD_ROOT)");
            return;
        }
    };
    let python = PathBuf::from(glade_gyld::DEFAULT_PYTHON);
    if !python.exists() {
        eprintln!("SKIP: {} is absent", python.display());
        return;
    }
    let layout =
        glade_gyld::Layout::new(root.clone(), std::env::temp_dir().join("glade-gyld-help"));
    let plan = Plan {
        verb: "help".into(),
        write: None,
        merge: None,
        argv: vec![
            layout
                .script("emit_decision_streams.py")
                .display()
                .to_string(),
            "--help".into(),
        ],
        cwd: root,
        pythonpath: layout.pythonpath(),
        output_dir: None,
        read: None,
        consult: None,
        overlay: None,
        stream: None,
    };
    let limits = Limits {
        timeout: Duration::from_secs(60),
        max_output_bytes: 1 << 20,
    };
    let out = glade_gyld::exec::run_bounded(&python, &plan, limits, &mut |_, _| {})
        .expect("the emit host ran");
    assert_eq!(out.exit, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("usage: emit_decision_streams.py"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("--output"), "{}", out.stdout);
}

// ---- 6. the binary attaches, answers, and shuts down on SIGTERM ------------

#[tokio::test(flavor = "multi_thread")]
async fn binary_serves_and_shuts_down_on_sigterm() {
    let tmp = Tmp::new("bin");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);

    // `--python` names an interpreter that is not there, so the first build the
    // binary starts for itself on this fresh root fails at once and on every
    // machine: the point here is that a FAILED first build is data and the
    // supplier stays up answering, not what a real Gyld host would have done.
    let mut supplier = Command::new(env!("CARGO_BIN_EXE_glade-gyld"))
        .args(["--node", &url, "--gyld-root"])
        .arg(&gyld)
        .arg("--bundle-root")
        .arg(&bundle)
        .args([
            "--share",
            "ws-razel",
            "--principal",
            "tester",
            "--python",
            "/nonexistent/python3",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn glade-gyld binary");
    let pid = supplier.id().expect("binary pid");

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();

    // The binary attached: `list` answers, and once the failed first build has
    // ended that answer is the plain refusal again — the supplier never claims a
    // build is on its way when none is.
    let answered = poll(|| {
        let r = requester.clone();
        async move {
            match r
                .exchange("ws-razel", "gyld.ops", br#"{"verb":"list"}"#.to_vec())
                .await
            {
                Ok(o) if o.ok => {
                    let resp: GyldResponse =
                        serde_json::from_slice(&o.payload.unwrap_or_default()).unwrap_or_default();
                    !resp.ok
                        && resp.attributed_to.as_deref() == Some("tester")
                        && resp.error.as_deref() == Some(glade_gyld::NO_BUNDLE)
                }
                _ => false,
            }
        }
    })
    .await;
    assert!(answered, "the glade-gyld binary attached and answered");
    assert!(
        !bundle.join("latest.json").exists(),
        "a failed first build advertises nothing"
    );

    // It laid out its app-owned bundle root on the way, and touched nothing else.
    assert!(bundle.join("overlays").is_dir(), "the overlays tree exists");
    assert!(
        bundle.join("stage/examples").exists(),
        "the staging repository exists"
    );
    assert!(
        bundle.join("overlays/glade-decisions.gyld.py").exists(),
        "the checkout's examples seeded the overlays tree"
    );

    let killed = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("send SIGTERM");
    assert!(killed.success(), "sent SIGTERM");
    let status = tokio::time::timeout(Duration::from_secs(10), supplier.wait())
        .await
        .expect("binary exited after SIGTERM")
        .expect("wait");
    assert!(status.success(), "clean shutdown exit 0, got {status:?}");

    requester.close().await;
    node.kill().await.ok();
}

// ---- 7. a successful build publishes onto the value surfaces (step 4.2) ----

/// A runner that writes a small but complete emitted bundle into the plan's
/// output directory, so the publication path has something real to read.
struct BundleBuilder;

impl Runner for BundleBuilder {
    fn run(
        &self,
        plan: &Plan,
        _limits: Limits,
        _on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        let dir = plan.output_dir.clone().ok_or("this verb builds nothing")?;
        let lenses = dir.join("streams/base/lenses");
        std::fs::create_dir_all(&lenses).map_err(|e| e.to_string())?;
        std::fs::write(
            dir.join("streams.json"),
            br#"{"format":"gyld.streams.v1","streams":[{"id":"base"}]}"#,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(dir.join("streams/base/stream.json"), br#"{"id":"base"}"#)
            .map_err(|e| e.to_string())?;
        std::fs::write(
            dir.join("streams/base/decide-now.json"),
            br#"{"format":"gyld.decide-now.v1","questions":[]}"#,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(lenses.join("decisions.lens.json"), b"abc").map_err(|e| e.to_string())?;
        Ok(RunOutput {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_build_publishes_the_bundle_onto_the_value_surfaces() {
    let tmp = Tmp::new("publish");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);

    let _sup = serve_with(
        config_for(&url, gyld, bundle.clone()),
        Arc::new(BundleBuilder),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    for (id, key) in [
        ("gyld.streams", None),
        ("gyld.stream", Some("base")),
        ("gyld.decisions", Some("base")),
        ("gyld.lens", Some("base/decisions")),
    ] {
        sub.subscribe("ws-razel", id, key.map(str::as_bytes))
            .await
            .unwrap();
    }

    let built = request(&requester, r#"{"verb":"rebuild"}"#).await;
    assert!(built.ok, "{built:?}");
    let output_dir = built.output_dir.clone().expect("a build directory");

    // The four surfaces converge: three documents and one lens POINTER.
    let s = sub.clone();
    let converged = poll(|| {
        let s = s.clone();
        async move {
            s.fold_value("ws-razel", "gyld.lens", Some(b"base/decisions"))
                .await
                .is_some()
        }
    })
    .await;
    assert!(converged, "the lens pointer reached the subscriber");

    let listing = sub
        .fold_value("ws-razel", "gyld.streams", None)
        .await
        .expect("streams.json");
    assert!(
        String::from_utf8_lossy(&listing).contains("gyld.streams.v1"),
        "the listing itself"
    );

    let stream = sub
        .fold_value("ws-razel", "gyld.stream", Some(b"base"))
        .await
        .expect("stream.json");
    assert_eq!(String::from_utf8_lossy(&stream), r#"{"id":"base"}"#);

    let decisions = sub
        .fold_value("ws-razel", "gyld.decisions", Some(b"base"))
        .await
        .expect("decide-now.json");
    assert!(String::from_utf8_lossy(&decisions).contains("gyld.decide-now.v1"));

    let pointer = sub
        .fold_value("ws-razel", "gyld.lens", Some(b"base/decisions"))
        .await
        .unwrap();
    let pointer: glade_gyld::FilePointer =
        serde_json::from_slice(&pointer).expect("a file pointer");
    assert_eq!(
        pointer.bytes, 3,
        "the pointer carries the size, not the bytes"
    );
    assert_eq!(
        pointer.digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // The URL path is the bundle-root-relative path under the static base, and
    // it resolves to the file grazel serves.
    let relative = pointer
        .path
        .strip_prefix("/gyld/")
        .expect("the default static base");
    assert_eq!(
        bundle.join(relative),
        PathBuf::from(&output_dir).join("streams/base/lenses/decisions.lens.json")
    );
    assert!(
        bundle.join(relative).is_file(),
        "the pointed-at file is there"
    );

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
}

// ---- 8. the build already in the bundle root is published at ATTACH --------

/// A runner that refuses to run: the point of this test is that NO verb is
/// issued and no host is invoked, and the census still lands.
struct NeverRuns;

impl Runner for NeverRuns {
    fn run(
        &self,
        _plan: &Plan,
        _limits: Limits,
        _on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        panic!("the attach-time publication runs no host");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_already_in_the_bundle_root_is_published_when_the_supplier_attaches() {
    let tmp = Tmp::new("attach-publish");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    let seeded = seed_whole_bundle(&bundle);

    let _sup = serve_with(
        config_for(&url, gyld, bundle.clone()),
        Arc::new(NeverRuns),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    // The subscriber arrives AFTER the supplier attached, which is the case
    // that was broken: a page opened on a running composition, no Rebuild
    // pressed, nothing on `gyld.streams` for streams.json.
    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    for (id, key) in [
        ("gyld.streams", None),
        ("gyld.stream", Some("base")),
        ("gyld.decisions", Some("base")),
        ("gyld.lens", Some("base/decisions")),
    ] {
        sub.subscribe("ws-razel", id, key.map(str::as_bytes))
            .await
            .unwrap();
    }

    // Every surface, not just the first one. The publication is one append per
    // document — the census FIRST and the lens pointer LAST — so a census that
    // has landed says nothing about a pointer that has not, and `fold_value` is
    // a fold over what this session has SEEN, never a fetch that waits. Waiting
    // on the census alone and then reading the pointer straight out was a race
    // the parallel suite lost about one run in ten.
    let s = sub.clone();
    let landed = poll(|| {
        let s = s.clone();
        async move {
            for (id, key) in [
                ("gyld.streams", None),
                ("gyld.stream", Some(b"base".as_slice())),
                ("gyld.decisions", Some(b"base".as_slice())),
                ("gyld.lens", Some(b"base/decisions".as_slice())),
            ] {
                if s.fold_value("ws-razel", id, key).await.is_none() {
                    return false;
                }
            }
            true
        }
    })
    .await;
    assert!(landed, "the whole build landed with no verb issued at all");

    let listing = sub
        .fold_value("ws-razel", "gyld.streams", None)
        .await
        .unwrap();
    assert_eq!(
        listing,
        std::fs::read(seeded.join("streams.json")).unwrap(),
        "the bytes on the share are the build's own streams.json"
    );

    let stream = sub
        .fold_value("ws-razel", "gyld.stream", Some(b"base"))
        .await
        .expect("stream.json");
    assert_eq!(String::from_utf8_lossy(&stream), r#"{"id":"base"}"#);
    assert!(sub
        .fold_value("ws-razel", "gyld.decisions", Some(b"base"))
        .await
        .is_some());

    let pointer = sub
        .fold_value("ws-razel", "gyld.lens", Some(b"base/decisions"))
        .await
        .expect("a lens pointer");
    let pointer: glade_gyld::FilePointer = serde_json::from_slice(&pointer).unwrap();
    assert_eq!(
        pointer.path,
        "/gyld/builds/build-0000000000009/streams/base/lenses/decisions.lens.json"
    );

    // The build it found is the build it published: nothing was rebuilt, and
    // no second directory appeared.
    let builds: Vec<_> = std::fs::read_dir(bundle.join("builds"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(builds, vec![seeded], "the build was reused, never rebuilt");

    sub.close().await;
    node.kill().await.ok();
}

// ---- 9. a bundle root with NO build gets its first build at attach ---------

/// A runner that plays both halves of the bootstrap: it answers the discovery
/// run with the ids a Gyld checkout would print, and writes a whole bundle for
/// the build itself. `gate`, when it is there, holds the BUILD closed (never
/// the discovery) so the test can see the supplier answering mid-flight.
///
/// A rendezvous channel rather than a `Barrier`, because the two parties here
/// are not symmetric: the test reaches the gate on the strength of a REFUSAL
/// that the supplier raises before it has run anything, so a bootstrap that
/// never reaches the runner at all leaves the test as the only party. A
/// `Barrier` parked it there for good and hung the whole `cargo test` run;
/// this side of the gate is bounded and says what did not arrive.
struct Bootstrapper {
    plans: Mutex<Vec<Plan>>,
    gate: Option<SyncSender<()>>,
}

impl Runner for Bootstrapper {
    fn run(
        &self,
        plan: &Plan,
        _limits: Limits,
        on_line: &mut dyn FnMut(&str, &str),
    ) -> Result<RunOutput, String> {
        self.plans.lock().unwrap().push(plan.clone());
        let dir = match plan.output_dir.clone() {
            Some(d) => d,
            None => {
                // The discovery run: the checkout's own answer, on stdout.
                return Ok(RunOutput {
                    exit: 0,
                    stdout: "[\"fork-a\", \"stream-a\", \"stream-b\"]\n".into(),
                    stderr: String::new(),
                    truncated: false,
                });
            }
        };
        if let Some(gate) = self.gate.as_ref() {
            // A zero-capacity send returns when the test receives it, and errors
            // at once if the test is already gone, so the build is held exactly
            // as long as the test holds it and never a moment past the test.
            let _ = gate.send(());
        }
        on_line("stdout", "capturing base");
        let lenses = dir.join("streams/base/lenses");
        std::fs::create_dir_all(&lenses).map_err(|e| e.to_string())?;
        std::fs::write(
            dir.join("streams.json"),
            br#"{"format":"gyld.streams.v1","streams":[{"id":"base"},{"id":"architecture"},{"id":"fork-a"},{"id":"stream-a"},{"id":"stream-b"}]}"#,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(dir.join("streams/base/stream.json"), br#"{"id":"base"}"#)
            .map_err(|e| e.to_string())?;
        std::fs::write(lenses.join("decisions.lens.json"), b"abc").map_err(|e| e.to_string())?;
        Ok(RunOutput {
            exit: 0,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_bundle_root_gets_its_first_build_and_the_census_lands() {
    let tmp = Tmp::new("bootstrap");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    // No seed at all: this is a fresh data directory.
    assert!(!bundle.join("latest.json").exists());

    let (open, gate) = std::sync::mpsc::sync_channel::<()>(0);
    let runner = Arc::new(Bootstrapper {
        plans: Mutex::new(Vec::new()),
        gate: Some(open),
    });
    let _sup = serve_with(
        config_for(&url, gyld.clone(), bundle.clone()),
        runner.clone(),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();

    // The supplier keeps SERVING while it builds, and a verb that needs a
    // bundle is told the first build is on its way rather than that there is
    // none. `fork` needs no bundle and is accepted throughout.
    let refused = poll(|| {
        let r = requester.clone();
        async move {
            match r
                .exchange("ws-razel", "gyld.ops", br#"{"verb":"list"}"#.to_vec())
                .await
            {
                Ok(o) if o.ok => {
                    let resp: GyldResponse =
                        serde_json::from_slice(&o.payload.unwrap_or_default()).unwrap_or_default();
                    !resp.ok
                        && resp
                            .error
                            .as_deref()
                            .unwrap_or("")
                            .contains("the first build is in progress (run boot-1)")
                }
                _ => false,
            }
        }
    })
    .await;
    assert!(
        refused,
        "the refusal names the run in flight, not `no bundle has been built yet`"
    );

    let forked = request(
        &requester,
        r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a"}}"#,
    )
    .await;
    assert!(
        forked.ok,
        "fork needs no bundle and still works: {forked:?}"
    );

    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    sub.subscribe("ws-razel", "gyld.streams", None)
        .await
        .unwrap();

    // Let the first build finish — bounded, so a bootstrap that never reached
    // the runner is a named failure rather than a test that never returns.
    gate.recv_timeout(Duration::from_secs(60))
        .expect("the first build reached the runner");

    let s = sub.clone();
    let landed = poll(|| {
        let s = s.clone();
        async move {
            s.fold_value("ws-razel", "gyld.streams", None)
                .await
                .is_some()
        }
    })
    .await;
    assert!(landed, "the first build published its own census");

    let listing = sub
        .fold_value("ws-razel", "gyld.streams", None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listing).contains("\"stream-b\""),
        "the census is the first build's whole listing"
    );

    // The bundle root is a bundle root now: a stage, a build and the pointer.
    assert!(bundle.join("stage/examples").exists());
    let pointed = std::fs::read_to_string(bundle.join("latest.json")).expect("latest.json");
    assert!(pointed.contains("builds/build-"), "{pointed}");

    // The bootstrap is two host invocations and no more: ask the checkout which
    // streams it declares, then build exactly what it said. (The `fork` above is
    // the third plan the runner saw; it is the test's, not the bootstrap's.)
    let plans = runner.plans.lock().unwrap().clone();
    let emit = gyld
        .join("scripts/emit_decision_streams.py")
        .display()
        .to_string();
    let asked = plans.iter().position(|p| p.argv[0] == "-c");
    let built = plans.iter().position(|p| p.argv[0] == emit);
    assert!(
        asked == Some(0) && built.is_some() && asked < built,
        "the checkout is asked with its own discover, and only then built: {plans:?}"
    );
    assert_eq!(
        plans.iter().filter(|p| p.argv[0] == emit).count(),
        1,
        "one first build, never two"
    );
    let build = &plans[built.unwrap()];
    assert_eq!(
        build.argv,
        vec![
            emit,
            "--repository".into(),
            bundle.join("stage").display().to_string(),
            "--output".into(),
            build.output_dir.clone().unwrap().display().to_string(),
            "--architecture".into(),
            "--stream".into(),
            "fork-a".into(),
            "--stream".into(),
            "stream-a".into(),
            "--stream".into(),
            "stream-b".into(),
        ],
        "every declared stream reached the writer host"
    );

    // And now that a build exists, `list` is accepted with no Rebuild pressed.
    let listed = poll(|| {
        let r = requester.clone();
        async move {
            match r
                .exchange("ws-razel", "gyld.ops", br#"{"verb":"list"}"#.to_vec())
                .await
            {
                Ok(o) if o.ok => {
                    let resp: GyldResponse =
                        serde_json::from_slice(&o.payload.unwrap_or_default()).unwrap_or_default();
                    resp.ok && resp.stdout.contains("gyld.streams.v1")
                }
                _ => false,
            }
        }
    })
    .await;
    assert!(listed, "`list` is accepted the first time it is pressed");

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
}

// ---- 7. a refused write, over the real wire with the REAL Gyld hosts -------

/// Gyld's two ways of rejecting a notebook, each provoked by one substitution in
/// the shipped `stream-a` sample, and each verified here against the real hosts
/// rather than assumed:
///
/// * `Selects[VersionPin]` puts a QUESTION where an alternative is accepted. The
///   capture raises, `emit_decision_streams.py` writes
///   `streams/stream-a/validation.json` (`ROLE_TYPE_MISMATCH`) and exits 1, and no
///   `streams.json` is written at all — the STRUCTURAL class.
/// * `Selects[SdaxRs]` selects an alternative that exists and that the question
///   does not offer. The build SUCCEEDS, exit 0 and complete, and that stream's
///   own document is `ok:false` with `SELECTION_NOT_OFFERED` — the FINDINGS
///   class, the owner's-mistake class as data.
///
/// Both are read off the SAMPLE TEXT, in memory. Nothing here writes anywhere
/// near the read-only checkout.
const STRUCTURAL: (&str, &str) = ("Selects[BumpToCurrent]", "Selects[VersionPin]");
const FINDINGS: (&str, &str) = ("Selects[BumpToCurrent]", "Selects[SdaxRs]");

/// The shipped `stream-a` module, with one substitution — or none, for the valid
/// answer that has to land first so there is a notebook to leave unchanged.
fn sample_answer(gyld: &Path, swap: Option<(&str, &str)>) -> String {
    let text = std::fs::read_to_string(gyld.join("examples/glade-decisions-stream-a.gyld.py"))
        .expect("the shipped stream-a sample");
    match swap {
        Some((from, to)) => {
            assert!(text.contains(from), "the sample still says {from}");
            text.replace(from, to)
        }
        None => text,
    }
}

/// An `answer` envelope carrying that module text.
fn answer_envelope(overlay: &str) -> String {
    serde_json::json!({
        "verb": "answer",
        "stream_output": true,
        "args": { "stream": "stream-a", "overlay": overlay }
    })
    .to_string()
}

/// A real Gyld build is seconds, not milliseconds: wait longer than [`poll`] does.
async fn poll_slowly<F, Fut>(mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..600 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Send one streamed `answer` and fold the run's records once its terminal
/// record is on the log. Answers `(accept, terminal record)`.
async fn answered(
    requester: &GladeClient,
    sub: &GladeClient,
    overlay: &str,
) -> (GyldResponse, GyldOutputRecord) {
    let accept = request(requester, &answer_envelope(overlay)).await;
    assert!(accept.ok, "the accept is always ok: {accept:?}");
    let run_id = accept.run_id.clone().expect("run_id on the accept");
    sub.subscribe("ws-razel", "gyld.output", Some(run_id.as_bytes()))
        .await
        .unwrap();

    let ended = |sub: GladeClient, key: String| async move {
        sub.fold_log("ws-razel", "gyld.output", Some(key.as_bytes()))
            .await
            .iter()
            .filter_map(|e| serde_json::from_slice::<GyldOutputRecord>(e).ok())
            .find(|r| r.done == Some(true))
    };
    let converged = poll_slowly(|| {
        let (sub, key) = (sub.clone(), run_id.clone());
        async move { ended(sub, key).await.is_some() }
    })
    .await;
    assert!(converged, "the run {run_id} closed with a terminal record");
    let end = ended(sub.clone(), run_id.clone())
        .await
        .expect("the terminal record");
    (accept, end)
}

/// The whole rule, end to end, against the hosts the owner really runs: a write
/// that makes things WORSE is refused and leaves no trace.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_gyld_rejects_is_refused_and_the_notebook_is_put_back() {
    let gyld = match gyld_checkout() {
        Some(r) => r,
        None => {
            eprintln!("SKIP: no Gyld checkout (set GLADE_GYLD_TEST_GYLD_ROOT)");
            return;
        }
    };
    let python = PathBuf::from(glade_gyld::DEFAULT_PYTHON);
    if !python.exists() {
        eprintln!("SKIP: {} is absent", python.display());
        return;
    }

    let tmp = Tmp::new("refused-real");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let bundle = tmp.path().join("bundle");
    let decisions = tmp.path().join("decisions");
    std::fs::create_dir_all(&bundle).unwrap();

    let mut config = config_for(&url, gyld.clone(), bundle.clone());
    config.layout = config
        .layout
        .clone()
        .with_decisions_root(Some(decisions.clone()));
    // A real host prints a summary of some size and takes seconds.
    config.limits = Limits {
        timeout: Duration::from_secs(180),
        max_output_bytes: 1 << 20,
    };
    let _sup = serve_with(
        config,
        Arc::new(glade_gyld::PythonRunner::new(python)),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();

    // The supplier makes its own first build on a bundle root that has none. Wait
    // for it: everything below is measured against the bundle it leaves.
    let landed = poll_slowly(|| {
        let bundle = bundle.clone();
        async move { bundle.join("latest.json").is_file() }
    })
    .await;
    assert!(landed, "the supplier's own first build landed");
    attached(&requester).await;
    let notebook = decisions.join("glade-decisions-stream-a.gyld.py");

    // 1. A VALID answer, so there is a notebook on disk whose bytes a refusal has
    //    to leave alone. The terminal record names the file, which is what lets a
    //    desk say "saved" only once it is true.
    let (accept, end) = answered(&requester, &sub, &sample_answer(&gyld, None)).await;
    assert_eq!(end.exit, Some(0), "a valid answer builds: {end:?}");
    assert_eq!(end.refusal, None, "nothing to refuse: {end:?}");
    assert_eq!(
        end.overlay_file.as_deref(),
        Some(notebook.display().to_string().as_str()),
        "the terminal record names the notebook the run really left"
    );
    assert!(accept.ok && accept.done == Some(false));
    let was = std::fs::read(&notebook).expect("the notebook the valid answer left");
    let latest_was = std::fs::read_to_string(bundle.join("latest.json")).unwrap();
    let builds_were = build_names(&bundle);

    // 2. The STRUCTURAL class: the capture raises, the build is abandoned.
    let (_, end) = answered(&requester, &sub, &sample_answer(&gyld, Some(STRUCTURAL))).await;
    let refusal = end.refusal.clone().expect("a refusal: {end:?}");
    assert_eq!(refusal.code, "ROLE_TYPE_MISMATCH", "{refusal:?}");
    assert_eq!(refusal.stream, "stream-a");
    assert!(refusal.restored, "the notebook was put back: {refusal:?}");
    assert!(!refusal.message.is_empty());
    assert_eq!(end.overlay_file, None, "a refused run saved nothing");
    assert_eq!(
        std::fs::read(&notebook).unwrap(),
        was,
        "the notebook is byte for byte what it was before the refused write"
    );
    assert_eq!(
        std::fs::read_to_string(bundle.join("latest.json")).unwrap(),
        latest_was,
        "no new latest"
    );
    assert_eq!(
        build_names(&bundle),
        builds_were,
        "the abandoned build directory is gone"
    );

    // 3. The FINDINGS class: exit 0, a COMPLETE bundle, and the written stream
    //    invalid in it. This is the one a `latest_build` fallback would otherwise
    //    adopt all by itself.
    let (_, end) = answered(&requester, &sub, &sample_answer(&gyld, Some(FINDINGS))).await;
    assert_eq!(
        end.exit,
        Some(0),
        "the findings class builds clean: {end:?}"
    );
    let refusal = end.refusal.clone().expect("a refusal on a clean exit");
    assert_eq!(refusal.code, "SELECTION_NOT_OFFERED", "{refusal:?}");
    assert_eq!(refusal.stream, "stream-a");
    assert!(refusal.restored);
    assert!(
        refusal.message.contains("does not offer"),
        "{}",
        refusal.message
    );
    assert_eq!(end.overlay_file, None);
    assert_eq!(std::fs::read(&notebook).unwrap(), was, "byte for byte");
    assert_eq!(
        std::fs::read_to_string(bundle.join("latest.json")).unwrap(),
        latest_was
    );
    assert_eq!(
        build_names(&bundle),
        builds_were,
        "a refused-but-COMPLETE build is removed, so the fallback cannot adopt it"
    );

    // And the sample that ships with Gyld was never touched by any of it.
    assert!(
        sample_answer(&gyld, None).contains(STRUCTURAL.0),
        "the read-only checkout still holds its committed sample"
    );

    // 4. A SYNCHRONOUS refusal answers the same thing as data: `ok:false`, the
    //    message as `error`, and Gyld's own document as `validation` (4.7).
    let envelope = serde_json::json!({
        "verb": "answer",
        "args": { "stream": "stream-a", "overlay": sample_answer(&gyld, Some(FINDINGS)) }
    })
    .to_string();
    let out = request(&requester, &envelope).await;
    assert!(!out.ok, "a refused write is not ok: {out:?}");
    assert!(out.output_dir.is_none(), "it advertises no build");
    let document = out.validation.clone().expect("the validation document");
    assert_eq!(document["format"], "gyld.validation.v1");
    assert_eq!(document["code"], "SELECTION_NOT_OFFERED");
    assert_eq!(document["ok"], false);
    assert_eq!(
        out.error.as_deref(),
        document["message"].as_str(),
        "the message the desk shows is Gyld's own"
    );
    assert_eq!(std::fs::read(&notebook).unwrap(), was, "byte for byte");
    assert_eq!(build_names(&bundle), builds_were);

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
}

// ---- 8. a notebook that holds SEVERAL answers, over the real Gyld hosts ----

/// One fragment, as the decide window composes it (GyldGrythPlugins.md 4.8): the
/// imports the added classes need, the class text and the members that place it.
///
/// Nothing here is the SUPPLIER's spelling — it is the desk's, written out in a
/// test because the desk is the other side of this contract and the Gyld host is
/// the only thing that reads it.
fn ruling_fragment(question: &str, alternative: &str) -> serde_json::Value {
    let symbol = format!("{question}Ruling");
    let member = match question {
        "VersionPin" => "version_pin_ruling",
        "ScopeModel" => "scope_model_ruling",
        other => panic!("no member name written down for {other}"),
    };
    serde_json::json!({
        "imports": {
            "decision_stream_concepts": ["Decides", "Ruling", "Selects"],
            "glade_decisions": [question, alternative],
            "gyld": ["use"],
        },
        "classes": format!(
            "class {symbol}(Ruling):\n    \"\"\"2026-09-21, owner: ruled from the desk.\"\"\"\n\
             \x20   principal = \"gianni\"\n    stamp = \"2026-09-21T00:00:00Z\"\n\
             \x20   decides = Decides[{question}]\n    selects = Selects[{alternative}]\n"
        ),
        "members": [format!("{member} = use({symbol})")],
    })
}

/// A fragment `answer` envelope, streamed like every other build.
fn fragment_envelope(stream: &str, fragment: serde_json::Value) -> String {
    serde_json::json!({
        "verb": "answer",
        "stream_output": true,
        "args": { "stream": stream, "fragment": fragment },
    })
    .to_string()
}

/// Send one streamed request and fold the run's records once the terminal one is
/// on the log. [`answered`]'s body, over any envelope rather than an `answer` on
/// `stream-a`.
async fn streamed(
    requester: &GladeClient,
    sub: &GladeClient,
    envelope: &str,
) -> (GyldResponse, Vec<GyldOutputRecord>) {
    let accept = request(requester, envelope).await;
    assert!(accept.ok, "the accept is always ok: {accept:?}");
    let run_id = accept.run_id.clone().expect("run_id on the accept");
    sub.subscribe("ws-razel", "gyld.output", Some(run_id.as_bytes()))
        .await
        .unwrap();
    let folded = |sub: GladeClient, key: String| async move {
        sub.fold_log("ws-razel", "gyld.output", Some(key.as_bytes()))
            .await
            .iter()
            .filter_map(|e| serde_json::from_slice::<GyldOutputRecord>(e).ok())
            .collect::<Vec<GyldOutputRecord>>()
    };
    let converged = poll_slowly(|| {
        let (sub, key) = (sub.clone(), run_id.clone());
        async move { folded(sub, key).await.iter().any(|r| r.done == Some(true)) }
    })
    .await;
    assert!(converged, "the run {run_id} closed with a terminal record");
    (accept, folded(sub.clone(), run_id).await)
}

/// The terminal record of a folded run.
fn terminal(records: &[GyldOutputRecord]) -> GyldOutputRecord {
    records
        .iter()
        .find(|r| r.done == Some(true))
        .cloned()
        .expect("a terminal record")
}

/// The `decide-now.json` the latest build emitted for one stream.
fn decide_now(bundle: &Path, stream: &str) -> serde_json::Value {
    let pointer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(bundle.join("latest.json")).unwrap())
            .unwrap();
    let build = bundle.join(pointer["output_dir"].as_str().expect("an output_dir"));
    let path = build.join("streams").join(stream).join("decide-now.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("cannot read {}: {e}", path.display());
    }))
    .expect("the decide-now document")
}

/// The whole point of the fragment path, over the hosts the owner really runs: ONE
/// notebook holds as many answers as he makes.
///
/// Four facts, in the order a desk meets them:
///
/// 1. two fragment answers into one fresh notebook both stand — both classes, both
///    members, and the build's decide-now list carrying both rulings;
/// 2. answering the same question twice is refused `NOTEBOOK_ALREADY_HAS`, with the
///    notebook byte for byte what it was;
/// 3. a merge that Gyld then REJECTS is refused by the existing outcome rule and
///    the notebook is put back — the merge itself is not a second way in past it;
/// 4. a fragment answer on a shipped SAMPLE is copy-on-write: the merge reads the
///    sample through the staging link, the write lands in the decisions root, and
///    the checkout is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn one_notebook_holds_as_many_fragment_answers_as_the_owner_makes() {
    let gyld = match gyld_checkout() {
        Some(r) => r,
        None => {
            eprintln!("SKIP: no Gyld checkout (set GLADE_GYLD_TEST_GYLD_ROOT)");
            return;
        }
    };
    let python = PathBuf::from(glade_gyld::DEFAULT_PYTHON);
    if !python.exists() {
        eprintln!("SKIP: {} is absent", python.display());
        return;
    }

    let tmp = Tmp::new("fragment-real");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let bundle = tmp.path().join("bundle");
    let decisions = tmp.path().join("decisions");
    std::fs::create_dir_all(&bundle).unwrap();

    let mut config = config_for(&url, gyld.clone(), bundle.clone());
    config.layout = config
        .layout
        .clone()
        .with_decisions_root(Some(decisions.clone()));
    config.limits = Limits {
        timeout: Duration::from_secs(180),
        max_output_bytes: 1 << 20,
    };
    let _sup = serve_with(
        config,
        Arc::new(glade_gyld::PythonRunner::new(python)),
        Arc::new(NoModel),
    )
    .await
    .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    let sub = GladeClient::new("subscriber");
    sub.connect(&url).await.unwrap();
    let landed = poll_slowly(|| {
        let bundle = bundle.clone();
        async move { bundle.join("latest.json").is_file() }
    })
    .await;
    assert!(landed, "the supplier's own first build landed");
    attached(&requester).await;

    // A fresh notebook of the owner's own: a link over the base declaration, which
    // is what the desk's landing flow presses first.
    let linked = serde_json::json!({
        "verb": "link",
        "stream_output": true,
        "args": { "parent": "base", "stream": "notes-a", "note": "the owner's notes" },
    })
    .to_string();
    let (_, records) = streamed(&requester, &sub, &linked).await;
    let end = terminal(&records);
    assert_eq!(end.exit, Some(0), "the link landed: {end:?}");
    let notebook = decisions.join("glade-decisions-notes-a.gyld.py");
    assert!(notebook.is_file(), "the link left the owner a notebook");

    // 1. TWO answers, into that one notebook.
    let (_, records) = streamed(
        &requester,
        &sub,
        &fragment_envelope("notes-a", ruling_fragment("VersionPin", "BumpToCurrent")),
    )
    .await;
    let end = terminal(&records);
    assert_eq!(
        end.refusal, None,
        "the first fragment answer stands: {end:?}"
    );
    assert_eq!(end.exit, Some(0), "{end:?}");
    assert_eq!(
        end.overlay_file.as_deref(),
        Some(notebook.display().to_string().as_str())
    );
    // The merged module is the merge host's stdout and never reaches the log: one
    // summary line goes out in its place.
    let lines: Vec<String> = records.iter().filter_map(|r| r.line.clone()).collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("merged VersionPinRuling into")),
        "one summary line, not the module: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("gyld-stream-record")),
        "the module's own text is not forwarded to the log: {lines:?}"
    );

    let (_, records) = streamed(
        &requester,
        &sub,
        &fragment_envelope("notes-a", ruling_fragment("ScopeModel", "NodeTrust")),
    )
    .await;
    let end = terminal(&records);
    assert_eq!(
        end.refusal, None,
        "the SECOND answer into the same notebook stands: {end:?}"
    );
    assert_eq!(end.exit, Some(0), "{end:?}");

    let text = std::fs::read_to_string(&notebook).expect("the notebook");
    for held in [
        "class VersionPinRuling(Ruling):",
        "class ScopeModelRuling(Ruling):",
        "    version_pin_ruling = use(VersionPinRuling)",
        "    scope_model_ruling = use(ScopeModelRuling)",
    ] {
        assert!(
            text.contains(held),
            "the notebook is missing {held}:\n{text}"
        );
    }
    let listed = decide_now(&bundle, "notes-a");
    let rulings = listed["rulings"].as_array().expect("the rulings list");
    assert_eq!(
        rulings.len(),
        2,
        "the build lists both rulings: {rulings:#?}"
    );
    let was = std::fs::read(&notebook).unwrap();
    let builds_were = build_names(&bundle);
    let latest_was = std::fs::read_to_string(bundle.join("latest.json")).unwrap();

    // 2. The same question twice: refused by name, and nothing moved.
    let (_, records) = streamed(
        &requester,
        &sub,
        &fragment_envelope("notes-a", ruling_fragment("VersionPin", "StayOnLock")),
    )
    .await;
    let end = terminal(&records);
    let refusal = end.refusal.clone().expect("a refused merge");
    assert_eq!(refusal.code, "NOTEBOOK_ALREADY_HAS", "{refusal:?}");
    assert!(
        refusal.message.contains("already has VersionPinRuling")
            && refusal.message.contains("Rebuild"),
        "the owner is told what to do about it: {}",
        refusal.message
    );
    assert!(
        refusal.restored,
        "the notebook is as it was: a desk reads false as `could NOT be put back \
         - check it`, which is an alarm and not a fact here"
    );
    assert_eq!(end.overlay_file, None, "a refused merge saved nothing");
    assert_eq!(
        std::fs::read(&notebook).unwrap(),
        was,
        "the notebook is byte for byte what it was"
    );
    assert_eq!(build_names(&bundle), builds_were, "and nothing was built");
    assert_eq!(
        std::fs::read_to_string(bundle.join("latest.json")).unwrap(),
        latest_was
    );

    // 3. A merge Gyld accepts and then REJECTS: `LifecycleComposition` does not
    //    offer `BumpToCurrent`, so the merged module builds clean and that stream's
    //    own document is `ok:false`. The existing outcome rule refuses it and puts
    //    the notebook back, which the merge path did not get to bypass.
    let rejected = serde_json::json!({
        "imports": {
            "decision_stream_concepts": ["Decides", "Ruling", "Selects"],
            "glade_decisions": ["BumpToCurrent", "LifecycleComposition"],
            "gyld": ["use"],
        },
        "classes": "class LifecycleRuling(Ruling):\n    \"\"\"an answer nobody offered.\"\"\"\n\
                    \x20   principal = \"gianni\"\n    stamp = \"2026-09-21T00:00:00Z\"\n\
                    \x20   decides = Decides[LifecycleComposition]\n\
                    \x20   selects = Selects[BumpToCurrent]\n",
        "members": ["lifecycle_ruling = use(LifecycleRuling)"],
    });
    let (_, records) = streamed(&requester, &sub, &fragment_envelope("notes-a", rejected)).await;
    let end = terminal(&records);
    let refusal = end.refusal.clone().expect("a refused write");
    assert_eq!(refusal.code, "SELECTION_NOT_OFFERED", "{refusal:?}");
    assert_eq!(refusal.stream, "notes-a");
    assert!(
        refusal.restored,
        "the merged notebook was put back: {refusal:?}"
    );
    assert_eq!(
        std::fs::read(&notebook).unwrap(),
        was,
        "byte for byte what it was before the merge"
    );
    assert_eq!(build_names(&bundle), builds_were);

    // 4. Copy-on-write on a SHIPPED sample: the merge reads the checkout's own
    //    module through the staging link and the write lands in the owner's folder.
    let sample = gyld.join("examples/glade-decisions-stream-a.gyld.py");
    let shipped = std::fs::read(&sample).expect("the shipped sample");
    let (_, records) = streamed(
        &requester,
        &sub,
        &fragment_envelope("stream-a", ruling_fragment("ScopeModel", "NodeTrust")),
    )
    .await;
    let end = terminal(&records);
    assert_eq!(
        end.refusal, None,
        "an answer on a shipped sample stands: {end:?}"
    );
    let owned = decisions.join("glade-decisions-stream-a.gyld.py");
    assert_eq!(
        end.overlay_file.as_deref(),
        Some(owned.display().to_string().as_str()),
        "the notebook is the owner's copy, in his folder"
    );
    let copy = std::fs::read_to_string(&owned).expect("the owner's copy");
    assert!(
        copy.contains("class ScopeModelRuling(Ruling):")
            && copy.contains("class VersionPinRuling(Ruling):"),
        "the merge read the sample and added to it:\n{copy}"
    );
    assert_eq!(
        std::fs::read(&sample).unwrap(),
        shipped,
        "the sample that ships with Gyld is never touched"
    );
    assert_eq!(
        std::fs::read_link(bundle.join("overlays/glade-decisions-stream-a.gyld.py")).unwrap(),
        owned,
        "the staging tree reads the owner's copy from here on"
    );

    // No fragment document is left behind anywhere: each one is written
    // immediately before its host and removed immediately after.
    let left: Vec<String> = std::fs::read_dir(bundle.join("requests"))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "fragments left on disk: {left:?}");

    sub.close().await;
    requester.close().await;
    node.kill().await.ok();
}

/// Every directory under `builds/`, sorted — the listing a refusal must not add
/// to.
fn build_names(bundle: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(bundle.join("builds"))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Two overlapping writing verbs both end in a CONSISTENT state.
///
/// The exchange handler is already serialised by the kit, but a STREAMED run is
/// accepted at once and settles later on its own task. Without the write gate a
/// second `answer` arriving mid-run would write its notebook and the first run's
/// refusal would put the FIRST notebook back over it — the second answer silently
/// lost, and the bundle agreeing with neither.
///
/// The second one is refused as DATA and writes nothing, and the desk goes on
/// answering everything else while the first run finishes. The first run here
/// FAILS, which is the case that would have done the damage.
#[tokio::test(flavor = "multi_thread")]
async fn two_overlapping_answers_both_end_in_a_consistent_state() {
    let tmp = Tmp::new("overlap");
    let (mut node, port) = boot(&tmp).await;
    let url = format!("ws://127.0.0.1:{port}");
    let (gyld, bundle) = roots(&tmp);
    seed_bundle(&bundle);
    let decisions = tmp.path().join("decisions");

    let runner = Arc::new(Recorder {
        build: true,
        // The first run dawdles, so the second answer arrives while it is in
        // flight — and it is the one that fails.
        hold_first: Some(Duration::from_millis(1200)),
        exits: Mutex::new(vec![1, 0]),
        ..Default::default()
    });
    let mut config = config_for(&url, gyld, bundle.clone());
    config.layout = config
        .layout
        .clone()
        .with_decisions_root(Some(decisions.clone()));
    let _sup = serve_with(config, runner.clone(), Arc::new(NoModel))
        .await
        .unwrap();

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();
    attached(&requester).await;

    let envelope = |text: &str| {
        serde_json::json!({
            "verb": "answer",
            "stream_output": true,
            "args": { "stream": "stream-a", "overlay": text }
        })
        .to_string()
    };
    let notebook = decisions.join("glade-decisions-stream-a.gyld.py");

    // The first answer is accepted and its run is now inside the host.
    let first = request(&requester, &envelope("# the first ruling")).await;
    assert!(first.ok, "{first:?}");
    let run_id = first.run_id.clone().expect("a run id");
    assert!(
        poll(|| async { runner.count() >= 1 }).await,
        "the first run reached the host"
    );
    assert_eq!(
        std::fs::read_to_string(&notebook).unwrap(),
        "# the first ruling\n"
    );

    // The second answer arrives WHILE that run is in flight: refused as data,
    // naming the run to wait for, and it wrote nothing.
    let second = request(&requester, &envelope("# the second ruling")).await;
    assert!(!second.ok, "a second write mid-run is refused: {second:?}");
    let said = second.error.clone().unwrap_or_default();
    assert!(
        said.contains("already in flight") && said.contains(&run_id),
        "the refusal names the run to wait for: {said}"
    );
    assert_eq!(
        std::fs::read_to_string(&notebook).unwrap(),
        "# the first ruling\n",
        "the refused write never touched the notebook"
    );
    assert_eq!(runner.count(), 1, "and never reached a host");

    // The desk is still answering everything else while the run finishes — the
    // reason the second write is refused rather than left to block the exchange.
    let listed = request(&requester, r#"{"verb":"list"}"#).await;
    assert!(listed.ok, "a read verb is unaffected: {listed:?}");

    // The first run lands: it FAILED, so its notebook is put back — and back is
    // ABSENT, because there was none before it.
    let settled = poll(|| {
        let notebook = notebook.clone();
        async move { notebook.symlink_metadata().is_err() }
    })
    .await;
    assert!(
        settled,
        "the failed write was put back: {:?}",
        std::fs::read_to_string(&notebook)
    );
    assert!(
        !bundle
            .join("overlays/glade-decisions-stream-a.gyld.py")
            .exists(),
        "and the staging tree holds no link to a notebook that is gone"
    );

    // The gate is free again: the next answer goes through and stands.
    let third = request(&requester, &envelope("# the third ruling")).await;
    assert!(third.ok, "{third:?}");
    let stands =
        poll(|| {
            let notebook = notebook.clone();
            async move {
                std::fs::read_to_string(&notebook).ok().as_deref() == Some("# the third ruling\n")
            }
        })
        .await;
    assert!(stands, "the write after the refusal is the one that stands");
    assert_eq!(
        std::fs::read_link(bundle.join("overlays/glade-decisions-stream-a.gyld.py")).unwrap(),
        notebook,
        "and the staging tree reads it"
    );

    requester.close().await;
    node.kill().await.ok();
}
