//! `glade-gyld` — attach a Gyld decision-stream supplier to a glade node and
//! serve until SIGTERM or SIGINT (GyldGrythPlugins.md phase 4).
//!
//! ```text
//! glade-gyld --node ws://127.0.0.1:PORT --gyld-root DIR --bundle-root DIR
//!            [--share ws-razel] [--glade-id gyld.ops] [--output-id gyld.output]
//!            [--streams-id gyld.streams] [--stream-id gyld.stream]
//!            [--decisions-id gyld.decisions] [--lens-id gyld.lens]
//!            [--static-base /gyld] [--principal P]
//!            [--python /opt/homebrew/bin/python3.13]
//!            [--timeout-secs 600] [--max-output-bytes 1048576]
//! ```
//!
//! It connects, attaches as THE provider for `(share, glade_id)`, reattaches on
//! link drop (the kit helper), and tears the session down cleanly on a signal.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use glade_gyld::{
    serve, GyldConfig, Limits, Surfaces, DEFAULT_GLADE_ID, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_OUTPUT_ID, DEFAULT_PYTHON, DEFAULT_SHARE, DEFAULT_TIMEOUT_SECS,
};

const USAGE: &str = "usage: glade-gyld --node ws://HOST:PORT --gyld-root DIR --bundle-root DIR \
[--share ws-razel] [--glade-id gyld.ops] [--output-id gyld.output] \
[--streams-id gyld.streams] [--stream-id gyld.stream] [--decisions-id gyld.decisions] \
[--lens-id gyld.lens] [--static-base /gyld] [--principal P] \
[--python /opt/homebrew/bin/python3.13] [--timeout-secs 600] [--max-output-bytes 1048576]";

/// The parsed CLI: the supplier config plus the interpreter to run the hosts.
struct Args {
    config: GyldConfig,
    python: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("glade-gyld: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glade-gyld: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> std::io::Result<()> {
    let config = args.config;
    eprintln!(
        "glade-gyld: attaching to {} as {}/{} (gyld-root {}, bundle-root {}, principal {})",
        config.node_url,
        config.share,
        config.glade_id,
        config.layout.gyld_root.display(),
        config.layout.bundle_root.display(),
        config.principal.as_deref().unwrap_or("<none>"),
    );
    let supplier = serve(config, args.python).await?;
    eprintln!("glade-gyld: serving; SIGTERM/SIGINT to stop");

    wait_for_shutdown_signal().await;
    eprintln!("glade-gyld: signal received, detaching");
    supplier.shutdown().await;
    Ok(())
}

/// Resolve on SIGTERM or SIGINT (Ctrl-C) — the clean-shutdown trigger.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A tiny hand-rolled flag parser (the crate stays dep-light — no clap).
/// `--node`, `--gyld-root` and `--bundle-root` are required; the rest default.
fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let mut node: Option<String> = None;
    let mut gyld_root: Option<PathBuf> = None;
    let mut bundle_root: Option<PathBuf> = None;
    let mut share = DEFAULT_SHARE.to_string();
    let mut glade_id = DEFAULT_GLADE_ID.to_string();
    let mut output_id = DEFAULT_OUTPUT_ID.to_string();
    let mut surfaces = Surfaces::default();
    let mut principal: Option<String> = None;
    let mut python = PathBuf::from(DEFAULT_PYTHON);
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;

    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut take = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--node" => node = Some(take("--node")?),
            "--gyld-root" => gyld_root = Some(PathBuf::from(take("--gyld-root")?)),
            "--bundle-root" => bundle_root = Some(PathBuf::from(take("--bundle-root")?)),
            "--share" => share = take("--share")?,
            "--glade-id" => glade_id = take("--glade-id")?,
            "--output-id" => output_id = take("--output-id")?,
            "--streams-id" => surfaces.streams_id = take("--streams-id")?,
            "--stream-id" => surfaces.stream_id = take("--stream-id")?,
            "--decisions-id" => surfaces.decisions_id = take("--decisions-id")?,
            "--lens-id" => surfaces.lens_id = take("--lens-id")?,
            "--static-base" => surfaces.static_base = take("--static-base")?,
            "--principal" => principal = Some(take("--principal")?),
            "--python" => python = PathBuf::from(take("--python")?),
            "--timeout-secs" => {
                timeout_secs = take("--timeout-secs")?
                    .parse()
                    .map_err(|_| "--timeout-secs must be an integer".to_string())?;
            }
            "--max-output-bytes" => {
                max_output_bytes = take("--max-output-bytes")?
                    .parse()
                    .map_err(|_| "--max-output-bytes must be an integer".to_string())?;
            }
            "-h" | "--help" => {
                return Err("help".into());
            }
            other => {
                return Err(format!("unknown flag `{other}`"));
            }
        }
    }

    let mut config = GyldConfig::new(
        node.ok_or("--node is required")?,
        gyld_root.ok_or("--gyld-root is required")?,
        bundle_root.ok_or("--bundle-root is required")?,
    );
    config.share = share;
    config.glade_id = glade_id;
    config.output_id = output_id;
    config.surfaces = surfaces;
    config.principal = principal;
    config.limits = Limits {
        timeout: Duration::from_secs(timeout_secs),
        max_output_bytes,
    };
    Ok(Args { config, python })
}
