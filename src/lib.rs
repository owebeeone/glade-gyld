//! a glade supplier: Gyld decision-stream verbs over an EXCHANGE surface, with
//! long-op output streamed onto a LOG surface (GyldGrythPlugins.md phase 4).
//!
//! `glade-gyld` is `glade-gwz`'s sibling and is modelled on it exactly: it
//! attaches over the wire as an ordinary authority session via [`glade_client`]
//! (no node internals, P00-a), stands behind the declared `(ws-razel, gyld.ops)`
//! exchange surface, and runs ALLOW-LISTED verbs as Gyld host subprocesses
//! against CONFIGURED roots. Failure is uniformly data.
//!
//! Two roots, two very different rights:
//!
//! * `--gyld-root` is a Gyld checkout and is READ ONLY. The supplier runs the
//!   hosts out of its `scripts/` and seeds the overlays tree from its
//!   `examples/`. It never writes a byte there, so the checkout's committed
//!   examples stay committed examples.
//! * `--bundle-root` is the app-owned storage the supplier owns outright: the
//!   overlay modules it writes, the staging repository the hosts are pointed at,
//!   and one directory per build. Nothing is ever built over an existing build.
//!
//! Modules:
//! * [`envelope`] — the request, response and output-record JSON shapes.
//! * [`ask`] — the `gyld.ask-context.v1` envelope the `explain` verb consults,
//!   and its refusals (GyldAskAgent.md sections 3 and 4).
//! * [`sources`] — the emitted source index (`gyld.sources.v1`) and the
//!   resolver that grounds an answer in it (section 5).
//! * [`conversation`] — the prior turns a follow-up replays, folded back out of
//!   the ask surface's own records, and what one conversation has spent
//!   (sections 6 and 7).
//! * [`prompt`] — the stance, the emitted context and the passages, composed
//!   into the one stable prefix a turn is cached on (section 7).
//! * [`model`] — the [`model::ModelClient`] trait and its raw-HTTPS
//!   implementation: key discovery, the budgets, the streamed SSE, and the one
//!   tool a draft comes back in (sections 7 and 8).
//! * [`agent`] — how that client is configured when nobody can pass it a flag:
//!   `agent/config.json` under the bundle root, the environment over it, the
//!   flags over both, and the compatibility profile of the endpoint.
//! * [`bundle`] — the bundle-root layout, the staging repository, containment,
//!   the latest-build pointer and file digests.
//! * [`verbs`] — the allow-list and the PURE planner: a request becomes one host
//!   invocation, or a refusal, with no filesystem effect.
//! * [`exec`] — the bounded runner (wall clock and output bytes).
//! * [`publish`] — what a successful build puts on the value surfaces.
//! * [`supplier`] — [`serve`], [`GyldConfig`], [`GyldSupplier`]: attach and serve,
//!   publishing the bundle root's current build the moment it is serving and
//!   making the first build itself when the root has none.

pub mod agent;
pub mod ask;
pub mod bundle;
pub mod conversation;
pub mod envelope;
pub mod exec;
pub mod model;
pub mod prompt;
pub mod publish;
pub mod sources;
pub mod supplier;
pub mod verbs;

pub use agent::{
    resolve as resolve_agent, AgentOverrides, Compat, Resolved, AUTH_TOKEN_ENV, BASE_URL_ENV,
    COMPAT_ENV, DEFAULT_CONFIG_FILE, MODEL_ENV,
};
pub use ask::{
    AgentState, AskContext, AskDraft, AskRefusal, Consultation, ASK_CONTEXT_FORMAT, SOURCES_FILE,
};
pub use bundle::{FilePointer, Layout};
pub use conversation::{Ledger, Turn};
pub use envelope::{
    GyldArgs, GyldAskRecord, GyldOutputRecord, GyldRequest, GyldResponse, ASK_ANSWER, ASK_CITATION,
    ASK_DRAFT, ASK_END, ASK_NOTE, ASK_QUESTION,
};
pub use exec::{
    Limits, PythonRunner, RunOutput, Runner, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_SECS,
};
pub use model::{
    degrade, draft_tool, estimate_tokens, Declined, Fold, ModelClient, ModelConfig, ModelEvent,
    ModelOutcome, ModelRequest, Shape, DEFAULT_AGENT_MODEL, DEFAULT_BASE_URL,
    DEFAULT_MAX_CONVERSATION_TOKENS, DEFAULT_MAX_INPUT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS,
    DRAFT_TOOL, END_TURN, ESTIMATED_CHARS_PER_TOKEN, MAX_DEGRADATIONS, MAX_TOKENS, REFUSAL,
    TOOL_USE,
};
pub use prompt::{compose, Prompt, STANCE};
pub use publish::{Publication, Surfaces, DEFAULT_STATIC_BASE};
pub use sources::{ResolvedSource, SourceIndex, SOURCES_FORMAT};
pub use supplier::{
    serve, serve_with, GyldConfig, GyldSupplier, DEFAULT_ASK_ID, DEFAULT_GLADE_ID,
    DEFAULT_OUTPUT_ID, DEFAULT_PYTHON, DEFAULT_SHARE, FIRST_BUILD_RUN_ID,
};
pub use verbs::{Plan, PlannedWrite, ALLOWED_VERBS, NO_BUNDLE};
