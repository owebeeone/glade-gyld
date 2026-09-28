use super::*;

/// The model `--agent-model` defaults to. Taken from the `claude-api` skill's
/// model table rather than from memory: `claude-opus-5`, 1M context, $5.00 per
/// 1M input tokens and $25.00 per 1M output tokens at first-party rates.
pub const DEFAULT_AGENT_MODEL: &str = "claude-opus-5";

/// The Messages API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The per-run input budget. Generous next to one grounded envelope, and still
/// a bound: it is what stops a pathological context costing real money.
pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 200_000;

/// The per-run output budget, and the request's `max_tokens`. The streaming
/// default: a grounded answer is long output.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// The wall clock one consultation gets.
pub const DEFAULT_MODEL_TIMEOUT_SECS: u64 = 300;

/// The per-CONVERSATION ceiling: every token every turn of one conversation
/// spent, summed. A turn that would cross it is refused before it is sent.
///
/// The per-run budgets bound one question. This one bounds the thread: prior
/// turns are replayed into every follow-up, so a conversation left running is
/// the one thing here that grows on its own. The default is this model's own
/// context window, which is the largest a single turn could ever be — several
/// turns of it, not one. `0` lifts the ceiling.
pub const DEFAULT_MAX_CONVERSATION_TOKENS: u64 = 1_000_000;

/// What a request may carry beyond the bare Messages API — and therefore what
/// is DROPPED when an endpoint will not take it.
///
/// Both are optimisations, not meaning: `strict` guarantees the draft tool's
/// input validates, `cache_control` makes a follow-up read the passages from
/// the cache instead of paying for them again. A turn sent without either is
/// the same turn, more expensive and less checked. That is why an endpoint
/// that rejects one is answered by dropping it and saying so, rather than by
/// failing the reader's question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// `strict: true` on the draft tool.
    pub strict: bool,
    /// The two `cache_control` breakpoints.
    pub cache_control: bool,
}

impl Shape {
    /// Everything on: what the Anthropic path has always sent, and what every
    /// first attempt sends.
    pub fn full() -> Shape {
        Shape {
            strict: true,
            cache_control: true,
        }
    }

    /// The same request with `strict` dropped from the tool.
    pub fn without_strict(self) -> Shape {
        Shape {
            strict: false,
            ..self
        }
    }

    /// The same request with no cache breakpoints.
    pub fn without_cache_control(self) -> Shape {
        Shape {
            cache_control: false,
            ..self
        }
    }
}

impl Default for Shape {
    fn default() -> Shape {
        Shape::full()
    }
}

/// Everything the model call is configured with. No key: see the module note.
///
/// It is resolved per call from [`crate::agent::resolve`] — a config file, the
/// environment and the flags — so the endpoint and the model can change under a
/// running supplier.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelConfig {
    pub model: String,
    pub base_url: String,
    /// Which dialect of the Messages API `base_url` speaks.
    pub compat: Compat,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    /// The running total one conversation may spend across its turns; `0` is
    /// no ceiling.
    pub max_conversation_tokens: u64,
    pub timeout: Duration,
    /// Where a key is read from when the environment carries none.
    pub key_file: PathBuf,
    /// Ask the endpoint to count the input before the call. False where the
    /// endpoint has no `count_tokens`, and the budget is estimated instead.
    pub count_tokens: bool,
    /// What the FIRST request of a call carries. The client degrades from here
    /// on what the endpoint actually rejects.
    pub shape: Shape,
    /// Which tools this desk lets the agent reach for, and what one turn's tool
    /// use may cost (GyldAskAgent.md section 11.2, 11.3).
    pub tools: ToolPolicy,
}

impl Default for ModelConfig {
    fn default() -> ModelConfig {
        ModelConfig {
            model: DEFAULT_AGENT_MODEL.into(),
            base_url: DEFAULT_BASE_URL.into(),
            compat: Compat::Anthropic,
            max_input_tokens: DEFAULT_MAX_INPUT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            max_conversation_tokens: DEFAULT_MAX_CONVERSATION_TOKENS,
            timeout: Duration::from_secs(DEFAULT_MODEL_TIMEOUT_SECS),
            key_file: PathBuf::new(),
            count_tokens: true,
            shape: Shape::full(),
            tools: ToolPolicy::default(),
        }
    }
}

impl ModelConfig {
    /// The default with the bundle root's own key file in it: what an
    /// unconfigured supplier resolves to.
    pub fn default_at(bundle_root: &Path) -> ModelConfig {
        ModelConfig {
            key_file: bundle_root.join(crate::ask::DEFAULT_KEY_FILE),
            ..ModelConfig::default()
        }
    }
}
