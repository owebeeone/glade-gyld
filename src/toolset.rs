//! The tools this supplier actually offers (GyldAskAgent.md section 11.6).
//!
//! [`crate::tools`] is the machinery — what a tool is, which are enabled, what
//! one call may cost. This is the SET: the concrete tools a build can be asked
//! about, and the one function the supplier calls to offer them.
//!
//! Everything here is read-only over the build `latest.json` names, and over
//! nothing else. There is no network tool in this module: `fetch_url`, `github`
//! and `web_search` are phases B and C, and each arrives with its own
//! configuration and its own refusal when that configuration is absent.

use std::sync::Arc;

use crate::tools::{Tool, ToolContext};

/// Every tool this supplier can offer for `context`'s build.
///
/// The allow-list chooses from this list and never adds to it
/// ([`crate::tools::ToolRegistry::build`]), which is what makes "local tools on
/// by default, network tools off until configured" one rule rather than two: a
/// tool that is not offered cannot be enabled by naming it, and a desk that
/// names nothing gets exactly this list.
pub fn local(_context: &ToolContext) -> Vec<Arc<dyn Tool>> {
    // Phase A step A.2 fills this with `read_source` and `gyld_query`. Until it
    // does, a supplier with the loop in it makes exactly the request it made
    // before section 11 existed: the draft tool alone, one call, one answer.
    Vec::new()
}
