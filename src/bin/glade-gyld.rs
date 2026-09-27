//! `glade-gyld` — attach a Gyld decision-stream supplier to a glade node and
//! serve until SIGTERM or SIGINT (GyldGrythPlugins.md phase 4).
//!
//! ```text
//! glade-gyld --node ws://127.0.0.1:PORT --gyld-root DIR --bundle-root DIR
//!            [--decisions-root DIR]
//!            [--share ws-razel] [--glade-id gyld.ops] [--output-id gyld.output]
//!            [--ask-id gyld.ask]
//!            [--streams-id gyld.streams] [--stream-id gyld.stream]
//!            [--decisions-id gyld.decisions] [--lens-id gyld.lens]
//!            [--file-id gyld.file] [--static-base /gyld] [--principal P]
//!            [--python /opt/homebrew/bin/python3.13]
//!            [--timeout-secs 600] [--max-output-bytes 1048576]
//!            [--agent-model claude-opus-5] [--agent-key-file FILE]
//!            [--agent-base-url https://api.anthropic.com] [--agent-compat anthropic|ollama]
//!            [--agent-max-input-tokens 200000] [--agent-max-output-tokens 64000]
//!            [--agent-max-conversation-tokens 1000000]
//! ```
//!
//! It connects, attaches as THE provider for `(share, glade_id)`, reattaches on
//! link drop (the kit helper), and on a signal ends every running host's whole
//! tree, then tears the session down cleanly, waiting at most about a second
//! for work still in flight.
//!
//! **A flag that was not passed sets nothing.** grazel spawns this binary with
//! a fixed argument list and none of the `--agent-*` flags in it, so the
//! configuration that reaches a running desk comes from
//! `<bundle-root>/agent/config.json` and the environment
//! (`glade_gyld::agent`). The flags here are the TOP of that precedence, and
//! they are collected as options precisely so an unpassed one does not overrule
//! a file with a default nobody chose.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use glade_gyld::{
    serve, AgentOverrides, Compat, Environment, GyldConfig, Limits, ShutdownSignals, Surfaces,
    DEFAULT_ASK_ID, DEFAULT_GLADE_ID, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_OUTPUT_ID, DEFAULT_PYTHON,
    DEFAULT_SHARE, DEFAULT_TIMEOUT_SECS,
};

const USAGE: &str = "usage: glade-gyld --node ws://HOST:PORT --gyld-root DIR --bundle-root DIR \
[--decisions-root DIR] \
[--share ws-razel] [--glade-id gyld.ops] [--output-id gyld.output] [--ask-id gyld.ask] \
[--streams-id gyld.streams] [--stream-id gyld.stream] [--decisions-id gyld.decisions] \
[--lens-id gyld.lens] [--file-id gyld.file] [--static-base /gyld] [--principal P] \
[--python /opt/homebrew/bin/python3.13] [--timeout-secs 600] [--max-output-bytes 1048576] \
[--agent-model claude-opus-5] [--agent-key-file FILE] \
[--agent-base-url https://api.anthropic.com] [--agent-compat anthropic|ollama] \
[--agent-max-input-tokens 200000] [--agent-max-output-tokens 64000] \
[--agent-max-conversation-tokens 1000000]";

/// How long stopping waits for work still in flight once the hosts are ended.
const IN_FLIGHT_WITHIN: Duration = Duration::from_secs(1);

/// The parsed CLI: the supplier config plus the interpreter to run the hosts.
struct Args {
    config: GyldConfig,
    python: PathBuf,
}

