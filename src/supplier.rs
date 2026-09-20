//! The gyld supplier — the authority-side module standing behind the Gyld
//! decision-stream command surface. It attaches over the wire as an ordinary
//! authority session through [`glade_client`] (no node internals, P00-a) and
//! serves both node mechanisms:
//!
//! * **exchange** (`(share, glade_id)`, default `(ws-razel, gyld.ops)`) — the
//!   verb surface. Each `ExchangeReq` payload is a [`GyldRequest`]; the answer is
//!   a [`GyldResponse`]. Allow-listed verbs map to exactly one Gyld host
//!   invocation each, run against the CONFIGURED roots; a refused verb, a bad
//!   envelope, a path leaving the bundle root, a spawn error and a timeout are
//!   all failure as DATA.
//! * **log** (`(share, output_id)`, default `(ws-razel, gyld.output)`) — long-op
//!   output. A `stream:true` request answers immediately with `{run_id,
//!   done:false}` and the run's stdout and stderr lines are APPENDED as ops
//!   keyed by `run_id`, closed by a `{done:true, exit}` marker.
//!
//! App-owned storage: `--bundle-root` is the app's, never derived from a
//! request, and `--gyld-root` is only ever READ (the scripts are run out of it
//! and its examples seed the overlays tree). A synchronous mutating verb holds
//! the exchange for as long as the run takes, bounded by the timeout; the UI
//! sends a build with `stream_output: true` instead.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use glade_client::supplier::{Supplier, SupplierConfig, SupplierSurface};
use glade_client::GladeClient;
use glade_wire::generated::ExchangeReq;

use crate::agent::{self, AgentOverrides, Resolved};
use crate::ask::{self, AgentState, AskDraft, Consultation};
use crate::bundle::{self, Layout};
use crate::conversation::{self, Ledger};
use crate::envelope::{GyldAskRecord, GyldOutputRecord, GyldRequest, GyldResponse};
use crate::exec::{Limits, PythonRunner, RunOutput, Runner};
use crate::model::{self, ModelClient, ModelConfig, ModelEvent, ModelRequest};
use crate::prompt::{self, Prompt};
use crate::publish::{self, Surfaces};
use crate::sources::{self, ResolvedSource};
use crate::tools;
use crate::toolset;
use crate::verbs::{self, Plan};

/// The default surfaces a gyld supplier stands behind (`gyld-app.glade`).
pub const DEFAULT_SHARE: &str = "ws-razel";
pub const DEFAULT_GLADE_ID: &str = "gyld.ops";
pub const DEFAULT_OUTPUT_ID: &str = "gyld.output";
/// The ask agent's reply surface, keyed by CONVERSATION and not by run id
/// (GyldAskAgent.md sections 4 and 6).
pub const DEFAULT_ASK_ID: &str = "gyld.ask";
/// The interpreter the Gyld hosts require: the default `python3` is 3.10 and
/// fails on them.
pub const DEFAULT_PYTHON: &str = "/opt/homebrew/bin/python3.13";

/// Everything the supplier needs to attach and serve.
#[derive(Clone, Debug)]
pub struct GyldConfig {
    pub node_url: String,
    pub share: String,
    pub glade_id: String,
    pub output_id: String,
    /// The log surface a consultation's reply lands on, keyed by conversation.
    pub ask_id: String,
    pub layout: Layout,
    /// The value surfaces a successful build publishes onto, and the static
    /// base the lens pointers are written against (step 4.2).
    pub surfaces: Surfaces,
    pub principal: Option<String>,
    pub limits: Limits,
    /// The ask agent, as the FLAGS set it (GyldAskAgent.md section 7).
    ///
    /// Only the flags: a field nobody passed is `None` and stays `None`, so
    /// `<bundle-root>/agent/config.json` and the environment are not overruled
    /// by a default that was never chosen ([`crate::agent`]). The effective
    /// configuration is [`GyldConfig::resolve_agent`], taken afresh at attach
    /// and at every call.
    pub agent: AgentOverrides,
}

impl GyldConfig {
    /// A config with the defaulted surfaces and bounds, given the node and the
    /// two roots.
    pub fn new(
        node_url: impl Into<String>,
        gyld_root: PathBuf,
        bundle_root: PathBuf,
    ) -> GyldConfig {
        GyldConfig {
            node_url: node_url.into(),
            share: DEFAULT_SHARE.into(),
            glade_id: DEFAULT_GLADE_ID.into(),
            output_id: DEFAULT_OUTPUT_ID.into(),
            ask_id: DEFAULT_ASK_ID.into(),
            layout: Layout::new(gyld_root, bundle_root),
            surfaces: Surfaces::default(),
            principal: None,
            limits: Limits::default(),
            agent: AgentOverrides::default(),
        }
    }

    /// The effective agent configuration, read NOW: the config file under the
    /// app-owned bundle root, the environment over it, the flags over both.
    ///
    /// Taken afresh every time, which is the point. grazel spawns this supplier
    /// with a fixed argument list, so the file is the only channel a running
    /// desk has — and a file that were read once at attach would need a
    /// restart of the whole app to change a model.
    pub fn resolve_agent(&self) -> Resolved {
        agent::resolve(&self.layout.bundle_root, &self.agent)
    }

