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
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use glade_client::supplier::{Supplier, SupplierConfig, SupplierSurface};
use glade_client::GladeClient;
use glade_wire::generated::ExchangeReq;

use crate::agent::{self, AgentOverrides, Resolved};
use crate::ask::{self, AgentState, AskDraft, Consultation};
use crate::bundle::{self, Layout};
use crate::conversation::{self, Ledger};
use crate::envelope::{GyldAskRecord, GyldOutputRecord, GyldRequest, GyldResponse, Refusal};
use crate::exec::{Limits, PythonRunner, RunOutput, Runner};
use crate::model::{self, ModelClient, ModelConfig, ModelEvent, ModelRequest};
use crate::outcome::{self, Put, Snapshot};
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

/// The run ids one supplier process mints, and the counter numbering them.
///
/// The counter alone is not enough. A run's output and its terminal record are
/// appended to the `gyld.output` log keyed by run id, and that log lives in the
/// node's PERSISTENT store — so a counter that restarts with the process gives a
/// fresh run an id the previous session already spent, and a reader that looks up
/// this run's outcome finds the OLD run's terminal record instead. The session
/// tag is what makes the id unique across restarts; the counter still orders the
/// runs within one.
struct Runs {
    /// Fixed for the life of this process, and different in the next one.
    session: String,
    next: AtomicU64,
}

impl Runs {
    /// Take the session tag ONCE, here: every id this process mints shares it.
    fn new() -> Runs {
        Runs {
            session: session_tag(),
            next: AtomicU64::new(0),
        }
    }