fn main() -> ExitCode {
    // The shutdown signals, blocked FIRST, before any thread exists, so every
    // thread inherits the block and one thread takes them: grazel's pattern,
    // with no handler installed (G4, `glade_gyld::signals`).
    let signals = ShutdownSignals::block();
    // The environment, read ONCE, here, and handed down through `GyldConfig`:
    // nothing below this line reads the process's own. The whole of it, so a
    // child the supplier spawns can be given exactly what it would have
    // inherited, and the snapshot prints names only, never a value.
    let env = Environment::of(std::env::vars_os());
    let args = match parse_args(std::env::args().skip(1).collect(), env) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("glade-gyld: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    // Built here, after the block, and not by `#[tokio::main]`, which starts
    // its threads before the first line of `main`.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("glade-gyld: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let served = runtime.block_on(run(args, signals));
    // Dropped, the runtime would wait for every blocking task to return, and an
    // `explain` whose model call waits on an answer returns only when that
    // call's own clock runs out, minutes away. `run` has ended the hosts by
    // now, so what is left gets a second and is then left behind (2026-09-28).
    runtime.shutdown_timeout(IN_FLIGHT_WITHIN);
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glade-gyld: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args, signals: ShutdownSignals) -> std::io::Result<()> {
    let config = args.config;
    eprintln!(
        "glade-gyld: attaching to {} as {}/{} (gyld-root {}, bundle-root {}, decisions-root {}, \
         principal {})",
        config.node_url,
        config.share,
        config.glade_id,
        config.layout.gyld_root.display(),
        config.layout.bundle_root.display(),
        // Said either way: a desk whose rulings land in its own scratch tree is
        // a desk whose rulings nobody will find in git, and that is worth
        // reading in a log before anybody answers a question.
        match config.layout.decisions_root.as_ref() {
            Some(root) => root.display().to_string(),
            None => "none".to_string(),
        },
        config.principal.as_deref().unwrap_or("<none>"),
    );
    let mut stop = signals.watch()?;
    // A signal while attaching stops there: attaching starts no host until
    // the moment it is done.
    let supplier = tokio::select! {
        served = serve(config, args.python) => served?,
        _ = &mut stop => {
            eprintln!("glade-gyld: signal received while attaching; stopping");
            return Ok(());
        }
    };
    eprintln!("glade-gyld: serving; SIGTERM/SIGINT to stop");

    let _ = stop.await;
    eprintln!("glade-gyld: signal received; ending running hosts, then detaching");
    supplier.shutdown().await;
    Ok(())
}

/// A tiny hand-rolled flag parser (the crate stays dep-light — no clap).
/// `--node`, `--gyld-root` and `--bundle-root` are required; the rest default.
/// `env` is the environment `main` captured, carried into the config as it is.
fn parse_args(args: Vec<String>, env: Environment) -> Result<Args, String> {
    let mut node: Option<String> = None;
    let mut gyld_root: Option<PathBuf> = None;
    let mut bundle_root: Option<PathBuf> = None;
    let mut decisions_root: Option<PathBuf> = None;
    let mut share = DEFAULT_SHARE.to_string();
    let mut glade_id = DEFAULT_GLADE_ID.to_string();
    let mut output_id = DEFAULT_OUTPUT_ID.to_string();
    let mut ask_id = DEFAULT_ASK_ID.to_string();
    let mut surfaces = Surfaces::default();
    let mut principal: Option<String> = None;
    let mut python = PathBuf::from(DEFAULT_PYTHON);
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;
    let mut agent = AgentOverrides::default();

    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut take = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--node" => node = Some(take("--node")?),
            "--gyld-root" => gyld_root = Some(PathBuf::from(take("--gyld-root")?)),
            "--bundle-root" => bundle_root = Some(PathBuf::from(take("--bundle-root")?)),
            "--decisions-root" => {
                decisions_root = Some(PathBuf::from(take("--decisions-root")?));
            }
            "--share" => share = take("--share")?,
            "--glade-id" => glade_id = take("--glade-id")?,
            "--output-id" => output_id = take("--output-id")?,
            "--ask-id" => ask_id = take("--ask-id")?,
            "--streams-id" => surfaces.streams_id = take("--streams-id")?,
            "--stream-id" => surfaces.stream_id = take("--stream-id")?,
            "--decisions-id" => surfaces.decisions_id = take("--decisions-id")?,
            "--lens-id" => surfaces.lens_id = take("--lens-id")?,
            "--file-id" => surfaces.file_id = take("--file-id")?,
            "--static-base" => surfaces.static_base = take("--static-base")?,
            "--principal" => principal = Some(take("--principal")?),
            "--agent-model" => agent.model = Some(take("--agent-model")?),
            "--agent-base-url" => agent.base_url = Some(take("--agent-base-url")?),
            "--agent-compat" => {
                agent.compat = Some(Compat::parse(&take("--agent-compat")?)?);
            }
            "--agent-key-file" => {
                agent.key_file = Some(PathBuf::from(take("--agent-key-file")?));
            }
            "--agent-max-input-tokens" => {
                agent.max_input_tokens = Some(
                    take("--agent-max-input-tokens")?
                        .parse()
                        .map_err(|_| "--agent-max-input-tokens must be an integer".to_string())?,
                );
            }
            "--agent-max-conversation-tokens" => {
                agent.max_conversation_tokens = Some(
                    take("--agent-max-conversation-tokens")?
                        .parse()
                        .map_err(|_| {
                            "--agent-max-conversation-tokens must be an integer".to_string()
                        })?,
                );
            }
            "--agent-max-output-tokens" => {
                agent.max_tokens = Some(
                    take("--agent-max-output-tokens")?
                        .parse()
                        .map_err(|_| "--agent-max-output-tokens must be an integer".to_string())?,
                );
            }
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
    // Optional, and unset means the earlier arrangement: a written overlay lives
    // in the bundle root's own overlays tree.
    config.layout = config.layout.with_decisions_root(decisions_root);
    config.share = share;
    config.glade_id = glade_id;
    config.output_id = output_id;
    config.ask_id = ask_id;
    config.surfaces = surfaces;
    config.principal = principal;
    config.limits = Limits {
        timeout: Duration::from_secs(timeout_secs),
        max_output_bytes,
    };
    config.agent = agent;
    config.env = env;
    Ok(Args { config, python })
}
