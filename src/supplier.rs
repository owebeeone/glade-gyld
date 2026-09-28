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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use glade_client::supplier::{Supplier, SupplierConfig, SupplierSurface};
use glade_client::{GladeClient, OpOutcome, SubscribeOutcome};
use glade_wire::generated::ExchangeReq;

use crate::agent::{self, AgentOverrides, Resolved};
use crate::ask::{self, AgentState, AskDraft, Consultation};
use crate::bundle::{self, Layout};
use crate::conversation::{self, Ledger};
use crate::envelope::{GyldAskRecord, GyldOutputRecord, GyldRequest, GyldResponse, Refusal};
use crate::environment::Environment;
use crate::exec::{Hosts, Limits, PythonRunner, RunOutput, Runner};
use crate::github::{self, Token};
use crate::model::{self, ModelClient, ModelConfig, ModelEvent, ModelRequest};
use crate::outcome::{self, Put, Snapshot};
use crate::prompt::{self, Prompt};
use crate::publish::{self, Surfaces};
use crate::sources::{self, ResolvedSource};
use crate::tools;
use crate::toolset;
use crate::verbs::{self, Plan};

mod attach;
mod config;
mod consultation;
mod exchange;
mod first_build;
mod gate;
mod landing;
mod merge;
mod notebook;
mod publication;
mod run_ids;
mod stream;
mod writer;

pub use attach::*;
pub use config::*;
pub use gate::*;
pub use run_ids::*;

use consultation::*;
use exchange::*;
use first_build::*;
use landing::*;
use merge::*;
use notebook::*;
use publication::*;
use stream::*;
use writer::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::GyldRequest;

    mod attach;
    mod config;
    mod consultation;
    mod exchange;
    mod first_build;
    mod gate;
    mod landing;
    mod merge;
    mod notebook;
    mod run_ids;
    mod stream;

    /// A bundle root of its own, removed by the caller.
    fn root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("glade-gyld-attach-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A plan whose only business is one forced write at `path`.
    fn forced_write(path: &std::path::Path, text: &str) -> Plan {
        let mut plan = verbs::plan(
            &Layout::new(PathBuf::from("/g"), PathBuf::from("/b")),
            &GyldRequest::parse(br#"{"verb":"fork","args":{"parent":"base","stream":"a-b"}}"#)
                .unwrap(),
            None,
            "build-1",
            "run-7",
            &AgentState::default(),
        )
        .unwrap();
        plan.write = Some(verbs::PlannedWrite {
            path: path.to_path_buf(),
            text: text.to_string(),
            force: true,
        });
        plan
    }

    /// An `answer` plan on `stream`, building into `stamp`.
    fn answering(layout: &Layout, stream: &str, stamp: &str) -> Plan {
        let notebook = layout.overlay_home().join(verbs::overlay_file(stream));
        Plan {
            overlay: Some(notebook.clone()),
            stream: Some(stream.to_string()),
            output_dir: Some(layout.new_build_dir(stamp)),
            verb: "answer".into(),
            ..forced_write(&notebook, "the ruling\n")
        }
    }
}
