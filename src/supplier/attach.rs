use super::*;

/// A live gyld supplier: an attached authority session serving the verb
/// exchange. Hold it for the process lifetime; [`GyldSupplier::shutdown`] is the
/// clean teardown.
pub struct GyldSupplier {
    #[allow(dead_code)]
    client: GladeClient,
    supplier: Supplier,
    /// The hosts its runs have running, which the shutdown ends first (G4).
    hosts: Hosts,
}

/// How long the shutdown waits for the runs to end their hosts. Each ends its
/// own within one of the runner's ticks, so this is a bound, not a pace.
const HOSTS_END_WITHIN: Duration = Duration::from_secs(5);

impl GyldSupplier {
    /// End every running host's whole tree, then stop reattaching and close the
    /// session: the SIGTERM path. Each host leads a process group (a job object
    /// on Windows) of its own, which no signal to this process reaches, so a
    /// host left running would outlive the supplier (G4, 2026-09-28).
    pub async fn shutdown(&self) {
        let hosts = self.hosts.clone();
        match tokio::task::spawn_blocking(move || hosts.end_all(HOSTS_END_WITHIN)).await {
            Ok(0) => {}
            Ok(left) => {
                eprintln!(
                    "glade-gyld: {left} host(s) still running {}s into the shutdown",
                    HOSTS_END_WITHIN.as_secs()
                );
            }
            Err(e) => {
                eprintln!("glade-gyld: ending the hosts failed: {e}");
            }
        }
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
    // The github token, discovered here and only here: from the environment the
    // supplier started with, then `gh`. The supplier holds it for its life and
    // hands it on; nothing in the process keeps one.
    let token = github::discover_with_gh(&config.env);
    // And which tools this desk gets, and where the network ones may go. Hosts,
    // counts and the SOURCE of the github token: no key, no token value and no
    // page.
    eprintln!(
        "glade-gyld: agent {}",
        toolset::says(&resolved.config.tools, &token)
    );
    for note in resolved.notes.iter() {
        eprintln!("glade-gyld: agent config: {note}");
    }
    let model = model::HttpsModelClient::new(resolved.config, config.env.clone());
    let runner = PythonRunner::new(python, config.env.clone());
    serve_with(config, Arc::new(runner), Arc::new(model), token).await
}

/// Connect, attach and serve with a caller-supplied runner, model client and
/// github token. The tests drive this one with a recording runner, a scripted
/// model and a token of their own, none or a made-up one, so the whole verb
/// path is exercised with no interpreter, no Gyld checkout and no network in
/// sight.
pub async fn serve_with(
    config: GyldConfig,
    runner: Arc<dyn Runner>,
    model: Arc<dyn ModelClient>,
    token: Token,
) -> io::Result<GyldSupplier> {
    let config = Arc::new(config);
    // The runner's hosts are the supplier's to end.
    let hosts = runner.hosts();
    let client = GladeClient::new(format!("glade-gyld:{}:{}", config.share, config.glade_id));
    client.connect(&config.node_url).await?;

    let supplier = Supplier::attach(
        client.clone(),
        SupplierConfig {
            principal: config.principal.clone(),
            ..Default::default()
        },
    );

    // One writer for the supplier's lifetime: every op this session appends goes
    // through it, and so does its record of the chains it has picked up.
    let writer = Writer::new(client.clone(), config.share.clone());
    // One run counter AND one session tag for the supplier's lifetime, made here
    // because the first build's id comes off the same tag: the clock is read once.
    let runs = Arc::new(Runs::new());
    let first = Arc::new(FirstBuild::new(runs.boot()));
    let handler = make_handler(
        writer.clone(),
        config.clone(),
        runner.clone(),
        model,
        Arc::new(token),
        Handle::current(),
        runs,
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
            spawn_publish(writer.clone(), config.clone(), Handle::current(), dir);
        }
        AtAttach::Bootstrap => {
            spawn_first_build(
                writer.clone(),
                config.clone(),
                runner,
                Handle::current(),
                first,
            );
        }
    }

    Ok(GyldSupplier {
        client,
        supplier,
        hosts,
    })
}

/// What the supplier does with the bundle root it finds when it starts serving.
/// A pure reading of the root: no request has been answered yet, and nothing
/// here writes.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AtAttach {
    /// A build is already there — publish it onto the value shares.
    Publish(PathBuf),
    /// The root holds no build at all.
    Bootstrap,
}

/// Read the bundle root and decide. [`bundle::latest_build`] is the supplier's
/// own authority for "which build is current": `latest.json` first, and failing
/// that the newest `builds/` directory holding a `streams.json`, so a
/// hand-seeded root with no pointer is still a root with a build.
pub(super) fn at_attach(layout: &Layout) -> AtAttach {
    match bundle::latest_build(layout) {
        Some(dir) => AtAttach::Publish(dir),
        None => AtAttach::Bootstrap,
    }
}