    /// The key file this supplier reads when the environment carries no key.
    /// The path is the APP's, never a request's: it is `--agent-key-file`, the
    /// config file's own `key_file`, or the bundle root's `agent/api-key`.
    pub fn key_file(&self) -> PathBuf {
        self.resolve_agent().config.key_file
    }

    /// The model configuration one turn is made with.
    pub fn model_config(&self) -> ModelConfig {
        self.resolve_agent().config
    }
}

/// The agent's readiness, read once per request and handed to the PURE planner
/// as data (GyldAskAgent.md sections 4 and 7).
///
/// PRESENCE only. Whether a key exists is a boolean; the key VALUE is read by
/// the model client at the moment of the call and by nothing else, so it never
/// reaches a plan, a prompt, a record or a log line.
fn agent_state(config: &GyldConfig, latest: Option<&std::path::Path>) -> AgentState {
    let key_file = config.key_file();
    let named = |name: &str| {
        std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    };
    let key = named(ask::KEY_ENV) || named(agent::AUTH_TOKEN_ENV) || key_file.is_file();
    let (index, streams) = match latest {
        Some(dir) => {
            let listed = std::fs::read(dir.join("streams.json"))
                .map(|bytes| publish::stream_ids(&bytes))
                .unwrap_or_default();
            (dir.join(ask::SOURCES_FILE).is_file(), listed)
        }
        None => (false, Vec::new()),
    };
    AgentState {
        key,
        key_file,
        index,
        streams,
    }
}

/// A live gyld supplier: an attached authority session serving the verb
/// exchange. Hold it for the process lifetime; [`GyldSupplier::shutdown`] is the
/// clean teardown.
pub struct GyldSupplier {
    #[allow(dead_code)]
    client: GladeClient,
    supplier: Supplier,
}

impl GyldSupplier {
    /// Stop reattaching and close the session (the SIGTERM path).
    pub async fn shutdown(&self) {
        self.supplier.detach_all().await;
    }
}

/// Connect, attach as the gyld authority, and serve, with the real Python
/// runner and the real HTTPS model client.
pub async fn serve(config: GyldConfig, python: PathBuf) -> io::Result<GyldSupplier> {
    // The effective configuration, SAID at attach: which endpoint and which
    // model this desk is about to be answered by. Never the key, and never
    // whether there is one — that is the refusal's business, per request.
    let resolved = config.resolve_agent();
    eprintln!("glade-gyld: agent {}", resolved.says());
    // And which tools this desk gets, and where the network ones may go. Hosts,
    // counts and the SOURCE of the github token: no key, no token value and no
    // page. The token is discovered here, once, for the life of the process.
    eprintln!(
        "glade-gyld: agent {}",
        toolset::says(&resolved.config.tools, crate::github::discovered())
    );
    for note in resolved.notes.iter() {
        eprintln!("glade-gyld: agent config: {note}");
    }
    let model = model::HttpsModelClient::new(resolved.config);
    serve_with(config, Arc::new(PythonRunner::new(python)), Arc::new(model)).await
}

/// Connect, attach and serve with a caller-supplied runner and model client.
/// The tests drive this one with a recording runner and a scripted model, so
/// the whole verb path is exercised with no interpreter, no Gyld checkout and
/// no network in sight.
pub async fn serve_with(
    config: GyldConfig,
    runner: Arc<dyn Runner>,
    model: Arc<dyn ModelClient>,
) -> io::Result<GyldSupplier> {
    let config = Arc::new(config);
    let client = GladeClient::new(format!("glade-gyld:{}:{}", config.share, config.glade_id));
    client.connect(&config.node_url).await?;

    let supplier = Supplier::attach(
        client.clone(),
        SupplierConfig {
            principal: config.principal.clone(),
            ..Default::default()
        },
    );

    let first = Arc::new(FirstBuild::new());
    let handler = make_handler(
        client.clone(),
        config.clone(),
        runner.clone(),
        model,
        Handle::current(),
        first.clone(),
    );
    supplier
        .serve_exchange(
            SupplierSurface::new(&config.share, &config.glade_id, "exchange"),
            handler,
        )
        .await?;

    // Serving NOW, and only then is the bundle root's own state acted on: the
    // supplier answers throughout, whether it is publishing a build it found or
    // making the first one.
    match at_attach(&config.layout) {
        AtAttach::Publish(dir) => {
            // A build made by a previous session of this data directory, or
            // seeded by hand, is a build a mount should see: without this it sat
            // there unpublished until somebody pressed Rebuild.
            spawn_publish(client.clone(), config.clone(), Handle::current(), dir);
        }
        AtAttach::Bootstrap => {
            spawn_first_build(
                client.clone(),
                config.clone(),
                runner,
                Handle::current(),
                first,
            );
        }
    }

    Ok(GyldSupplier { client, supplier })
}

