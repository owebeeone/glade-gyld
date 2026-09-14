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
//!   5. the `glade-gyld` BINARY attaches, answers, and shuts down on SIGTERM.

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
    serve_with, GyldConfig, GyldOutputRecord, GyldResponse, Limits, Plan, RunOutput, Runner,
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
    let _sup = serve_with(config_for(&url, gyld, bundle), runner.clone())
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
    let _sup = serve_with(config_for(&url, gyld, bundle), runner)
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
    let _sup = serve_with(config_for(&url, gyld, bundle), runner)
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

    let mut supplier = Command::new(env!("CARGO_BIN_EXE_glade-gyld"))
        .args(["--node", &url, "--gyld-root"])
        .arg(&gyld)
        .arg("--bundle-root")
        .arg(&bundle)
        .args(["--share", "ws-razel", "--principal", "tester"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn glade-gyld binary");
    let pid = supplier.id().expect("binary pid");

    let requester = GladeClient::new("requester");
    requester.connect(&url).await.unwrap();

    // The binary attached: `list` answers, and with no bundle built yet that
    // answer is a readable refusal rather than a hang.
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
                        && resp.error.as_deref().unwrap_or("").contains("no bundle")
                }
                _ => false,
            }
        }
    })
    .await;
    assert!(answered, "the glade-gyld binary attached and answered");

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

    let _sup = serve_with(config_for(&url, gyld, bundle.clone()), Arc::new(NeverRuns))
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
