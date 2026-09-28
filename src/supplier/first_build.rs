use super::*;

/// The first build the supplier makes for itself, and whether it is still
/// running.
///
/// While it is, a verb that needs a bundle is refused with a message that names
/// the run instead of the flat [`verbs::NO_BUNDLE`]: one IS on its way, and a UI
/// that is told so can wait for it rather than conclude the root is broken.
#[derive(Debug)]
pub(super) struct FirstBuild {
    run_id: String,
    running: std::sync::atomic::AtomicBool,
}

impl FirstBuild {
    /// Handed the id rather than minting one, so the session tag behind it is the
    /// process's single reading of the clock — [`Runs::boot`] is where it comes
    /// from.
    pub(super) fn new(run_id: String) -> FirstBuild {
        FirstBuild {
            run_id,
            running: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn begin(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Landed, or failed: either way it is no longer on its way, and a verb
    /// that still finds no bundle gets the plain refusal back.
    pub(super) fn ended(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Say what the no-bundle refusal really means right now. Every other
    /// refusal passes through untouched.
    pub(super) fn explain(&self, refusal: String) -> String {
        if refusal == verbs::NO_BUNDLE && self.running.load(Ordering::SeqCst) {
            return format!(
                "the first build is in progress (run {}); nothing has landed yet",
                self.run_id
            );
        }
        refusal
    }
}

/// Lay the bundle root and give it its first build, as a STREAMING run on the
/// output surface keyed by this process's own [`FIRST_BUILD_RUN_PREFIX`] id — the
/// same path a `rebuild` takes, so the build lands, `latest.json` is swapped and
/// the census is published by the code that already does all three.
///
/// The supplier keeps serving throughout: this is a task, and the verbs that
/// need a bundle are refused meanwhile with a message that names the run
/// ([`FirstBuild::explain`]). `fork` and `link` are unaffected — they need no
/// bundle. A first build that fails is failure as DATA on the run and one log
/// line; the supplier stays up and the root simply still has no build.
pub(super) fn spawn_first_build(
    writer: Writer,
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
                    writer.clone(),
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
                fail_first_build(&writer, &config, &run_id, &who, e).await;
            }
            Err(e) => {
                fail_first_build(
                    &writer,
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
    writer: &Writer,
    config: &GyldConfig,
    run_id: &str,
    who: &Option<String>,
    reason: String,
) {
    eprintln!("glade-gyld: first build failed: {reason}");
    let record = GyldOutputRecord::line(run_id, 1, who, "stderr", reason);
    append(writer, config, run_id, &record).await;
    append(
        writer,
        config,
        run_id,
        &GyldOutputRecord::end(run_id, 2, who, -1),
    )
    .await;
}
