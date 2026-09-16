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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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
        if let Some(gate) = self.gate.as_ref() {
            gate.wait();
        }
        if let Some(e) = self.fail.as_deref() {
            return Err(e.to_string());
        }
        for (stream, line) in self.lines.iter() {
            on_line(stream, line);
        }
        if self.build {
            if let Some(dir) = plan.output_dir.as_ref() {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join("streams.json"), "{\"format\":\"gyld.streams.v1\"}")
                    .unwrap();
            }
        }
        Ok(RunOutput {
            exit: self.exit,
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
    stop_reason: &'static str,
    declined: Option<Declined>,
    output_tokens: u64,
    transport: Option<&'static str>,
    calls: AtomicU64,
}

impl Default for ScriptedModel {
    fn default() -> ScriptedModel {
        ScriptedModel {
            counted: 1200,
            chunks: Vec::new(),
            stop_reason: glade_gyld::END_TURN,
            declined: None,
            output_tokens: 42,
            transport: None,
            calls: AtomicU64::new(0),
        }
    }
}

impl ModelClient for ScriptedModel {
    fn count_tokens(&self, _request: &ModelRequest) -> Result<u64, String> {
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
        Ok(ModelOutcome {
            stop_reason: self.stop_reason.into(),
            declined: self.declined.clone(),
            output_tokens: self.output_tokens,
            ..Default::default()
        })
    }
}

/// The model for every test that is not about the model: it panics if a verb
/// ever reaches it, because no other verb should.
struct NoModel;

impl ModelClient for NoModel {
    fn count_tokens(&self, _request: &ModelRequest) -> Result<u64, String> {
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

/// Fold the ASK surface for one conversation, waiting for the turn's close.
async fn ask_records(sub: &GladeClient, conversation: &str) -> Vec<GyldAskRecord> {
    let s = sub.clone();
    let key = conversation.to_string();
    let closed = poll(|| {
        let s = s.clone();
        let key = key.clone();
        async move {
            s.fold_log("ws-razel", "gyld.ask", Some(key.as_bytes()))
                .await
                .iter()
                .any(|e| {
                    serde_json::from_slice::<GyldAskRecord>(e)
                        .map(|r| r.done == Some(true))
                        .unwrap_or(false)
                })
        }
    })
    .await;
    assert!(closed, "the turn on {conversation} closed");
    sub.fold_log("ws-razel", "gyld.ask", Some(conversation.as_bytes()))
        .await
        .iter()
        .filter_map(|e| serde_json::from_slice(e).ok())
        .collect()
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

/// The REAL HTTPS client, driven where the supplier drives it: on a blocking
/// task, against a port nothing is listening on.
///
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
        config,
        prompt: glade_gyld::Prompt {
            system: "the stance".into(),
            user: "why?".into(),
        },
    };
    let said = tokio::task::spawn_blocking(move || client.count_tokens(&request))
        .await
        .expect("the blocking task did not panic")
        .expect_err("nothing is listening on port 1");
    assert!(said.contains("the model call failed"), "{said}");
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
    assert!(landed, "the census landed with no verb issued at all");

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
struct Bootstrapper {
    plans: Mutex<Vec<Plan>>,
    gate: Option<Arc<Barrier>>,
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
            gate.wait();
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

    let gate = Arc::new(Barrier::new(2));
    let runner = Arc::new(Bootstrapper {
        plans: Mutex::new(Vec::new()),
        gate: Some(gate.clone()),
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

    gate.wait(); // let the first build finish

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
