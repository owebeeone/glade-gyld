use super::*;

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
    /// The environment this process STARTED with, captured once by the entry
    /// point and handed down ([`Environment`]). Every variable the supplier
    /// reads is read here: the agent's endpoint and model, the model key, the
    /// GitHub token. Empty unless the caller supplies one, so a test sees the
    /// variables it made up and nothing else.
    pub env: Environment,
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
            env: Environment::default(),
        }
    }

    /// The effective agent configuration, read NOW: the config file under the
    /// app-owned bundle root, the environment the process started with over it,
    /// the flags over both.
    ///
    /// Taken afresh every time, which is the point. grazel spawns this supplier
    /// with a fixed argument list, so the file is the only channel a running
    /// desk has — and a file that were read once at attach would need a
    /// restart of the whole app to change a model.
    pub fn resolve_agent(&self) -> Resolved {
        agent::resolve(&self.layout.bundle_root, &self.env, &self.agent)
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
/// reaches a plan, a prompt, a record or a log line. The variables are the
/// config's snapshot, never the process's own.
pub(super) fn agent_state(config: &GyldConfig, latest: Option<&std::path::Path>) -> AgentState {
    let key_file = config.key_file();
    let named = |name: &str| {
        config
            .env
            .var(name)
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