/// What the supplier does with the bundle root it finds when it starts serving.
/// A pure reading of the root: no request has been answered yet, and nothing
/// here writes.
#[derive(Debug, Clone, PartialEq)]
enum AtAttach {
    /// A build is already there — publish it onto the value shares.
    Publish(PathBuf),
    /// The root holds no build at all.
    Bootstrap,
}

/// Read the bundle root and decide. [`bundle::latest_build`] is the supplier's
/// own authority for "which build is current": `latest.json` first, and failing
/// that the newest `builds/` directory holding a `streams.json`, so a
/// hand-seeded root with no pointer is still a root with a build.
fn at_attach(layout: &Layout) -> AtAttach {
    match bundle::latest_build(layout) {
        Some(dir) => AtAttach::Publish(dir),
        None => AtAttach::Bootstrap,
    }
}

/// The run id the supplier's own first build takes. Deliberately not `run-N`:
/// nobody asked for this run, so it is not numbered among the ones that were.
pub const FIRST_BUILD_RUN_ID: &str = "boot-1";

/// The first build the supplier makes for itself, and whether it is still
/// running.
///
/// While it is, a verb that needs a bundle is refused with a message that names
/// the run instead of the flat [`verbs::NO_BUNDLE`]: one IS on its way, and a UI
/// that is told so can wait for it rather than conclude the root is broken.
#[derive(Debug)]
struct FirstBuild {
    run_id: String,
    running: std::sync::atomic::AtomicBool,
}

