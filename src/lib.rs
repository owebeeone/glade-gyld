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
//! * [`bundle`] — the bundle-root layout, the staging repository, containment,
//!   the latest-build pointer and file digests.
//! * [`verbs`] — the allow-list and the PURE planner: a request becomes one host
//!   invocation, or a refusal, with no filesystem effect.
//! * [`exec`] — the bounded runner (wall clock and output bytes).
//! * [`publish`] — what a successful build puts on the value surfaces.
//! * [`supplier`] — [`serve`], [`GyldConfig`], [`GyldSupplier`]: attach and serve.

pub mod bundle;
pub mod envelope;
pub mod exec;
pub mod publish;
pub mod supplier;
pub mod verbs;

pub use bundle::{FilePointer, Layout};
pub use envelope::{GyldArgs, GyldOutputRecord, GyldRequest, GyldResponse};
pub use exec::{
    Limits, PythonRunner, RunOutput, Runner, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_SECS,
};
pub use publish::{Publication, Surfaces, DEFAULT_STATIC_BASE};
pub use supplier::{
    serve, serve_with, GyldConfig, GyldSupplier, DEFAULT_GLADE_ID, DEFAULT_OUTPUT_ID,
    DEFAULT_PYTHON, DEFAULT_SHARE,
};
pub use verbs::{Plan, PlannedWrite, ALLOWED_VERBS};
