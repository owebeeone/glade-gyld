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

    let handler = make_handler(client.clone(), config.clone(), runner, Handle::current());
    supplier
        .serve_exchange(
            SupplierSurface::new(&config.share, &config.glade_id, "exchange"),
            handler,
        )
        .await?;

    Ok(GyldSupplier { client, supplier })
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
            let dir = finish(config, &plan, &out);
            GyldResponse::ran(run_id, out.exit, out.stdout, out.stderr, dir, who)
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

/// Record a successful build as the bundle root's latest, and answer with the
/// directory it built. A failed run advertises nothing: the previous bundle
/// stands untouched.
fn finish(config: &Arc<GyldConfig>, plan: &Plan, out: &RunOutput) -> Option<String> {
    let dir = plan.output_dir.as_ref()?;
    if out.exit != 0 || !dir.join("streams.json").is_file() {
        return None;
    }
    if let Err(e) = bundle::write_latest(&config.layout, dir) {
        eprintln!("glade-gyld: could not record the latest build: {e}");
    }
    Some(dir.display().to_string())
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
                finish(&config, &plan, &out);
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