impl FirstBuild {
    fn new() -> FirstBuild {
        FirstBuild {
            run_id: FIRST_BUILD_RUN_ID.to_string(),
            running: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn begin(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Landed, or failed: either way it is no longer on its way, and a verb
    /// that still finds no bundle gets the plain refusal back.
    fn ended(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Say what the no-bundle refusal really means right now. Every other
    /// refusal passes through untouched.
    fn explain(&self, refusal: String) -> String {
        if refusal == verbs::NO_BUNDLE && self.running.load(Ordering::SeqCst) {
            return format!(
                "the first build is in progress (run {}); nothing has landed yet",
                self.run_id
            );
        }
        refusal
    }
}

/// Build the exchange handler. It is a synchronous `Fn` (the kit's contract) and
/// never returns `Err`, so the WIRE `ExchangeRes.ok` stays `true` and the PAYLOAD
/// carries success or failure.
fn make_handler(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    model: Arc<dyn ModelClient>,
    handle: Handle,
    first: Arc<FirstBuild>,
) -> impl Fn(&ExchangeReq) -> Result<Vec<u8>, String> + Send + Sync + 'static {
    let runs = Arc::new(AtomicU64::new(0));
    // One running total per conversation, for the supplier's lifetime
    // (GyldAskAgent.md section 7).
    let ledger = Arc::new(Ledger::default());
    move |req: &ExchangeReq| -> Result<Vec<u8>, String> {
        let response = answer(
            &client,
            &config,
            &runner,
            &model,
            &ledger,
            &handle,
            &runs,
            &first,
            &req.payload,
        );
        Ok(response.to_bytes())
    }
}

/// Parse, plan, prepare, then run or accept. Every branch resolves to a
/// [`GyldResponse`].
#[allow(clippy::too_many_arguments)]
fn answer(
    client: &GladeClient,
    config: &Arc<GyldConfig>,
    runner: &Arc<dyn Runner>,
    model: &Arc<dyn ModelClient>,
    ledger: &Arc<Ledger>,
    handle: &Handle,
    runs: &Arc<AtomicU64>,
    first: &Arc<FirstBuild>,
    payload: &[u8],
) -> GyldResponse {
    let request = match GyldRequest::parse(payload) {
        Ok(r) => r,
        Err(e) => {
            return GyldResponse::failed(e, config.principal.clone());
        }
    };
    // Attribution: the request's principal, else the supplier's configured one.
    let who = request
        .principal
        .clone()
        .or_else(|| config.principal.clone());

    if let Err(e) = bundle::ensure_stage(&config.layout) {
        return GyldResponse::failed(format!("bundle root unusable: {e}"), who);
    }
    let latest = bundle::latest_build(&config.layout);
    let stamp = bundle::build_stamp();
    let agent = agent_state(config, latest.as_deref());
    let plan = match verbs::plan(&config.layout, &request, latest.as_deref(), &stamp, &agent) {
        Ok(p) => p,
        Err(e) => {
            // `fork` and `link` need no bundle and are unaffected; the five that
            // do are told the first build is on its way rather than that there
            // is none.
            return GyldResponse::failed(first.explain(e), who);
        }
    };
    let run_id = format!("run-{}", runs.fetch_add(1, Ordering::SeqCst) + 1);

    if let Err(e) = write_overlay(&plan) {
        return GyldResponse::failed(e, who);
    }

    // `explain` runs no host: it consults. A consult plan carries no argv at
    // all, so nothing about it can reach a runner.
    //
    // It is ALWAYS a streaming run, whatever `stream_output` said: a
    // consultation is model time, and its reply is a stream by nature. The
    // accept answer carries the run id, and the reply lands on the log surface.
    if let Some(consult) = plan.consult.clone() {
        spawn_consult(
            client.clone(),
            config.clone(),
            model.clone(),
            ledger.clone(),
            handle.clone(),
            run_id.clone(),
            consult,
            who.clone(),
        );
        return GyldResponse::accepted(run_id, who);
    }

    // `list` is the one verb with no subprocess: it reads the current bundle's
    // stream listing straight off the app-owned disk.
    if let Some(path) = plan.read.clone() {
        return match read_bounded(&path, config.limits.max_output_bytes) {
            Ok(text) => GyldResponse::ran(run_id, 0, text, String::new(), None, who),
            Err(e) => GyldResponse::failed(e, who),
        };
    }

    if request.stream {
        spawn_stream(
            client.clone(),
            config.clone(),
            runner.clone(),
            handle.clone(),
            run_id.clone(),
            plan,
            who.clone(),
        );
        return GyldResponse::accepted(run_id, who);
    }

    match runner.run(&plan, config.limits, &mut |_, _| {}) {
        Ok(out) => {
            let dir = finish(client, config, handle, &plan, &out);
            let named = dir.as_ref().map(|d| d.display().to_string());
            GyldResponse::ran(run_id, out.exit, out.stdout, out.stderr, named, who)
        }
        Err(e) => GyldResponse::failed(e, who),
    }
}

/// Ground one consultation (GyldAskAgent.md sections 5 and 7).
///
/// Read the build's own source index, resolve the tags this record and its
/// ruling cite, and compose the prompt. One log line says how it went, with
/// both counts: a citation that resolves to nothing is a thing to be VISIBLE
/// about, in the log as well as in the answer.
///
/// An index that is absent, unreadable or of another format is a refusal as
/// data, exactly like every other failure here.
fn ground(consult: &Consultation) -> Result<(Vec<ResolvedSource>, Prompt), String> {
    let index = sources::read(&consult.sources)?;
    let resolved = index.resolve(&consult.context);
    let (found, missing) = sources::counted(&resolved);
    eprintln!(
        "glade-gyld: explain {} on {} ({}): {found} source tag(s) resolved, {missing} unresolved",
        consult.context.record.slot, consult.context.stream, consult.conversation
    );
    let prompt = prompt::compose(&consult.context, &resolved);
    Ok((resolved, prompt))
}

/// Write the planned overlay module, refusing to clobber one unless the plan
/// says so. A write failure is data.
///
/// NEVER THROUGH A SYMLINK. `overlays/` holds one seed link per file of the Gyld
/// checkout's `examples`, and `std::fs::write` follows a link: a write to
/// `overlays/glade-decisions-stream-a.gyld.py` used to land in the owner's
/// checkout, which the containment check could not see because it is lexical and
/// the path it was handed really is under the bundle root. So the text goes to a
/// sibling temporary file and is RENAMED over the target — `rename` replaces the
/// link itself instead of following it, and makes the write atomic into the
/// bargain: a reader of that name sees the old module or the new one, never half
/// of either.
///
/// The `exists` test does not follow a link either ([`std::fs::symlink_metadata`]):
/// a seed link IS something at that name, and an unforced write must refuse it
/// rather than ask what it points at.
fn write_overlay(plan: &Plan) -> Result<(), String> {
    let write = match plan.write.as_ref() {
        Some(w) => w,
        None => {
            return Ok(());
        }
    };
    if !write.force && write.path.symlink_metadata().is_ok() {
        return Err(format!(
            "{} exists; nothing is overwritten",
            write.path.display()
        ));
    }
    if let Some(parent) = write.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let temp = bundle::sibling_temp(&write.path);
    std::fs::write(&temp, &write.text)
        .map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, &write.path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot write {}: {e}", write.path.display())
    })
}

/// Record a successful build as the bundle root's latest, publish its documents
/// onto the value surfaces, and answer with the directory it built. A failed run
/// advertises nothing: the previous bundle stands untouched and nothing is
/// published over it.
fn finish(
    client: &GladeClient,
    config: &Arc<GyldConfig>,
    handle: &Handle,
    plan: &Plan,
    out: &RunOutput,
) -> Option<PathBuf> {
    let dir = plan.output_dir.as_ref()?;
    if out.exit != 0 || !dir.join("streams.json").is_file() {
        return None;
    }
    if let Err(e) = bundle::write_latest(&config.layout, dir) {
        eprintln!("glade-gyld: could not record the latest build: {e}");
    }
    spawn_publish(client.clone(), config.clone(), handle.clone(), dir.clone());
    Some(dir.clone())
}

/// Publish the build's documents onto the value surfaces (step 4.2), off the
/// exchange's own thread: the answer already carried the build directory, and a
/// mount converges when the ops land.
///
/// The ONE publication path. A build the supplier just ran, a build it found in
/// the bundle root when it attached and the first build it made for itself all
/// arrive here, so all three land the same documents and log the same line.
fn spawn_publish(
    client: GladeClient,
    config: Arc<GyldConfig>,
    handle: Handle,
    output_dir: PathBuf,
) {
    handle.spawn(async move {
        let plan = publish::publications(&config.layout, &output_dir, &config.surfaces);
        for note in plan.notes.iter() {
            eprintln!("glade-gyld: not published: {note}");
        }
        for publication in plan.publications.iter() {
            let key = publication.key.as_deref().map(str::as_bytes);
            let appended = client
                .append(
                    &config.share,
                    &publication.glade_id,
                    "value",
                    publication.payload.clone(),
                    key,
                )
                .await;
            if let Err(e) = appended {
                eprintln!(
                    "glade-gyld: could not publish {} {:?}: {e}",
                    publication.glade_id, publication.key
                );
            }
        }
        if !plan.publications.is_empty() {
            eprintln!(
                "glade-gyld: published {} ({} streams)",
                named(&config.layout, &output_dir),
                plan.streams
            );
        }
    });
}

/// A build directory as the bundle root names it — `builds/<stamp>` — which is
/// what `latest.json` records and what the static path serves it under. The
/// absolute path is the fallback for a directory that is somehow not under the
/// root, so a log line never silently drops where it was.
fn named(layout: &Layout, output_dir: &std::path::Path) -> String {
    output_dir
        .strip_prefix(&layout.bundle_root)
        .unwrap_or(output_dir)
        .display()
        .to_string()
}

/// Read a bundle document, bounded. A document larger than the budget is a
/// refusal rather than a giant exchange payload.
fn read_bounded(path: &std::path::Path, max: usize) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() as usize > max {
        return Err(format!(
            "{} is {} bytes; the exchange budget is {max} (fetch it over the static path)",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Accept a streaming run and answer at once: [`stream_run`] on its own task.
fn spawn_stream(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
) {
    let inner = handle.clone();
    handle.spawn(stream_run(client, config, runner, inner, run_id, plan, who));
}

/// Run the plan on a blocking task, appending every output line to the log
/// surface keyed by `run_id`, then a terminal `{done:true, exit}` record.
/// Best effort: an append failure (a link drop mid-run) is dropped, because the
/// exchange answer already carried the run id.
///
/// Resolves when the run has landed and its terminal record is on the log, so a
/// caller that must know when a build finished — the first build — can await it.
async fn stream_run(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
) {
    let inner = handle;
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
    let limits = config.limits;
    let work = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || {
            let result = runner.run(&plan, limits, &mut |stream, line| {
                let _ = tx.send((stream.to_string(), line.to_string()));
            });
            (plan, result)
        })
    };

    let mut seq: u64 = 0;
    while let Some((stream, line)) = rx.recv().await {
        seq += 1;
        let record = GyldOutputRecord::line(&run_id, seq, &who, &stream, line);
        append(&client, &config, &run_id, &record).await;
    }

    let exit = match work.await {
        Ok((plan, Ok(out))) => {
            finish(&client, &config, &inner, &plan, &out);
            out.exit
        }
        Ok((_, Err(e))) => {
            seq += 1;
            let record = GyldOutputRecord::line(&run_id, seq, &who, "stderr", e);
            append(&client, &config, &run_id, &record).await;
            -1
        }
        Err(e) => {
            seq += 1;
            let record = GyldOutputRecord::line(
                &run_id,
                seq,
                &who,
                "stderr",
                format!("run task failed: {e}"),
            );
            append(&client, &config, &run_id, &record).await;
            -1
        }
    };
    seq += 1;
    append(
        &client,
        &config,
        &run_id,
        &GyldOutputRecord::end(&run_id, seq, &who, exit),
    )
    .await;
}

/// Accept a consultation and answer at once: [`consult_run`] on its own task.
#[allow(clippy::too_many_arguments)]
fn spawn_consult(
    client: GladeClient,
    config: Arc<GyldConfig>,
    model: Arc<dyn ModelClient>,
    ledger: Arc<Ledger>,
    handle: Handle,
    run_id: String,
    consult: Consultation,
    who: Option<String>,
) {
    handle.spawn(consult_run(
        client, config, model, ledger, run_id, consult, who,
    ));
}

/// Run one consultation and stream its reply.
///
/// The shape is [`stream_run`]'s, and deliberately: ground and call on a
/// BLOCKING task, append each chunk to the log as it arrives, close with a
/// terminal record carrying the exit. A refusal — no index, an over-budget
/// turn, a transport failure — is one line on the log and a non-zero exit,
/// never a hang and never a panic.
///
/// The partial text of a turn the output budget stopped is KEPT and said to be
/// partial: half an answer that says it is half an answer is data; half an
/// answer presented as a whole one is not.
async fn consult_run(
    client: GladeClient,
    config: Arc<GyldConfig>,
    model: Arc<dyn ModelClient>,
    ledger: Arc<Ledger>,
    run_id: String,
    consult: Consultation,
    who: Option<String>,
) {
    let conversation = consult.conversation.clone();
    let question = consult.context.question.trim().to_string();

    // The transcript IS the log share (section 6): the supplier reads back its
    // own records for this conversation and replays them as prior turns. This
    // happens BEFORE anything of this turn is appended, so what comes back is
    // exactly the turns that came before.
    let prior = conversation::turns(
        &client
            .fold_log(&config.share, &config.ask_id, Some(conversation.as_bytes()))
            .await,
    );
    let mut seq: u64 = 1;
    append_ask(
        &client,
        &config,
        &conversation,
        &GyldAskRecord::question(&run_id, seq, &who, &conversation, question),
    )
    .await;

    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    // Read at every call, not once at attach: the config file is the only
    // channel a running desk has, so a model changed there takes effect on the
    // next question rather than on the next restart.
    let resolved = config.resolve_agent();
    let budget = resolved.config.max_output_tokens;
    // What resolving the configuration had to say — a file that did not decode,
    // a setting nobody has heard of — is the run's business too: a desk served
    // by the wrong model can read why here, next to the answer.
    for note in resolved.notes.iter() {
        seq += 1;
        append_ask(
            &client,
            &config,
            &conversation,
            &GyldAskRecord::note(&run_id, seq, &who, &conversation, note.clone()),
        )
        .await;
    }
    let model_config = resolved.config;
    let drafted_by = model_config.model.clone();
    // The two roots a source index measures itself against, carried onto the
    // blocking task with everything else the tools need.
    let layout = config.layout.clone();
    let spent = ledger.spent(&conversation);
    let work =
        tokio::task::spawn_blocking(move || -> Result<crate::model::ModelOutcome, String> {
            let (sources, prompt) = ground(&consult)?;
            // The tools this desk allows, over the build this consultation was
            // grounded in — so a tool's answer and a citation can never name
            // two different snapshots (GyldAskAgent.md 11.6).
            let context = tools::ToolContext::beside(&consult.sources, &layout);
            // The allow-list a desk that wrote none means, resolved against
            // what IS configured: the local tools always, and a network tool
            // only where the thing it needs is already there (11.2, 11.7).
            let token = crate::github::discovered();
            let policy = model_config
                .tools
                .allowing(toolset::on_by_default(&model_config.tools, token));
            let (registry, said) = tools::ToolRegistry::build(
                &policy,
                toolset::offered(&context, &model_config.tools, token),
            );
            for note in said.into_iter() {
                let _ = tx.send(Reply::Note(note));
            }
            // The citations first, so a reader sees what the answer is grounded in
            // before the prose arrives — and sees it even when the call then fails.
            for source in sources.iter() {
                let _ = tx.send(Reply::Citation(
                    serde_json::to_value(source).unwrap_or_default(),
                ));
            }
            let request = ModelRequest {
                config: model_config,
                prompt,
                turns: prior,
                tools: registry.declarations(),
                steps: Vec::new(),
            };
            model::consult(model.as_ref(), &request, spent, &registry, &mut |event| {
                let reply = match event {
                    // A fallback the call had to make. It rides the same
                    // channel as the answer so it lands in the records in the
                    // ORDER it happened — before the prose it weakened.
                    ModelEvent::Note(note) => Reply::Note(note),
                    ModelEvent::Text(chunk) => Reply::Answer(chunk),
                    // The draft is READ here, against the envelope this
                    // consultation resolved, so an alternative the envelope
                    // does not offer is marked unresolved rather than bent onto
                    // one it does.
                    ModelEvent::Draft(input) => {
                        Reply::Draft(AskDraft::parse(&input, &consult.context, &drafted_by))
                    }
                    // A tool the agent reached for, and what it answered. Both
                    // ride the same channel as the prose, so they land in the
                    // records in the ORDER they happened — which is what lets a
                    // follow-up replay the turn as it ran (11.4).
                    ModelEvent::ToolCall(call) => Reply::ToolCall(call),
                    ModelEvent::ToolResult(answered) => Reply::ToolResult(answered),
                };
                let _ = tx.send(reply);
            })
            .map_err(|refusal| refusal.says())
        });

    // A draft that did not decode is not a draft: nothing is offered, and the
    // turn's close is where that is said.
    let mut malformed: Option<String> = None;
    while let Some(reply) = rx.recv().await {
        let record = match reply {
            Reply::Citation(source) => {
                GyldAskRecord::citation(&run_id, seq + 1, &who, &conversation, source)
            }
            Reply::Answer(chunk) => {
                GyldAskRecord::answer(&run_id, seq + 1, &who, &conversation, chunk)
            }
            Reply::Note(note) => GyldAskRecord::note(&run_id, seq + 1, &who, &conversation, note),
            Reply::ToolCall(call) => {
                GyldAskRecord::tool_call(&run_id, seq + 1, &who, &conversation, call)
            }
            Reply::ToolResult(answered) => {
                GyldAskRecord::tool_result(&run_id, seq + 1, &who, &conversation, answered)
            }
            Reply::Draft(Ok(draft)) => GyldAskRecord::draft(
                &run_id,
                seq + 1,
                &who,
                &conversation,
                serde_json::to_value(&draft).unwrap_or_default(),
            ),
            Reply::Draft(Err(reason)) => {
                malformed = Some(reason);
                continue;
            }
        };
        seq += 1;
        append_ask(&client, &config, &conversation, &record).await;
    }

    let (exit, said) = match work.await {
        Ok(Ok(outcome)) => {
            // What the turn cost joins the conversation's running total, so the
            // NEXT turn is measured against what this one actually spent.
            ledger.spend(&conversation, outcome.tokens());
            (outcome.exit(), outcome.says(budget))
        }
        Ok(Err(refused)) => (1, Some(refused)),
        Err(e) => (-1, Some(format!("the consultation task failed: {e}"))),
    };
    // The prose still stands; the offer the reader asked for did not arrive, so
    // the turn did not end clean.
    let (exit, said) = match malformed {
        Some(reason) => {
            let exit = if exit == 0 { 1 } else { exit };
            match said {
                Some(already) => (exit, Some(format!("{already}; {reason}"))),
                None => (exit, Some(reason)),
            }
        }
        None => (exit, said),
    };
    seq += 1;
    let end = GyldAskRecord::end(&run_id, seq, &who, &conversation, exit, said);
    append_ask(&client, &config, &conversation, &end).await;
}

/// One thing a consultation produces, on its way to a record.
enum Reply {
    Citation(serde_json::Value),
    Answer(String),
    /// Something the call had to do differently, on its way to a `note` record.
    Note(String),
    /// A tool the agent reached for, on its way to a `tool_call` record.
    ToolCall(serde_json::Value),
    /// What that call answered, on its way to a `tool_result` record.
    ToolResult(serde_json::Value),
    /// A draft, read against the envelope — or the reason it was not a draft.
    Draft(Result<AskDraft, String>),
}

/// Append one reply record to the ask surface, keyed by CONVERSATION.
async fn append_ask(
    client: &GladeClient,
    config: &GyldConfig,
    conversation: &str,
    record: &GyldAskRecord,
) {
    let _ = client
        .append(
            &config.share,
            &config.ask_id,
            "log",
            record.to_bytes(),
            Some(conversation.as_bytes()),
        )
        .await;
}

/// Lay the bundle root and give it its first build, as a STREAMING run on the
/// output surface keyed by [`FIRST_BUILD_RUN_ID`] — the same path a `rebuild`
/// takes, so the build lands, `latest.json` is swapped and the census is
/// published by the code that already does all three.
///
/// The supplier keeps serving throughout: this is a task, and the verbs that
/// need a bundle are refused meanwhile with a message that names the run
/// ([`FirstBuild::explain`]). `fork` and `link` are unaffected — they need no
/// bundle. A first build that fails is failure as DATA on the run and one log
/// line; the supplier stays up and the root simply still has no build.
fn spawn_first_build(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    first: Arc<FirstBuild>,
) {
    first.begin();
    let run_id = first.run_id.clone();
    let who = config.principal.clone();
    eprintln!(
        "glade-gyld: first build of {} — the bundle root holds none (run {run_id})",
        config.layout.bundle_root.display()
    );

    let inner = handle.clone();
    handle.spawn(async move {
        let prepared = {
            let config = config.clone();
            let runner = runner.clone();
            tokio::task::spawn_blocking(move || prepare_first_build(&config, &runner)).await
        };
        match prepared {
            Ok(Ok(plan)) => {
                stream_run(
                    client.clone(),
                    config.clone(),
                    runner,
                    inner,
                    run_id.clone(),
                    plan,
                    who.clone(),
                )
                .await;
            }
            Ok(Err(e)) => {
                fail_first_build(&client, &config, &run_id, &who, e).await;
            }
            Err(e) => {
                fail_first_build(
                    &client,
                    &config,
                    &run_id,
                    &who,
                    format!("run task failed: {e}"),
                )
                .await;
            }
        }
        first.ended();
    });
}

/// Lay the stage, ask the checkout which streams it declares, and plan the first
/// build. Blocking: it touches the filesystem and runs one short host.
fn prepare_first_build(config: &GyldConfig, runner: &Arc<dyn Runner>) -> Result<Plan, String> {
    bundle::ensure_stage(&config.layout).map_err(|e| format!("bundle root unusable: {e}"))?;
    let declared = declared_streams(config, runner);
    Ok(verbs::first_build_plan(
        &config.layout,
        &bundle::build_stamp(),
        &declared,
    ))
}

/// Which streams the staging repository declares, asked of the Gyld host that
/// owns the answer. A checkout that cannot answer degrades to the base build
/// rather than failing the start: a bundle root with a small build in it is
/// still a bundle root a UI can work from.
fn declared_streams(config: &GyldConfig, runner: &Arc<dyn Runner>) -> Vec<String> {
    let plan = verbs::discover_plan(&config.layout);
    let found = match runner.run(&plan, config.limits, &mut |_, _| {}) {
        Ok(out) if out.exit == 0 => verbs::declared_streams(&out.stdout),
        Ok(out) => {
            eprintln!(
                "glade-gyld: stream discovery exited {}; the first build takes the base streams \
                 only",
                out.exit
            );
            Vec::new()
        }
        Err(e) => {
            eprintln!(
                "glade-gyld: stream discovery failed ({e}); the first build takes the base \
                 streams only"
            );
            Vec::new()
        }
    };
    eprintln!(
        "glade-gyld: the checkout declares {}",
        if found.is_empty() {
            "no stream but the base".to_string()
        } else {
            found.join(", ")
        }
    );
    found
}

/// A first build that never got as far as a host: the reason goes on the run,
/// closed by the terminal record, exactly as a failed run's would.
async fn fail_first_build(
    client: &GladeClient,
    config: &GyldConfig,
    run_id: &str,
    who: &Option<String>,
    reason: String,
) {
    eprintln!("glade-gyld: first build failed: {reason}");
    let record = GyldOutputRecord::line(run_id, 1, who, "stderr", reason);
    append(client, config, run_id, &record).await;
    append(
        client,
        config,
        run_id,
        &GyldOutputRecord::end(run_id, 2, who, -1),
    )
    .await;
}

/// Append one output record to the log surface, keyed by run id.
async fn append(
    client: &GladeClient,
    config: &GyldConfig,
    run_id: &str,
    record: &GyldOutputRecord,
) {
    let _ = client
        .append(
            &config.share,
            &config.output_id,
            "log",
            record.to_bytes(),
            Some(run_id.as_bytes()),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::GyldRequest;

    /// A bundle root of its own, removed by the caller.
    fn root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("glade-gyld-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

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

    #[test]
    fn the_no_bundle_refusal_names_the_first_build_while_it_is_running() {
        let first = FirstBuild::new();
        // Before it starts, and after it ends, the plain refusal stands: there
        // really is no bundle and nothing is coming.
        assert_eq!(first.explain(verbs::NO_BUNDLE.into()), verbs::NO_BUNDLE);

        first.begin();
        let said = first.explain(verbs::NO_BUNDLE.into());
        assert!(
            said.contains("the first build is in progress") && said.contains("run boot-1"),
            "{said}"
        );
        assert_ne!(said, verbs::NO_BUNDLE);

        // Every other refusal passes through untouched, running or not.
        let other = "verb `forall` not in the allow-list".to_string();
        assert_eq!(first.explain(other.clone()), other);

        first.ended();
        assert_eq!(first.explain(verbs::NO_BUNDLE.into()), verbs::NO_BUNDLE);
    }

    #[test]
    fn a_planned_write_refuses_to_clobber_unless_forced() {
        let dir = std::env::temp_dir().join(format!("glade-gyld-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("glade-decisions-a.gyld.py");

        let mut plan = verbs::plan(
            &Layout::new(PathBuf::from("/g"), PathBuf::from("/b")),
            &GyldRequest::parse(br#"{"verb":"fork","args":{"parent":"base","stream":"a-b"}}"#)
                .unwrap(),
            None,
            "build-1",
            &AgentState::default(),
        )
        .unwrap();

        plan.write = Some(verbs::PlannedWrite {
            path: path.clone(),
            text: "one\n".into(),
            force: false,
        });
        write_overlay(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

        let e = write_overlay(&plan).unwrap_err();
        assert!(e.contains("nothing is overwritten"), "{e}");

        plan.write = Some(verbs::PlannedWrite {
            path: path.clone(),
            text: "two\n".into(),
            force: true,
        });
        write_overlay(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
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

        let overlays = dir.join("bundle/overlays");
        std::fs::create_dir_all(&overlays).unwrap();
        let staged = overlays.join("glade-decisions-stream-a.gyld.py");
        bundle::link(&shipped, &staged).unwrap();

        let plan = forced_write(&staged, "the owner's ruling\n");
        write_overlay(&plan).expect("the write lands");

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

    /// A plan whose only business is one forced write at `path`.
    fn forced_write(path: &std::path::Path, text: &str) -> Plan {
        let mut plan = verbs::plan(
            &Layout::new(PathBuf::from("/g"), PathBuf::from("/b")),
            &GyldRequest::parse(br#"{"verb":"fork","args":{"parent":"base","stream":"a-b"}}"#)
                .unwrap(),
            None,
            "build-1",
            &AgentState::default(),
        )
        .unwrap();
        plan.write = Some(verbs::PlannedWrite {
            path: path.to_path_buf(),
            text: text.to_string(),
            force: true,
        });
        plan
    }

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

    #[test]
    fn a_document_over_the_budget_is_refused_not_returned() {
        let dir = std::env::temp_dir().join(format!("glade-gyld-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("streams.json");
        std::fs::write(&path, "x".repeat(100)).unwrap();

        assert_eq!(read_bounded(&path, 1000).unwrap().len(), 100);
        let e = read_bounded(&path, 10).unwrap_err();
        assert!(e.contains("the exchange budget is 10"), "{e}");
        let e = read_bounded(&dir.join("missing.json"), 10).unwrap_err();
        assert!(e.contains("cannot read"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
