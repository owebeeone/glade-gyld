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

use crate::bundle::{self, Layout};
use crate::envelope::{GyldOutputRecord, GyldRequest, GyldResponse};
use crate::exec::{Limits, PythonRunner, RunOutput, Runner};
use crate::publish::{self, Surfaces};
use crate::verbs::{self, Plan};

/// The default surfaces a gyld supplier stands behind (`gyld-app.glade`).
pub const DEFAULT_SHARE: &str = "ws-razel";
pub const DEFAULT_GLADE_ID: &str = "gyld.ops";
pub const DEFAULT_OUTPUT_ID: &str = "gyld.output";
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
    pub layout: Layout,
    /// The value surfaces a successful build publishes onto, and the static
    /// base the lens pointers are written against (step 4.2).
    pub surfaces: Surfaces,
    pub principal: Option<String>,
    pub limits: Limits,
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
            layout: Layout::new(gyld_root, bundle_root),
            surfaces: Surfaces::default(),
            principal: None,
            limits: Limits::default(),
        }
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
/// runner.
pub async fn serve(config: GyldConfig, python: PathBuf) -> io::Result<GyldSupplier> {
    serve_with(config, Arc::new(PythonRunner::new(python))).await
}

/// Connect, attach and serve with a caller-supplied runner. The tests drive this
/// one with a recording double, so the whole verb path is exercised with no
/// interpreter and no Gyld checkout in sight.
pub async fn serve_with(config: GyldConfig, runner: Arc<dyn Runner>) -> io::Result<GyldSupplier> {
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

    let handler = make_handler(
        client.clone(),
        config.clone(),
        runner.clone(),
        Handle::current(),
    );
    supplier
        .serve_exchange(
            SupplierSurface::new(&config.share, &config.glade_id, "exchange"),
            handler,
        )
        .await?;

    // Serving now, so whatever the bundle root already holds can go onto the
    // value shares. A build made by a previous session of this data directory,
    // or seeded by hand, is a build a mount should see: without this it sat
    // there unpublished until somebody pressed Rebuild.
    if let AtAttach::Publish(dir) = at_attach(&config.layout) {
        spawn_publish(
            client.clone(),
            config.clone(),
            Handle::current(),
            dir.clone(),
        );
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

/// Build the exchange handler. It is a synchronous `Fn` (the kit's contract) and
/// never returns `Err`, so the WIRE `ExchangeRes.ok` stays `true` and the PAYLOAD
/// carries success or failure.
fn make_handler(
    client: GladeClient,
    config: Arc<GyldConfig>,
    runner: Arc<dyn Runner>,
    handle: Handle,
) -> impl Fn(&ExchangeReq) -> Result<Vec<u8>, String> + Send + Sync + 'static {
    let runs = Arc::new(AtomicU64::new(0));
    move |req: &ExchangeReq| -> Result<Vec<u8>, String> {
        let response = answer(&client, &config, &runner, &handle, &runs, &req.payload);
        Ok(response.to_bytes())
    }
}

/// Parse, plan, prepare, then run or accept. Every branch resolves to a
/// [`GyldResponse`].
fn answer(
    client: &GladeClient,
    config: &Arc<GyldConfig>,
    runner: &Arc<dyn Runner>,
    handle: &Handle,
    runs: &Arc<AtomicU64>,
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
    let plan = match verbs::plan(&config.layout, &request, latest.as_deref(), &stamp) {
        Ok(p) => p,
        Err(e) => {
            return GyldResponse::failed(e, who);
        }
    };
    let run_id = format!("run-{}", runs.fetch_add(1, Ordering::SeqCst) + 1);

    if let Err(e) = write_overlay(&plan) {
        return GyldResponse::failed(e, who);
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

/// Write the planned overlay module, refusing to clobber one unless the plan
/// says so. A write failure is data.
fn write_overlay(plan: &Plan) -> Result<(), String> {
    let write = match plan.write.as_ref() {
        Some(w) => w,
        None => {
            return Ok(());
        }
    };
    if !write.force && write.path.exists() {
        return Err(format!(
            "{} exists; nothing is overwritten",
            write.path.display()
        ));
    }
    if let Some(parent) = write.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&write.path, &write.text)
        .map_err(|e| format!("cannot write {}: {e}", write.path.display()))
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

/// Run the plan on a blocking task, appending every output line to the log
/// surface keyed by `run_id`, then a terminal `{done:true, exit}` record.
/// Best effort: an append failure (a link drop mid-run) is dropped, because the
/// exchange answer already carried the run id.
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
    handle.spawn(async move {
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
    });
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