    /// The next run's id — distinct from every other id this process mints, and
    /// from the ids of every other process.
    fn mint(&self) -> String {
        mint_run_id(&self.session, self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// One run id, from the session it was minted in and the number it took.
///
/// OPAQUE to every reader: the log is keyed by the whole string and
/// `requests/<run-id>.json` uses it as a path component, so the only properties
/// that matter are that it holds no whitespace, stays one path component, and
/// that no two runs anywhere ever share one.
fn mint_run_id(session: &str, n: u64) -> String {
    format!("run-{session}-{n}")
}

/// A tag for this process, read off the clock when the counter is made.
///
/// Milliseconds since the epoch in base36: short (eight characters until 2059)
/// and, at one width, ordered as text the way it is ordered in time, so a reader
/// that sorts ids as strings still puts an older session's runs first. A clock
/// before the epoch degrades to `0` rather than panicking, as
/// [`bundle::build_stamp`] does with the same reading.
fn session_tag() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    base36(millis)
}

/// `n` in lowercase base36, most significant digit first.
fn base36(mut n: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    out.reverse();
    out.into_iter().collect()
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

/// Writing verbs run ONE AT A TIME: the gate is taken before the write and freed
/// when the run has settled or been put back.
///
/// The kit already serialises the exchange handler — one `ExchangeReq` at a time
/// — so two writes can never interleave. What it does not serialise is a STREAMED
/// run, which is accepted at once and settles later on its own task. Without this
/// gate a second `answer` arriving mid-run would write its notebook, and the first
/// run's refusal would then put the FIRST notebook back over it: the second write
/// lost, and the bundle agreeing with neither.
///
/// A second writing verb is REFUSED as data rather than made to wait. Waiting
/// would have to happen in the kit's synchronous handler, which is the ONE loop
/// every request passes through, so a blocked write would freeze `list` and
/// `explain` and `diff` behind it for as long as a build takes — and `list` is
/// what a UI polls while it waits. A refusal that names the run in flight is
/// something a desk can say and a reader can act on; a desk that stops answering
/// is not. The owner's draft is his, so submitting again costs him nothing.
///
/// One gate and nothing else: no queue, no lock manager, nothing to configure. A
/// `std::sync::Mutex` rather than a `tokio::sync::Mutex` because the hold begins
/// in that synchronous handler and ends on a spawned task, and a guard that
/// crosses that seam must be `Send` and must not need a runtime flavour.
#[derive(Debug, Default)]
pub struct WriteGate {
    /// The run id of the write in flight, when one is.
    holder: std::sync::Mutex<Option<String>>,
}

/// The gate, HELD. Dropping it frees the gate, so a run that panics cannot wedge
/// the desk shut against every write after it.
#[derive(Debug)]
pub struct Writing(Arc<WriteGate>);

impl WriteGate {
    /// Take the gate for `run_id`, or say which run already has it.
    ///
    /// A poisoned lock is taken anyway: the state behind it is one `Option`, a
    /// panic cannot have left it torn, and refusing every write for the rest of
    /// the process is a worse answer than carrying on.
    pub fn try_hold(gate: &Arc<WriteGate>, run_id: &str) -> Result<Writing, String> {
        let mut holder = gate.holder.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = holder.as_deref() {
            return Err(format!(
                "a write is already in flight (run {held}); wait for it to finish and submit again"
            ));
        }
        *holder = Some(run_id.to_string());
        Ok(Writing(gate.clone()))
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut holder = self.0.holder.lock().unwrap_or_else(|e| e.into_inner());
        *holder = None;
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
    // One run counter AND one session tag for the supplier's lifetime, so the ids
    // it mints are distinct from the ones the process before it minted.
    let runs = Arc::new(Runs::new());
    // One running total per conversation, for the supplier's lifetime
    // (GyldAskAgent.md section 7).
    let ledger = Arc::new(Ledger::default());
    // One gate, for the supplier's lifetime: writing verbs run one at a time.
    let writes = Arc::new(WriteGate::default());
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
            &writes,
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
    runs: &Arc<Runs>,
    first: &Arc<FirstBuild>,
    writes: &Arc<WriteGate>,
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
    // The run id is minted BEFORE the plan, because the plan names a file after it:
    // a fragment `answer` lays its fragment down at `requests/<run-id>.json`, and
    // the planner stays pure by being handed the id rather than inventing one.
    let run_id = runs.mint();
    let plan = match verbs::plan(
        &config.layout,
        &request,
        latest.as_deref(),
        &stamp,
        &run_id,
        &agent,
    ) {
        Ok(p) => p,
        Err(e) => {
            // `fork` and `link` need no bundle and are unaffected; the five that
            // do are told the first build is on its way rather than that there
            // is none.
            return GyldResponse::failed(first.explain(e), who);
        }
    };

    // BEFORE anything is written: what the notebook's name held, so a write Gyld
    // then rejects can be put back exactly. A snapshot that cannot be taken
    // refuses the verb here, because the alternative is a restore that would
    // delete a notebook it could not read.
    let snapshot = match Snapshot::take(&config.layout, &plan) {
        Ok(s) => s,
        Err(e) => {
            return GyldResponse::failed(e, who);
        }
    };
    // One writing verb at a time, from before the write until the run has
    // settled or been put back. Taken only for a verb that leaves a notebook, and
    // a second one is refused as data rather than left to freeze the exchange.
    let gate = match snapshot.is_some() {
        true => match WriteGate::try_hold(writes, &run_id) {
            Ok(held) => Some(held),
            Err(e) => {
                return GyldResponse::failed(e, who);
            }
        },
        false => None,
    };
    // The whole-module path writes its notebook HERE, before the accept, exactly as
    // it always has. The fragment path writes nothing yet: the notebook's text is
    // the merge host's answer and that host runs inside the run ([`fold`]), so both
    // paths reach the rebuild with the same plan and the same file on disk.
    if plan.merge.is_none() {
        if let Err(e) = write_overlay(&config.layout, &plan) {
            return GyldResponse::failed(e, who);
        }
        if let Err(e) = stage_notebook(&config.layout, &plan) {
            return GyldResponse::failed(e, who);
        }
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
        // Named before the plan is handed over: `answer` and `ask` have already
        // written their notebook, and that is what the accept says.
        //
        // It is not yet a notebook that is SAVED, and a desk must not present it
        // as one. Gyld has not seen the text — the host runs after this answer
        // goes out — so the file may be about to be put back. The run's TERMINAL
        // record is where the outcome lives: it carries `overlay_file` when the
        // write stood and a `refusal` when it did not (README, "A refused
        // write"). The field stays here for the readers that have it.
        let left = overlay_left(&plan);
        spawn_stream(
            client.clone(),
            config.clone(),
            runner.clone(),
            handle.clone(),
            run_id.clone(),
            plan,
            who.clone(),
            snapshot,
            latest,
            gate,
        );
        return GyldResponse::accepted(run_id, who).leaving(left);
    }

    // The fragment path's merge runs here, inside the run and before the rebuild.
    // A merge Gyld refused wrote nothing, so there is nothing to put back and the
    // answer carries its code and its message like any other refusal.
    let (plan, merged) = match fold(config, runner.as_ref(), &plan) {
        Ok(both) => both,
        Err(refusal) => {
            eprintln!("{}", outcome::said(&plan.verb, &refusal, Put::Nothing));
            return GyldResponse::refused(
                run_id,
                1,
                String::new(),
                String::new(),
                &refusal,
                None,
                who,
            );
        }
    };
    let ran = runner.run(&plan, config.limits, &mut |_, _| {});
    let judged = match ran.as_ref() {
        Ok(out) => land(
            &config.layout,
            &plan,
            Ok(out),
            snapshot.as_ref(),
            latest.as_deref(),
        ),
        Err(e) => land(
            &config.layout,
            &plan,
            Err(e),
            snapshot.as_ref(),
            latest.as_deref(),
        ),
    };
    match (ran, judged) {
        // Refused: failure as data, with the message as `error` and Gyld's own
        // document as `validation` (GyldGrythPlugins.md 4.7). The notebook is
        // back, the build is gone and nothing was published.
        (Ok(out), Some((refusal, document))) => GyldResponse::refused(
            run_id, out.exit, out.stdout, out.stderr, &refusal, document, who,
        ),
        (Ok(out), None) => {
            settle(config, &plan, &out);
            let dir = finish(client, config, handle, &plan, &out);
            let named = dir.as_ref().map(|d| d.display().to_string());
            // The merge's one summary line stands above the rebuild's own output, so
            // the synchronous answer says what a streamed one appends to the log.
            let stdout = match merged {
                Some(line) => format!("{line}\n{}", out.stdout),
                None => out.stdout,
            };
            GyldResponse::ran(run_id, out.exit, stdout, out.stderr, named, who)
                .leaving(overlay_left(&plan))
        }
        // A run that never landed at all — a spawn failure, a timeout. The
        // refusal says the same thing the plain failure used to, and the notebook
        // is put back before it is said.
        (Err(e), Some((refusal, document))) => {
            GyldResponse::refused(run_id, -1, String::new(), e, &refusal, document, who)
        }
        (Err(e), None) => GyldResponse::failed(e, who),
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
///
/// AND NOT THROUGH A SYMLINKED DIRECTORY. The planner's containment check is
/// lexical, which is what keeps it pure; that leaves the filesystem's own
/// question — where does this directory actually lead — to be asked here, where
/// there is a filesystem to ask. The target's parent is canonicalized and must
/// come out inside the canonicalized [`Layout::overlay_home`], so a directory
/// link laid in the tree cannot redirect a ruling somewhere else.
fn write_overlay(layout: &Layout, plan: &Plan) -> Result<(), String> {
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
    let home = layout.overlay_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    // Lexically first, so nothing is CREATED outside the home on the way to
    // finding out that the write does not belong there.
    if !bundle::contained(&home, &write.path) {
        return Err(format!(
            "cannot write {}: it is not in the overlay home {}",
            write.path.display(),
            home.display()
        ));
    }
    let parent = write
        .path
        .parent()
        .ok_or_else(|| format!("cannot write {}: it has no directory", write.path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let real_home = canonical(&home)?;
    let real_parent = canonical(parent)?;
    if !real_parent.starts_with(&real_home) {
        return Err(format!(
            "cannot write {}: {} leads outside the overlay home {}",
            write.path.display(),
            real_parent.display(),
            real_home.display()
        ));
    }

    bundle::replace(&write.path, write.text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", write.path.display()))
}

/// Where a directory really is, links resolved. A failure is data, like every
/// other refusal on the write path.
fn canonical(dir: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(dir).map_err(|e| format!("cannot resolve {}: {e}", dir.display()))
}

/// Point the staging tree at the notebook the write just left in the decisions
/// root, REPLACING whatever held that name — a seed link into the Gyld checkout,
/// or a real file from before there was a decisions root.
///
/// This is the whole of copy-on-write for a shipped sample. An `answer` on
/// `stream-a` writes the owner's copy into his folder; this points the staging
/// tree at it; the sample in the checkout is never touched and is shadowed from
/// here on. With no decisions root the write already landed in the staging tree
/// and there is nothing to point anywhere.
fn stage_notebook(layout: &Layout, plan: &Plan) -> Result<(), String> {
    let decisions = match layout.decisions_root.as_ref() {
        Some(root) => root,
        None => {
            return Ok(());
        }
    };
    let written = match plan.write.as_ref() {
        Some(w) => &w.path,
        None => {
            return Ok(());
        }
    };
    let name = match written.file_name() {
        Some(n) => n,
        None => {
            return Ok(());
        }
    };
    if written.parent() != Some(decisions.as_path()) {
        return Ok(());
    }
    let staged = layout.overlays().join(name);
    bundle::relink(written, &staged).map_err(|e| {
        format!(
            "cannot point {} at {}: {e}",
            staged.display(),
            written.display()
        )
    })
}

/// Fold a fragment into the notebook and write the result, or answer with the
/// refusal the Gyld host gave (GyldGrythPlugins.md 4.8).
///
/// The sequence, and it runs INSIDE the run for both paths, so a streamed and a
/// synchronous `answer` behave the same:
///
/// 1. the fragment goes to `requests/<run-id>.json`, the host's only operand;
/// 2. `manage_decision_streams.py merge` runs, bounded like any other host;
/// 3. the fragment file is taken away, whatever the host said;
/// 4. on `ok`, the merged module — the host's STDOUT — becomes the planned write's
///    text, and the write and the staging link happen exactly as a whole-module
///    `answer`'s do.
///
/// QUIET on purpose. That stdout is a whole Gyld module, and `gyld.output` is a
/// place a person reads: the lines are not forwarded and one summary line goes out
/// in their place, which is the second half of the answer.
///
/// A refusal is the host's own `code` and `message`.
///
/// `restored` on that refusal answers the question the field asks — IS THE NOTEBOOK
/// AS IT WAS — and every refusal up to and including the host's answer leaves it
/// untouched, so it is. Only a refusal from the write itself says otherwise; a desk
/// reads `false` as "could NOT be put back, check it", which is a thing to say to
/// an owner when it is true and an alarm when it is not (found live, 2026-09-21).
fn fold(
    config: &GyldConfig,
    runner: &dyn Runner,
    plan: &Plan,
) -> Result<(Plan, Option<String>), Refusal> {
    let merge = match plan.merge.as_ref() {
        Some(merge) => merge,
        None => {
            return Ok((plan.clone(), None));
        }
    };
    let refused = |code: &str, message: String, untouched: bool| -> Refusal {
        Refusal {
            stream: plan.stream.clone().unwrap_or_default(),
            code: code.to_string(),
            message: verbs::one_line(&message),
            details: None,
            restored: untouched,
        }
    };
    let directory = config.layout.requests();
    if let Err(e) = std::fs::create_dir_all(&directory) {
        return Err(refused(
            outcome::RUN_FAILED,
            format!("cannot create {}: {e}", directory.display()),
            true,
        ));
    }
    if let Err(e) = bundle::replace(&merge.fragment.path, merge.fragment.text.as_bytes()) {
        return Err(refused(
            outcome::RUN_FAILED,
            format!("cannot write {}: {e}", merge.fragment.path.display()),
            true,
        ));
    }
    let host = Plan {
        argv: merge.argv.clone(),
        write: None,
        merge: None,
        output_dir: None,
        ..plan.clone()
    };
    let ran = runner.run(&host, config.limits, &mut |_, _| {});
    // The fragment has served its purpose either way: it is a request document, and
    // a request document left lying about is a request nobody made.
    if let Err(e) = std::fs::remove_file(&merge.fragment.path) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "glade-gyld: could not remove {}: {e}",
                merge.fragment.path.display()
            );
        }
    }
    let out = match ran {
        Ok(out) => out,
        Err(e) => {
            return Err(refused(outcome::RUN_FAILED, e, true));
        }
    };
    let answered = verbs::merge_answer(&out.stdout);
    let said = |key: &str| -> Option<String> {
        answered
            .as_ref()?
            .get(key)?
            .as_str()
            .map(|value| value.to_string())
    };
    let ok = answered
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !ok {
        // Gyld's own code and message when it wrote one; otherwise the host's last
        // word, which is what every other failed run here is reported by.
        let code = said("code").unwrap_or_else(|| outcome::RUN_FAILED.to_string());
        let message = said("message").unwrap_or_else(|| last_line(&out));
        return Err(refused(&code, message, true));
    }
    let text = match said("text") {
        Some(text) => text,
        None => {
            return Err(refused(
                outcome::RUN_FAILED,
                "the merge host answered ok and printed no merged module".into(),
                true,
            ));
        }
    };
    if text.len() > verbs::MAX_OVERLAY_BYTES {
        return Err(refused(
            outcome::RUN_FAILED,
            format!(
                "the merged module is {} bytes; the limit is {}",
                text.len(),
                verbs::MAX_OVERLAY_BYTES
            ),
            true,
        ));
    }
    let mut written = plan.clone();
    if let Some(write) = written.write.as_mut() {
        write.text = text;
    }
    // The two that may have TOUCHED the notebook: an atomic replace that failed
    // wrote nothing, but a staging link that failed after it did not, and the owner
    // is the one who has to look.
    if let Err(e) = write_overlay(&config.layout, &written) {
        return Err(refused(outcome::RUN_FAILED, e, false));
    }
    if let Err(e) = stage_notebook(&config.layout, &written) {
        return Err(refused(outcome::RUN_FAILED, e, false));
    }
    Ok((written, Some(merged_line(&answered, plan))))
}

/// The one line a merge forwards in place of the module it printed: what was added
/// and which notebook it went into.
fn merged_line(answered: &Option<serde_json::Value>, plan: &Plan) -> String {
    let named = |key: &str| -> Vec<String> {
        answered
            .as_ref()
            .and_then(|value| value.get("added"))
            .and_then(|added| added.get(key))
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let added = match named("classes") {
        found if found.is_empty() => named("members"),
        found => found,
    };
    let file = plan
        .overlay
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| plan.stream.clone().unwrap_or_default());
    match added.is_empty() {
        true => format!("merged a fragment into {file}"),
        false => format!("merged {} into {file}", added.join(", ")),
    }
}

/// [`fold`] on a blocking task, so the merge host and the two writes it leads to
/// never run on the async runtime's own thread.
///
/// The plan comes back either way: a refusal needs it to say which verb was
/// refused, and a plan moved onto the task and lost with it could not.
async fn folded(
    config: &Arc<GyldConfig>,
    runner: &Arc<dyn Runner>,
    plan: Plan,
) -> Result<(Plan, Option<String>), (Plan, Refusal)> {
    if plan.merge.is_none() {
        return Ok((plan, None));
    }
    let held = plan.clone();
    let work = {
        let config = config.clone();
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || fold(&config, runner.as_ref(), &plan))
    };
    match work.await {
        Ok(Ok(both)) => Ok(both),
        Ok(Err(refusal)) => Err((held, refusal)),
        Err(e) => {
            let refusal = Refusal {
                stream: held.stream.clone().unwrap_or_default(),
                code: outcome::RUN_FAILED.into(),
                message: format!("the merge task failed: {e}"),
                details: None,
                restored: false,
            };
            Err((held, refusal))
        }
    }
}

/// A host's last word: its last non-empty stderr line, else its exit code.
fn last_line(out: &RunOutput) -> String {
    let last = out
        .stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty());
    match last {
        Some(line) => line.to_string(),
        None => format!(
            "the merge host exited {} and said nothing on stderr",
            out.exit
        ),
    }
}

/// Take what a Gyld host wrote into the staging tree over to the decisions root.
///
/// `fork` and `link` write their module themselves, into `<stage>/examples`, and
/// read it back to check it — none of which changes. Afterwards the file is the
/// OWNER's, so [`bundle::ensure_stage`] adopts it: moved to his folder, with a
/// link left in its place. Idempotent, so a verb that had already settled its
/// own notebook (`answer`, `ask`) costs one wasted directory listing and nothing
/// else. A failed run settles nothing — there is no file to take.
///
/// An adoption failure is a LOG LINE, not a refusal: the run itself succeeded,
/// the module is in the staging tree, and the stream builds either way.
fn settle(config: &GyldConfig, plan: &Plan, out: &RunOutput) {
    if out.exit != 0 || plan.overlay.is_none() || config.layout.decisions_root.is_none() {
        return;
    }
    if let Err(e) = bundle::ensure_stage(&config.layout) {
        eprintln!("glade-gyld: could not adopt the notebook this run wrote: {e}");
    }
}

/// The notebook this verb has left behind, IF it is really there now.
///
/// A path that exists, not a path that was planned. `answer` and `ask` write the
/// file before the answer goes out, so they always name it; a streamed `fork`
/// answers before its host has run at all, and names nothing rather than promise
/// a file that may never arrive.
fn overlay_left(plan: &Plan) -> Option<String> {
    let path = plan.overlay.as_ref()?;
    path.symlink_metadata().ok()?;
    Some(path.display().to_string())
}

/// Decide what a writing run DID, and make the bundle root agree with it.
///
/// A writing verb that makes things worse is refused and leaves no trace: the
/// notebook goes back exactly as it was ([`Snapshot::restore`]), the build this
/// run made is removed, nothing is recorded as the latest and nothing is
/// published. Answers the refusal and Gyld's own validation document, or `None`
/// when the write stands and the caller may go on to [`settle`] and [`finish`].
///
/// The build directory must GO and not merely be left unpublished:
/// [`bundle::latest_build`] falls back to the newest `builds/` directory holding a
/// `streams.json`, so a refused-but-COMPLETE build left behind would become the
/// current one by itself the next time the pointer was missing or stale.
///
/// One line on stderr, always. The defect this answers was silent: a rejected
/// answer left a broken file, abandoned the rebuild, and said nothing anywhere.
fn land(
    layout: &Layout,
    plan: &Plan,
    ran: Result<&RunOutput, &str>,
    snapshot: Option<&Snapshot>,
    previous: Option<&std::path::Path>,
) -> Option<(Refusal, Option<serde_json::Value>)> {
    let (mut refusal, document) = match outcome::classify(plan, ran, previous) {
        outcome::Outcome::Accepted => {
            return None;
        }
        outcome::Outcome::Refused { refusal, document } => (refusal, document),
    };
    let put = match snapshot {
        Some(snapshot) => match snapshot.restore() {
            Ok(()) => {
                refusal.restored = true;
                Put::Restored
            }
            Err(e) => {
                eprintln!("glade-gyld: the notebook could NOT be put back: {e}");
                Put::Failed
            }
        },
        None => Put::Nothing,
    };
    discard(layout, plan);
    eprintln!("{}", outcome::said(&plan.verb, &refusal, put));
    Some((refusal, document))
}

/// Remove the build this run made, if it made one. Only ever a directory under
/// the root's own `builds/`: the recursive removal is checked against the layout
/// rather than trusted to the plan, cheap insurance on the one call here that
/// deletes a tree.
fn discard(layout: &Layout, plan: &Plan) {
    let dir = match plan.output_dir.as_ref() {
        Some(dir) => dir,
        None => {
            return;
        }
    };
    if !bundle::contained(&layout.builds(), dir) {
        eprintln!(
            "glade-gyld: not removing {}: it is not under {}",
            dir.display(),
            layout.builds().display()
        );
        return;
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("glade-gyld: could not remove {}: {e}", dir.display());
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn spawn_stream(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
    snapshot: Option<Snapshot>,
    previous: Option<PathBuf>,
    gate: Option<Writing>,
) {
    let inner = handle.clone();
    handle.spawn(stream_run(
        client, config, runner, inner, run_id, plan, who, snapshot, previous, gate,
    ));
}

/// Run the plan on a blocking task, appending every output line to the log
/// surface keyed by `run_id`, then a terminal `{done:true, exit}` record.
/// Best effort: an append failure (a link drop mid-run) is dropped, because the
/// exchange answer already carried the run id.
///
/// Resolves when the run has landed and its terminal record is on the log, so a
/// caller that must know when a build finished — the first build — can await it.
///
/// The TERMINAL record carries the outcome of a writing verb, because the accept
/// answer could not: it went out before the host ran. A refused write appends the
/// `refusal` there and an accepted one the notebook it really left, so a desk says
/// "saved" once the run says so and not before.
#[allow(clippy::too_many_arguments)]
async fn stream_run(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
    run_id: String,
    plan: Plan,
    who: Option<String>,
    snapshot: Option<Snapshot>,
    previous: Option<PathBuf>,
    gate: Option<Writing>,
) {
    let inner = handle;
    let mut seq: u64 = 0;
    // The fragment path's merge, on a blocking task of its own and BEFORE the
    // rebuild: it writes the notebook this run is about to build, so nothing after
    // it can tell a fragment `answer` from a whole-module one.
    let (plan, merged) = match folded(&config, &runner, plan).await {
        Ok(both) => both,
        Err((plan, refusal)) => {
            eprintln!("{}", outcome::said(&plan.verb, &refusal, Put::Nothing));
            drop(gate);
            seq += 1;
            append(
                &client,
                &config,
                &run_id,
                &GyldOutputRecord::end(&run_id, seq, &who, 1).refusing(Some(refusal)),
            )
            .await;
            return;
        }
    };
    if let Some(line) = merged {
        seq += 1;
        let record = GyldOutputRecord::line(&run_id, seq, &who, "stdout", line);
        append(&client, &config, &run_id, &record).await;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
    let limits = config.limits;
    // The plan is judged HERE and run over there: a copy stays behind so a run
    // whose task died outright is still put back, which a plan handed to the task
    // and lost with it could not be.
    let judged = plan.clone();
    let work = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || {
            runner.run(&plan, limits, &mut |stream, line| {
                let _ = tx.send((stream.to_string(), line.to_string()));
            })
        })
    };

    while let Some((stream, line)) = rx.recv().await {
        seq += 1;
        let record = GyldOutputRecord::line(&run_id, seq, &who, &stream, line);
        append(&client, &config, &run_id, &record).await;
    }

    let (exit, refused) = match work.await {
        Ok(Ok(out)) => {
            let refused = land(
                &config.layout,
                &judged,
                Ok(&out),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            if refused.is_none() {
                settle(&config, &judged, &out);
                finish(&client, &config, &inner, &judged, &out);
            }
            (out.exit, refused)
        }
        Ok(Err(e)) => {
            seq += 1;
            let record = GyldOutputRecord::line(&run_id, seq, &who, "stderr", e.clone());
            append(&client, &config, &run_id, &record).await;
            let refused = land(
                &config.layout,
                &judged,
                Err(&e),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            (-1, refused)
        }
        Err(e) => {
            let said = format!("run task failed: {e}");
            seq += 1;
            let record = GyldOutputRecord::line(&run_id, seq, &who, "stderr", said.clone());
            append(&client, &config, &run_id, &record).await;
            let refused = land(
                &config.layout,
                &judged,
                Err(&said),
                snapshot.as_ref(),
                previous.as_deref(),
            );
            (-1, refused)
        }
    };
    // The write has settled or been put back: the next writing verb may go.
    drop(gate);

    // The notebook this run really left, and only on a run that was not refused.
    let saved = match refused.is_some() {
        true => None,
        false => overlay_left(&judged),
    };
    seq += 1;
    append(
        &client,
        &config,
        &run_id,
        &GyldOutputRecord::end(&run_id, seq, &who, exit)
            .refusing(refused.map(|(refusal, _)| refusal))
            .leaving(saved),
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
                // Nothing to put back and nothing to wait for: the first build
                // writes no notebook, and there is no previous build for its
                // streams to have been valid in. A failed one still has its
                // half-written directory removed, which is `land`'s business.
                stream_run(
                    client.clone(),
                    config.clone(),
                    runner,
                    inner,
                    run_id.clone(),
                    plan,
                    who.clone(),
                    None,
                    None,
                    None,
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

    /// A minted run id has to be unique across supplier RESTARTS, not just
    /// within one process. The `gyld.output` log it keys lives in the node's
    /// persistent store, so a second session that reused `run-1` would hand a
    /// fresh run the FIRST session's terminal record — and report that run's
    /// outcome for this one.
    #[test]
    fn two_sessions_mint_different_ids_for_the_same_counter() {
        assert_ne!(mint_run_id("mfk3x9p", 1), mint_run_id("mfk3xa2", 1));
        assert_ne!(mint_run_id("mfk3x9p", 7), mint_run_id("mfk3xa2", 7));
    }

    /// Within one session the counter still does its job: every id distinct,
    /// numbered from one, in the order the runs were taken.
    #[test]
    fn one_session_mints_distinct_ids_numbered_from_one() {
        let runs = Runs::new();
        let minted: Vec<String> = (0..3).map(|_| runs.mint()).collect();
        let session = runs.session.clone();
        assert_eq!(
            minted,
            vec![
                mint_run_id(&session, 1),
                mint_run_id(&session, 2),
                mint_run_id(&session, 3)
            ],
            "the counter numbers the runs from one, within the session"
        );
    }

    /// The id is a KEY before it is anything else: it names the one file a
    /// fragment `answer` lays down, and `gyld-ui.py` reads a run id back off a
    /// log line by whitespace. So it holds no whitespace, it is ONE path
    /// component, and the fragment path it names still passes the planner's
    /// containment check.
    #[test]
    fn a_minted_id_is_a_valid_fragment_path_key() {
        let layout = Layout::new(PathBuf::from("/g"), PathBuf::from("/b"));
        let id = Runs::new().mint();
        assert!(
            !id.chars().any(char::is_whitespace),
            "no whitespace in a run id: {id:?}"
        );
        assert_eq!(
            std::path::Path::new(&id).components().count(),
            1,
            "a run id is one path component: {id:?}"
        );

        let fragment = layout.requests().join(format!("{id}.json"));
        assert!(
            bundle::contained(&layout.bundle_root, &fragment),
            "{} leaves the bundle root",
            fragment.display()
        );
        assert_eq!(fragment.parent(), Some(layout.requests().as_path()));
    }

    /// base36, because the session tag has to be SHORT and still sort as text
    /// the way it sorts in time: at one width its digits ascend in ASCII too, so
    /// a reader that orders ids as strings puts an older session's runs first.
    #[test]
    fn base36_is_short_and_sorts_as_it_counts() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");

        // A millisecond timestamp of today's size is eight characters, and the
        // next millisecond still sorts after it as plain text.
        let now = base36(1_789_247_615_547);
        let later = base36(1_789_247_615_548);
        assert_eq!(now.len(), 8, "{now}");
        assert!(now < later, "{now} < {later}");
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
        let through_home = Layout::new(dir.join("gyld"), dir.join("bundle"))
            .with_decisions_root(Some(linked.clone()));
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

    /// A plan whose only business is one forced write at `path`.
    fn forced_write(path: &std::path::Path, text: &str) -> Plan {
        let mut plan = verbs::plan(
            &Layout::new(PathBuf::from("/g"), PathBuf::from("/b")),
            &GyldRequest::parse(br#"{"verb":"fork","args":{"parent":"base","stream":"a-b"}}"#)
                .unwrap(),
            None,
            "build-1",
            "run-7",
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

    /// An `answer` plan on `stream`, building into `stamp`.
    fn answering(layout: &Layout, stream: &str, stamp: &str) -> Plan {
        let notebook = layout.overlay_home().join(verbs::overlay_file(stream));
        Plan {
            overlay: Some(notebook.clone()),
            stream: Some(stream.to_string()),
            output_dir: Some(layout.new_build_dir(stamp)),
            verb: "answer".into(),
            ..forced_write(&notebook, "the ruling\n")
        }
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

        let refused =
            land(&layout, &plan, Err("timed out after 30s"), None, None).expect("refused");
        assert_eq!(refused.0.stream, "");
        assert_eq!(refused.0.message, "timed out after 30s");
        assert!(!refused.0.restored);
        assert!(!half.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The terminal record is extended ADDITIVELY: every field an old-shape
    /// reader knows is exactly what it was, and the two new ones are absent
    /// unless there is something to say.
    #[test]
    fn the_terminal_record_carries_the_outcome_without_disturbing_an_old_reader() {
        let plain = GyldOutputRecord::end("run-1", 4, &Some("gianni".into()), 0);
        let accepted = plain.clone().leaving(Some("/decisions/a.gyld.py".into()));
        let refused = plain.clone().refusing(Some(Refusal {
            stream: "stream-a".into(),
            code: "SELECTION_NOT_OFFERED".into(),
            message: "VersionPin does not offer SdaxRs".into(),
            details: Some(serde_json::json!({ "ruling": "R" })),
            restored: true,
        }));

        // An old-shape reader: the record's fields as they were, on all three.
        for record in [&plain, &accepted, &refused] {
            let seen: serde_json::Value = serde_json::from_slice(&record.to_bytes()).unwrap();
            assert_eq!(seen["run_id"], "run-1");
            assert_eq!(seen["seq"], 4);
            assert_eq!(seen["principal"], "gianni");
            assert_eq!(seen["stream"], "end");
            assert_eq!(seen["done"], true);
            assert_eq!(seen["exit"], 0);
        }
        let bare: serde_json::Value = serde_json::from_slice(&plain.to_bytes()).unwrap();
        assert!(bare.get("refusal").is_none() && bare.get("overlay_file").is_none());

        let seen: serde_json::Value = serde_json::from_slice(&accepted.to_bytes()).unwrap();
        assert_eq!(seen["overlay_file"], "/decisions/a.gyld.py");
        assert!(seen.get("refusal").is_none());

        let seen: serde_json::Value = serde_json::from_slice(&refused.to_bytes()).unwrap();
        assert_eq!(seen["refusal"]["code"], "SELECTION_NOT_OFFERED");
        assert_eq!(seen["refusal"]["stream"], "stream-a");
        assert_eq!(seen["refusal"]["restored"], true);
        assert_eq!(seen["refusal"]["details"]["ruling"], "R");
        assert!(seen.get("overlay_file").is_none());
    }

    /// Writing verbs run ONE AT A TIME. Without this a second `answer` arriving
    /// mid-run would write its notebook and the first run's refusal would put the
    /// FIRST notebook back over it.
    #[test]
    fn one_writing_verb_at_a_time_and_the_second_is_told_which() {
        let gate = Arc::new(WriteGate::default());
        let held = WriteGate::try_hold(&gate, "run-1").expect("a free gate is taken");

        let said = WriteGate::try_hold(&gate, "run-2").expect_err("the second is refused");
        assert!(
            said.contains("already in flight") && said.contains("run run-1"),
            "the refusal names the run to wait for: {said}"
        );

        // The run has settled: the next write may go, and the gate is free again.
        drop(held);
        let next = WriteGate::try_hold(&gate, "run-2").expect("the gate is free");
        drop(next);
        WriteGate::try_hold(&gate, "run-3").expect("and free again");
    }

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
