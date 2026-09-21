//! The verb allow-list and the PURE planner: a request plus the two configured
//! roots become a [`Plan`], or a refusal, with no filesystem effect at all.
//!
//! Three guards stand between a request and a Gyld host:
//!
//! 1. [`ALLOWED_VERBS`] — anything else is refused as data.
//! 2. [`valid_stream_id`] — every id a verb names must match Gyld's own
//!    `ID_PATTERN` (`manage_decision_streams.py`), so an id can never carry a
//!    path separator, a flag, or a shell metacharacter into an argv.
//! 3. [`crate::bundle::contained`] — every path the plan writes or builds into
//!    must stay under the bundle root.
//!
//! The argv is BUILT from typed fields, never passed through: [`GyldArgs`] has
//! no free-form argument list, so no requester composes a command line. Each
//! mutating verb maps to exactly ONE Gyld host invocation, and every one of them
//! builds into a NEW directory: nothing is ever edited in place.
//!
//! Two verbs run no host at all. `list` READS the latest build's stream listing
//! ([`Plan::read`]); `explain` CONSULTS ([`Plan::consult`]) — it resolves an ask
//! envelope against the build's source index and calls a model, with no
//! subprocess and no filesystem effect whatever. Neither ever carries an argv,
//! and `explain` carries no [`PlannedWrite`] either, so "the agent never writes
//! an overlay" is a property of the plan type rather than a promise in prose.

use std::path::PathBuf;

use crate::ask::{AgentState, Consultation};
use crate::bundle::{contained, Layout};
use crate::envelope::{GyldArgs, GyldRequest};

/// The allow-list (GyldGrythPlugins.md 4.7 step 4.1, GyldAskAgent.md section 4).
///
/// `list` and `explain` are the two verbs with no subprocess: `list` reads the
/// latest build's `streams.json` off disk and `explain` consults a model about
/// what the build already emitted. The other six are the write path; each
/// produces a new build or a new document and never overwrites one.
///
/// `explain`, not `ask`: `ask` is taken and the collision is not cosmetic — it
/// means APPEND A QUESTION TO A STREAM'S OVERLAY AND REBUILD, which writes Gyld
/// source. A second meaning on that name would put a verb that writes and a
/// verb that writes nothing behind one word, and make `attributed_to`
/// unreadable in an audit trail.
///
/// EXCLUDED for now and why: `occurred`, `lens` and `inspect` (section 4.7 names
/// them, but no Gyld host verb exists for them yet), and everything else,
/// because the supplier only ever runs the hosts it can name.
pub const ALLOWED_VERBS: &[&str] = &[
    "list", "answer", "ask", "explain", "fork", "link", "rebuild", "diff",
];

/// The Gyld host that owns the four stream-manager verbs.
pub const MANAGER_HOST: &str = "manage_decision_streams.py";

/// The Gyld host that WRITES a bundle. Every other verb reaches it through
/// `manage_decision_streams.py rebuild`, which re-captures an existing bundle;
/// the FIRST build has no bundle to re-capture, so the supplier runs the writer
/// itself ([`first_build_plan`]).
pub const EMIT_HOST: &str = "emit_decision_streams.py";

/// The refusal a verb that needs a bundle gets before there is one.
///
/// Named rather than repeated because the supplier REWRITES it while it is
/// running its own first build: "no bundle has been built yet" is the wrong
/// thing to say when one is on its way.
pub const NO_BUNDLE: &str = "no bundle has been built yet";

/// The base entry module every generated stream's module name extends.
const ENTRY: &str = "glade_decisions";

/// The longest stream id the supplier accepts. Gyld imposes none; a bound here
/// keeps a pathological id out of an argv and off the filesystem.
const MAX_STREAM_ID: usize = 64;

/// The largest overlay module the supplier will write. Overlay modules are
/// hand-sized source files; anything larger is a mistake or an attack.
pub const MAX_OVERLAY_BYTES: usize = 1 << 20;

/// Is `verb` on the allow-list?
pub fn verb_allowed(verb: &str) -> bool {
    ALLOWED_VERBS.contains(&verb)
}

/// Gyld's `ID_PATTERN`: `[a-z][a-z0-9]*(-[a-z0-9]+)*`, fully matched.
pub fn valid_stream_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_STREAM_ID {
        return false;
    }
    let mut parts = id.split('-');
    let first = match parts.next() {
        Some(p) => p,
        None => {
            return false;
        }
    };
    if !first.starts_with(|c: char| c.is_ascii_lowercase()) {
        return false;
    }
    if !first
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }
    for part in parts {
        if part.is_empty() {
            return false;
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return false;
        }
    }
    true
}

/// The overlay module name a stream id takes, the same rule `names()` follows.
pub fn overlay_module(stream: &str) -> String {
    format!("{ENTRY}_{}", stream.replace('-', "_"))
}

/// The overlay file name a stream id takes: the module with underscores turned
/// back into hyphens, plus the authoring suffix.
pub fn overlay_file(stream: &str) -> String {
    format!("{}.gyld.py", overlay_module(stream).replace('_', "-"))
}

/// A file the plan writes before it runs anything.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedWrite {
    pub path: PathBuf,
    pub text: String,
    /// Overwrite an existing file. `false` refuses rather than clobbering.
    pub force: bool,
}

/// What one request resolves to. Exactly one of `read` (the `list` verb),
/// `consult` (the `explain` verb) or `argv` (everything else) is set.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub verb: String,
    /// The overlay module to write first, for `answer` and `ask`.
    pub write: Option<PlannedWrite>,
    /// The host argv, INCLUDING the script path as argv[0] for the interpreter.
    pub argv: Vec<String>,
    /// The working directory for the host: always the Gyld root.
    pub cwd: PathBuf,
    /// `PYTHONPATH` for the host.
    pub pythonpath: String,
    /// The new bundle directory this verb builds, when it builds one.
    pub output_dir: Option<PathBuf>,
    /// The document this verb reads instead of running a host (`list`).
    pub read: Option<PathBuf>,
    /// The consultation this verb runs instead of a host (`explain`): the
    /// validated envelope, the index it is resolved against and the
    /// conversation the reply is keyed by. Never accompanied by a `write` or
    /// an `argv`.
    pub consult: Option<Consultation>,
    /// The overlay module FILE this verb leaves behind — the notebook, in the
    /// decisions root when one is configured.
    ///
    /// Set for the four verbs that produce one: `answer` and `ask` (which the
    /// supplier writes itself, so it is `write.path` too) and `fork` and `link`
    /// (which a Gyld host writes into the staging tree, from where it is
    /// adopted). It is what the answer tells the requester, so a ruling is never
    /// a file nobody named.
    pub overlay: Option<PathBuf>,
    /// The stream this verb WROTE, for the same four verbs that leave an
    /// `overlay` — the one whose validation document says whether the write made
    /// things worse ([`crate::outcome::classify`]). `rebuild` and `diff` write no
    /// stream of their own and name none.
    pub stream: Option<String>,
}

/// Resolve a request into a [`Plan`], or a refusal. PURE: it touches no file,
/// so the same refusal is produced whether or not the bundle root exists yet.
///
/// `latest` is the caller's answer to "what is the current bundle", which the
/// bundle-reading verbs need and the two generating verbs do not. `agent` is
/// the caller's already-taken reading of the agent's world — a key was found,
/// the build emitted an index, these are the streams it lists — handed in as
/// DATA so `explain`'s refusals are produced here too, with no environment
/// read and no file stat anywhere in this function.
pub fn plan(
    layout: &Layout,
    request: &GyldRequest,
    latest: Option<&std::path::Path>,
    stamp: &str,
    agent: &AgentState,
) -> Result<Plan, String> {
    if request.verb.is_empty() {
        return Err("envelope missing `verb`".into());
    }
    if !verb_allowed(&request.verb) {
        return Err(format!(
            "verb `{}` not in the allow-list {ALLOWED_VERBS:?}",
            request.verb
        ));
    }
    let args = &request.args;
    let host = layout.script(MANAGER_HOST);
    let stage = layout.stage();
    let base = |argv: Vec<String>| -> Vec<String> {
        let mut v = vec![
            host.display().to_string(),
            "--repository".into(),
            stage.display().to_string(),
        ];
        v.extend(argv);
        v
    };

    let mut plan = Plan {
        verb: request.verb.clone(),
        write: None,
        argv: Vec::new(),
        cwd: layout.gyld_root.clone(),
        pythonpath: layout.pythonpath(),
        output_dir: None,
        read: None,
        consult: None,
        overlay: None,
        stream: None,
    };

    match request.verb.as_str() {
        "list" => {
            let bundle = latest.ok_or(NO_BUNDLE)?;
            plan.read = Some(bundle.join("streams.json"));
        }
        "explain" => {
            // The one verb that runs no host AND touches no file: it resolves
            // the envelope against the build's source index and consults a
            // model. No `write`, no `argv`, no `output_dir` — the agent has no
            // path by which to become a Gyld fact.
            let bundle = latest.ok_or(NO_BUNDLE)?;
            let consult = Consultation::resolve(bundle, args.context.as_ref(), agent)
                .map_err(|refusal| refusal.says())?;
            plan.consult = Some(consult);
        }
        "answer" | "ask" => {
            let stream = require_stream(args.stream.as_deref(), "stream")?;
            let text = overlay_text(&request.verb, args)?;
            let bundle = latest.ok_or(NO_BUNDLE)?;
            let output = layout.new_build_dir(stamp);
            // The notebook's home, which is the decisions root when the app
            // configured one: a ruling is the owner's file, and it is written
            // where he keeps his files. The staging tree LINKS to it.
            let notebook = layout.overlay_home().join(overlay_file(stream));
            plan.write = Some(PlannedWrite {
                path: notebook.clone(),
                text,
                // An answer or a question is appended to a stream that already
                // exists, so its module is replaced by the exported text.
                force: true,
            });
            plan.overlay = Some(notebook);
            plan.stream = Some(stream.to_string());
            plan.argv = base(rebuild_argv(bundle, &output, args.built.as_deref()));
            plan.output_dir = Some(output);
        }
        "fork" | "link" => {
            let parent = require_stream(args.parent.as_deref(), "parent")?;
            let stream = require_stream(args.stream.as_deref(), "stream")?;
            if parent == stream {
                return Err("a stream cannot be its own parent".into());
            }
            let mut argv = vec![request.verb.clone(), parent.to_string(), stream.to_string()];
            // No `--output`: the host's default is `<repository>/examples`,
            // which IS the bundle root's overlays tree, and only a module
            // written there is one the capture host can build.
            if let Some(note) = args.note.as_deref() {
                argv.push("--note".into());
                argv.push(one_line(note));
            }
            if args.force {
                argv.push("--force".into());
            }
            plan.argv = base(argv);
            // The host writes the module into the staging tree itself; the
            // notebook it becomes is the one the adoption leaves in the home.
            plan.overlay = Some(layout.overlay_home().join(overlay_file(stream)));
            plan.stream = Some(stream.to_string());
        }
        "rebuild" => {
            let bundle = latest.ok_or(NO_BUNDLE)?;
            let output = layout.new_build_dir(stamp);
            plan.argv = base(rebuild_argv(bundle, &output, args.built.as_deref()));
            plan.output_dir = Some(output);
        }
        "diff" => {
            let left = require_stream(args.left.as_deref(), "left")?;
            let right = require_stream(args.right.as_deref(), "right")?;
            if left == right {
                return Err("a diff needs two different streams".into());
            }
            let bundle = latest.ok_or(NO_BUNDLE)?;
            let mut argv = vec![
                "diff".into(),
                left.to_string(),
                right.to_string(),
                "--bundle".into(),
                bundle.display().to_string(),
            ];
            if args.force {
                argv.push("--force".into());
            }
            plan.argv = base(argv);
        }
        other => {
            return Err(format!(
                "verb `{other}` not in the allow-list {ALLOWED_VERBS:?}"
            ));
        }
    }

    // Containment, last and unconditional. A path this plan BUILDS INTO or READS
    // must be under the bundle root; a path it WRITES may also be under the
    // decisions root, which is the one other tree the app gave the supplier.
    // The bundle a verb was handed is checked too, so a stale pointer cannot aim
    // a build elsewhere.
    let root = &layout.bundle_root;
    for path in plan
        .output_dir
        .iter()
        .chain(plan.read.iter())
        .chain(plan.consult.iter().map(|c| &c.sources))
    {
        if !contained(root, path) {
            return Err(format!(
                "path {} leaves the bundle root {}",
                path.display(),
                root.display()
            ));
        }
    }
    for path in plan
        .write
        .iter()
        .map(|w| &w.path)
        .chain(plan.overlay.iter())
    {
        if !writable(layout, path) {
            return Err(format!("path {} leaves {}", path.display(), homes(layout)));
        }
    }
    if let Some(bundle) = latest {
        if !contained(root, bundle) {
            return Err(format!(
                "bundle {} leaves the bundle root {}",
                bundle.display(),
                root.display()
            ));
        }
    }
    Ok(plan)
}

/// Ask the Gyld checkout which streams the staging repository declares.
///
/// `manage_decision_streams.py rebuild` is one line of exactly this —
/// `streams = discover(repository)` — and it then hands each id to the emit host
/// as `--stream`. Nothing is reimplemented here: the checkout owns the answer to
/// "which streams are there", so the checkout is asked, with its own `discover`.
/// Ask it any other way and the first build lists two streams where a Rebuild
/// lists five.
///
/// The snippet is a CONSTANT and its one operand is the staging repository, an
/// app-owned path the supplier composed itself. No request reaches it.
const DISCOVER: &str = "import json\n\
     import sys\n\
     from pathlib import Path\n\
     from scripts.capture_decision_stream import discover\n\
     from scripts.emit_decision_streams import STREAM_ID\n\
     found = discover(Path(sys.argv[1]))\n\
     print(json.dumps(sorted(n for n in found if n != STREAM_ID)))\n";

/// The plan that runs [`DISCOVER`]. Builds nothing, writes nothing and reads
/// nothing of the bundle root but its staging repository.
pub fn discover_plan(layout: &Layout) -> Plan {
    Plan {
        verb: "discover".into(),
        write: None,
        argv: vec![
            "-c".into(),
            DISCOVER.into(),
            layout.stage().display().to_string(),
        ],
        cwd: layout.gyld_root.clone(),
        pythonpath: layout.pythonpath(),
        output_dir: None,
        read: None,
        consult: None,
        overlay: None,
        stream: None,
    }
}

/// The stream ids a [`discover_plan`] run printed: the LAST line of its stdout
/// that is a JSON array of ids, so a host that says something on the way still
/// answers. Every id is checked against [`valid_stream_id`] before it can reach
/// an argv — the checkout is trusted to be honest, not to be well-formed.
pub fn declared_streams(stdout: &str) -> Vec<String> {
    for line in stdout.lines().rev() {
        let parsed: Vec<String> = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => {
                continue;
            }
        };
        return parsed
            .into_iter()
            .filter(|id| valid_stream_id(id))
            .collect();
    }
    Vec::new()
}

/// The FIRST build of a bundle root that has none.
///
/// `rebuild` re-captures a bundle and so needs one; with nothing to re-capture
/// the writer host is run directly over the staging repository, naming every
/// declared stream the way `rebuild` names them. `--architecture` emits the
/// architecture lineage beside the decision one, which is what a later `rebuild`
/// carries forward on its own (it adds the flag back whenever the bundle it
/// replaces holds a record of another lineage), so the first build and every
/// build after it list the same streams.
///
/// PURE, like [`plan`], and it needs no containment check: the output is
/// [`Layout::new_build_dir`] and the repository is [`Layout::stage`], both of
/// them the supplier's own paths under its own root, with no request in sight.
pub fn first_build_plan(layout: &Layout, stamp: &str, declared: &[String]) -> Plan {
    let mut argv = vec![
        layout.script(EMIT_HOST).display().to_string(),
        "--repository".into(),
        layout.stage().display().to_string(),
        "--output".into(),
        layout.new_build_dir(stamp).display().to_string(),
        "--architecture".into(),
    ];
    for stream in declared.iter().filter(|id| valid_stream_id(id)) {
        argv.push("--stream".into());
        argv.push(stream.clone());
    }
    Plan {
        verb: "rebuild".into(),
        write: None,
        argv,
        cwd: layout.gyld_root.clone(),
        pythonpath: layout.pythonpath(),
        output_dir: Some(layout.new_build_dir(stamp)),
        read: None,
        consult: None,
        overlay: None,
        stream: None,
    }
}

/// `rebuild --bundle <latest> --output <new> [--built ISO]` — the one host call
/// that re-captures a bundle into a directory that does not exist yet.
fn rebuild_argv(
    bundle: &std::path::Path,
    output: &std::path::Path,
    built: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "rebuild".to_string(),
        "--bundle".into(),
        bundle.display().to_string(),
        "--output".into(),
        output.display().to_string(),
    ];
    if let Some(built) = built {
        argv.push("--built".into());
        argv.push(one_line(built));
    }
    argv
}

/// The overlay module text an `answer` or an `ask` writes. `answer` takes the
/// exported module as it stands; `ask` appends the new question's fragment to
/// it. Nothing else is composed: the supplier is not an author.
fn overlay_text(verb: &str, args: &GyldArgs) -> Result<String, String> {
    let overlay = args
        .overlay
        .as_deref()
        .ok_or("`overlay` (the exported module text) is required")?;
    if overlay.trim().is_empty() {
        return Err("`overlay` is empty".into());
    }
    let question = args.question.as_deref().unwrap_or("");
    let text = match (verb, args.question.as_deref()) {
        ("answer", Some(_)) => {
            return Err("`question` belongs to `ask`, not `answer`".into());
        }
        ("ask", None) => {
            return Err("`ask` needs the added question's module fragment in `question`".into());
        }
        ("ask", Some(_)) => {
            if question.trim().is_empty() {
                return Err("`question` is empty".into());
            }
            format!("{}\n\n{}\n", overlay.trim_end(), question.trim_end())
        }
        _ => format!("{}\n", overlay.trim_end()),
    };
    if text.len() > MAX_OVERLAY_BYTES {
        return Err(format!(
            "overlay is {} bytes; the limit is {MAX_OVERLAY_BYTES}",
            text.len()
        ));
    }
    Ok(text)
}

/// May this plan write at `path`? Under the bundle root, always; under the
/// decisions root, when the app configured one.
///
/// Two roots and no more. The check stays [`contained`] — LEXICAL, so `plan`
/// remains pure and answers the same way for a notebook that does not exist
/// yet, which the first ruling in a stream always is. What the filesystem has
/// to say about a symlinked directory is the WRITER's question, asked at the
/// moment of the write.
fn writable(layout: &Layout, path: &std::path::Path) -> bool {
    if contained(&layout.bundle_root, path) {
        return true;
    }
    match layout.decisions_root.as_ref() {
        Some(root) => contained(root, path),
        None => false,
    }
}

/// The roots a written path may be under, as a refusal says them.
fn homes(layout: &Layout) -> String {
    let bundle = format!("the bundle root {}", layout.bundle_root.display());
    match layout.decisions_root.as_ref() {
        Some(root) => format!("{bundle} or the decisions root {}", root.display()),
        None => bundle,
    }
}

/// A named stream id, validated. A missing or malformed id is a refusal, never
/// a guess.
fn require_stream<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str, String> {
    let id = value.ok_or_else(|| format!("`{field}` is required"))?;
    if !valid_stream_id(id) {
        return Err(format!(
            "`{field}` value {id:?} is not a stream id (lower case words joined by hyphens)"
        ));
    }
    Ok(id)
}

/// Collapse a free-text argument to a single bounded line. A note and a build
/// stamp are the only free text that reaches an argv, and a newline in one of
/// them would confuse a log far more than it would help anybody.
pub fn one_line(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    out.truncate(240);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn layout() -> Layout {
        Layout::new(PathBuf::from("/g"), PathBuf::from("/b"))
    }

    fn request(json: &str) -> GyldRequest {
        GyldRequest::parse(json.as_bytes()).expect("envelope")
    }

    /// An agent ready to consult about the streams this bundle lists.
    fn agent() -> AgentState {
        AgentState::ready(&["base", "stream-a", "keys-a"])
    }

    fn planned(json: &str) -> Plan {
        plan(
            &layout(),
            &request(json),
            Some(Path::new("/b/builds/build-0")),
            "build-1",
            &agent(),
        )
        .expect("plan")
    }

    fn refused(json: &str) -> String {
        plan(
            &layout(),
            &request(json),
            Some(Path::new("/b/builds/build-0")),
            "build-1",
            &agent(),
        )
        .expect_err("refusal")
    }

    /// An `explain` request carrying the fixture envelope.
    fn explain_request() -> GyldRequest {
        let body = serde_json::json!({
            "verb": "explain",
            "stream_output": true,
            "args": { "context": crate::ask::tests::envelope() },
        });
        request(&body.to_string())
    }

    #[test]
    fn the_allow_list_is_the_eight_named_verbs() {
        for v in ALLOWED_VERBS {
            assert!(verb_allowed(v), "{v}");
        }
        for v in [
            "occurred", "lens", "inspect", "capture", "emit", "", "rm", "fork ", "consult",
            "advise", "explain ",
        ] {
            assert!(!verb_allowed(v), "{v} must be refused");
        }
        assert!(
            verb_allowed("explain") && verb_allowed("ask"),
            "`explain` and `ask` are two verbs, not two spellings of one"
        );
    }

    #[test]
    fn stream_ids_follow_gylds_own_pattern() {
        for id in ["base", "keys-2026-09-13", "stream-a", "a1"] {
            assert!(valid_stream_id(id), "{id}");
        }
        for id in [
            "",
            "-a",
            "a-",
            "A",
            "a--b",
            "a/b",
            "a b",
            "../etc",
            "a.b",
            "1a",
            &"a".repeat(65),
        ] {
            assert!(!valid_stream_id(id), "{id} must be refused");
        }
    }

    #[test]
    fn overlay_names_match_the_shipped_fixtures() {
        assert_eq!(overlay_module("stream-a"), "glade_decisions_stream_a");
        assert_eq!(overlay_file("stream-a"), "glade-decisions-stream-a.gyld.py");
        assert_eq!(
            overlay_file("keys-2026-09-13"),
            "glade-decisions-keys-2026-09-13.gyld.py"
        );
    }

    #[test]
    fn list_reads_the_latest_bundle_and_runs_nothing() {
        let p = planned(r#"{"verb":"list"}"#);
        assert!(p.argv.is_empty() && p.output_dir.is_none());
        assert_eq!(
            p.read.as_deref(),
            Some(Path::new("/b/builds/build-0/streams.json"))
        );
    }

    #[test]
    fn fork_and_link_map_to_one_manager_invocation_each() {
        let f =
            planned(r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a","note":"why"}}"#);
        assert_eq!(
            f.argv,
            vec![
                "/g/scripts/manage_decision_streams.py",
                "--repository",
                "/b/stage",
                "fork",
                "base",
                "keys-a",
                "--note",
                "why",
            ]
        );
        assert!(f.write.is_none() && f.output_dir.is_none());
        assert_eq!(f.cwd, PathBuf::from("/g"));
        assert_eq!(f.pythonpath, "/g/src:/g");

        let l =
            planned(r#"{"verb":"link","args":{"parent":"base","stream":"keys-a","force":true}}"#);
        assert_eq!(l.argv[3], "link");
        assert_eq!(l.argv.last().unwrap(), "--force");
    }

    #[test]
    fn answer_writes_the_overlay_then_rebuilds_into_a_new_directory() {
        let p = planned(r#"{"verb":"answer","args":{"stream":"keys-a","overlay":"module text"}}"#);
        let w = p.write.expect("a planned write");
        assert_eq!(
            w.path,
            PathBuf::from("/b/overlays/glade-decisions-keys-a.gyld.py")
        );
        assert_eq!(w.text, "module text\n");
        assert_eq!(
            p.argv,
            vec![
                "/g/scripts/manage_decision_streams.py",
                "--repository",
                "/b/stage",
                "rebuild",
                "--bundle",
                "/b/builds/build-0",
                "--output",
                "/b/builds/build-1",
            ]
        );
        assert_eq!(p.output_dir, Some(PathBuf::from("/b/builds/build-1")));
    }

    /// The same layout with the owner's decisions folder configured.
    fn with_decisions() -> Layout {
        layout().with_decisions_root(Some(PathBuf::from("/glade-wz/decisions")))
    }

    fn planned_in(layout: &Layout, json: &str) -> Plan {
        plan(
            layout,
            &request(json),
            Some(Path::new("/b/builds/build-0")),
            "build-1",
            &agent(),
        )
        .expect("plan")
    }

    #[test]
    fn a_configured_decisions_root_is_where_every_written_notebook_is_planned() {
        let owned = with_decisions();
        let answer = planned_in(
            &owned,
            r#"{"verb":"answer","args":{"stream":"keys-a","overlay":"text"}}"#,
        );
        let notebook = PathBuf::from("/glade-wz/decisions/glade-decisions-keys-a.gyld.py");
        assert_eq!(answer.write.as_ref().expect("a write").path, notebook);
        assert_eq!(
            answer.overlay.as_ref(),
            Some(&notebook),
            "the answer names the file the ruling was left in"
        );
        // The build is still the bundle root's own business.
        assert_eq!(answer.output_dir, Some(PathBuf::from("/b/builds/build-1")));

        // fork and link write no file here — the host does — but they name the
        // notebook the adoption leaves behind all the same.
        let fork = planned_in(
            &owned,
            r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a"}}"#,
        );
        assert!(fork.write.is_none());
        assert_eq!(fork.overlay.as_ref(), Some(&notebook));
        // And nothing of the argv moved: the host still writes into the stage.
        assert_eq!(fork.argv[2], "/b/stage");

        // A verb that leaves no notebook names none.
        for json in [r#"{"verb":"list"}"#, r#"{"verb":"rebuild"}"#] {
            assert_eq!(planned_in(&owned, json).overlay, None, "{json}");
        }

        // With no decisions root the same two verbs plan into the staging tree.
        let staged = PathBuf::from("/b/overlays/glade-decisions-keys-a.gyld.py");
        let answer = planned(r#"{"verb":"answer","args":{"stream":"keys-a","overlay":"text"}}"#);
        assert_eq!(answer.write.expect("a write").path, staged);
        assert_eq!(answer.overlay, Some(staged.clone()));
        let fork = planned(r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a"}}"#);
        assert_eq!(fork.overlay, Some(staged));
    }

    #[test]
    fn a_written_path_may_leave_the_bundle_root_only_for_the_decisions_root() {
        let plain = layout();
        let owned = with_decisions();
        let notebook = PathBuf::from("/glade-wz/decisions/glade-decisions-a.gyld.py");

        assert!(writable(&plain, &PathBuf::from("/b/overlays/x.gyld.py")));
        assert!(writable(&owned, &PathBuf::from("/b/overlays/x.gyld.py")));
        assert!(
            writable(&owned, &notebook),
            "the decisions root is the second home, and only when configured"
        );
        assert!(
            !writable(&plain, &notebook),
            "a decisions path is nowhere at all when no decisions root was given"
        );
        for elsewhere in [
            "/etc/passwd",
            "/glade-wz/decisions/../../etc/passwd",
            "/bb/x",
        ] {
            assert!(!writable(&owned, &PathBuf::from(elsewhere)), "{elsewhere}");
        }

        // And the refusal says which homes it means, both ways.
        assert_eq!(homes(&plain), "the bundle root /b");
        assert_eq!(
            homes(&owned),
            "the bundle root /b or the decisions root /glade-wz/decisions"
        );
    }

    #[test]
    fn ask_appends_the_question_to_the_exported_module() {
        let p = planned(
            r#"{"verb":"ask","args":{"stream":"keys-a","overlay":"module","question":"class Q: pass"}}"#,
        );
        assert_eq!(p.write.unwrap().text, "module\n\nclass Q: pass\n");
    }

    #[test]
    fn diff_names_two_streams_of_the_current_bundle() {
        let p = planned(r#"{"verb":"diff","args":{"left":"base","right":"stream-a"}}"#);
        assert_eq!(
            p.argv,
            vec![
                "/g/scripts/manage_decision_streams.py",
                "--repository",
                "/b/stage",
                "diff",
                "base",
                "stream-a",
                "--bundle",
                "/b/builds/build-0",
            ]
        );
        assert!(p.output_dir.is_none());
    }

    #[test]
    fn rebuild_carries_an_explicit_build_stamp_when_one_is_given() {
        let p = planned(r#"{"verb":"rebuild","args":{"built":"2026-09-13T00:00:00Z"}}"#);
        assert_eq!(p.argv[p.argv.len() - 2], "--built");
        assert_eq!(p.argv[p.argv.len() - 1], "2026-09-13T00:00:00Z");
    }

    #[test]
    fn refusals_are_readable_and_never_reach_an_argv() {
        assert!(refused(r#"{"verb":"forall"}"#).contains("not in the allow-list"));
        assert!(refused(r#"{"verb":""}"#).contains("missing `verb`"));
        assert!(
            refused(r#"{"verb":"fork","args":{"parent":"base"}}"#).contains("`stream` is required")
        );
        assert!(
            refused(r#"{"verb":"fork","args":{"parent":"base","stream":"../etc"}}"#)
                .contains("not a stream id")
        );
        assert!(
            refused(r#"{"verb":"fork","args":{"parent":"base","stream":"base"}}"#)
                .contains("its own parent")
        );
        assert!(refused(r#"{"verb":"answer","args":{"stream":"a"}}"#).contains("`overlay`"));
        assert!(
            refused(r#"{"verb":"answer","args":{"stream":"a","overlay":"x","question":"q"}}"#)
                .contains("belongs to `ask`")
        );
        assert!(
            refused(r#"{"verb":"ask","args":{"stream":"a","overlay":"x"}}"#)
                .contains("needs the added question")
        );
        assert!(
            refused(r#"{"verb":"diff","args":{"left":"a","right":"a"}}"#).contains("two different")
        );
    }

    #[test]
    fn an_oversize_overlay_is_refused_as_data() {
        let big = "x".repeat(MAX_OVERLAY_BYTES + 1);
        let body =
            serde_json::json!({ "verb": "answer", "args": { "stream": "a", "overlay": big } });
        let e = plan(
            &layout(),
            &request(&body.to_string()),
            Some(Path::new("/b/builds/b0")),
            "b1",
            &agent(),
        )
        .expect_err("refusal");
        assert!(e.contains("the limit is"), "{e}");
    }

    #[test]
    fn a_bundle_outside_the_root_is_refused() {
        let e = plan(
            &layout(),
            &request(r#"{"verb":"list"}"#),
            Some(Path::new("/elsewhere")),
            "b1",
            &agent(),
        )
        .expect_err("refusal");
        assert!(e.contains("leaves the bundle root"), "{e}");
    }

    #[test]
    fn verbs_that_need_a_bundle_refuse_before_one_exists() {
        for verb in ["list", "rebuild"] {
            let body = format!("{{\"verb\":\"{verb}\"}}");
            let e = plan(&layout(), &request(&body), None, "b1", &agent()).expect_err("refusal");
            assert!(e.contains("no bundle"), "{verb}: {e}");
        }
        // fork and link do not need one: they generate an overlay module.
        let p = plan(
            &layout(),
            &request(r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a"}}"#),
            None,
            "b1",
            &agent(),
        );
        assert!(p.is_ok(), "{p:?}");
    }

    #[test]
    fn discovery_asks_the_checkout_rather_than_reimplementing_it() {
        let p = discover_plan(&layout());
        assert_eq!(p.argv[0], "-c");
        assert_eq!(
            p.argv[2], "/b/stage",
            "the staging repository is the operand"
        );
        assert!(
            p.argv[1].contains("from scripts.capture_decision_stream import discover"),
            "the checkout's own discover answers: {}",
            p.argv[1]
        );
        assert!(p.output_dir.is_none() && p.write.is_none() && p.read.is_none());
        assert_eq!(p.cwd, PathBuf::from("/g"));
        assert_eq!(p.pythonpath, "/g/src:/g");
    }

    #[test]
    fn declared_streams_reads_the_last_json_line_and_drops_a_bad_id() {
        assert_eq!(
            declared_streams("[\"fork-a\", \"stream-a\", \"stream-b\"]\n"),
            vec!["fork-a", "stream-a", "stream-b"]
        );
        // A host that says something on the way still answers.
        assert_eq!(
            declared_streams("warning: something\n[\"stream-a\"]\n"),
            vec!["stream-a"]
        );
        assert_eq!(declared_streams("[]\n"), Vec::<String>::new());
        assert_eq!(declared_streams("not json at all\n"), Vec::<String>::new());
        assert_eq!(declared_streams(""), Vec::<String>::new());
        // An id that is not an id never reaches an argv, whoever said it.
        assert_eq!(
            declared_streams("[\"stream-a\", \"../etc\", \"--force\"]\n"),
            vec!["stream-a"]
        );
    }

    #[test]
    fn the_first_build_runs_the_writer_host_over_every_declared_stream() {
        let declared = vec!["fork-a".to_string(), "stream-a".to_string()];
        let p = first_build_plan(&layout(), "build-1", &declared);
        assert_eq!(
            p.argv,
            vec![
                "/g/scripts/emit_decision_streams.py",
                "--repository",
                "/b/stage",
                "--output",
                "/b/builds/build-1",
                "--architecture",
                "--stream",
                "fork-a",
                "--stream",
                "stream-a",
            ],
            "the first build has no bundle to re-capture, so it runs the writer"
        );
        assert_eq!(p.output_dir, Some(PathBuf::from("/b/builds/build-1")));
        assert!(p.write.is_none() && p.read.is_none());

        // A checkout that declares nothing but the base still builds.
        let bare = first_build_plan(&layout(), "build-1", &[]);
        assert_eq!(bare.argv.last().unwrap(), "--architecture");
    }

    #[test]
    fn verbs_that_need_a_bundle_all_name_the_same_refusal() {
        for verb in ["list", "rebuild"] {
            let body = format!("{{\"verb\":\"{verb}\"}}");
            let e = plan(&layout(), &request(&body), None, "b1", &agent()).expect_err("refusal");
            assert_eq!(e, NO_BUNDLE, "{verb}");
        }
    }

    #[test]
    fn explain_consults_and_carries_no_write_and_no_argv() {
        let p = plan(
            &layout(),
            &explain_request(),
            Some(Path::new("/b/builds/build-0")),
            "build-1",
            &agent(),
        )
        .expect("a consult plan");
        assert!(
            p.argv.is_empty(),
            "the agent never reaches a command line: {:?}",
            p.argv
        );
        assert!(
            p.write.is_none(),
            "the agent never writes an overlay: {:?}",
            p.write
        );
        assert!(p.output_dir.is_none() && p.read.is_none(), "{p:?}");

        let consult = p.consult.expect("a consultation");
        assert_eq!(
            consult.sources,
            PathBuf::from("/b/builds/build-0/sources.json"),
            "the index is the one the current build emitted"
        );
        assert_eq!(consult.conversation, "conv-tab1-key_custody-1789");
        assert_eq!(consult.context.stream, "base");
        assert_eq!(consult.context.tags(), vec!["Q11", "AZ-7"]);
    }

    #[test]
    fn each_explain_refusal_is_produced_with_no_filesystem_effect() {
        let bundle = Path::new("/b/builds/build-0");
        let explain = explain_request();
        let refuse = |agent: &AgentState| -> String {
            plan(&layout(), &explain, Some(bundle), "build-1", agent).expect_err("refusal")
        };

        // No model key — the whole world is otherwise ready.
        let e = refuse(&AgentState {
            key: false,
            key_file: PathBuf::from("/b/agent/api-key"),
            ..agent()
        });
        assert!(
            e.contains("no model key") && e.contains("ANTHROPIC_API_KEY"),
            "{e}"
        );

        // No source index: grounding was ruled in from day one.
        let e = refuse(&AgentState {
            index: false,
            ..agent()
        });
        assert!(
            e.contains("sources.json") && e.contains("--sources-root"),
            "{e}"
        );

        // A stream this build does not list.
        let e = refuse(&AgentState::ready(&["stream-a"]));
        assert!(e.contains("this build lists"), "{e}");

        // An envelope that did not decode.
        let bad = request(r#"{"verb":"explain","args":{"stream":"base"}}"#);
        let e = plan(&layout(), &bad, Some(bundle), "build-1", &agent()).expect_err("refusal");
        assert!(e.contains("`context`"), "{e}");

        // And before any of them: a bundle root with no build at all.
        let e = plan(&layout(), &explain, None, "build-1", &agent()).expect_err("refusal");
        assert_eq!(e, NO_BUNDLE);

        // Nothing above touched the filesystem: `/b` is not a directory here.
        assert!(!Path::new("/b").exists(), "the planner laid nothing down");
    }

    #[test]
    fn an_index_outside_the_bundle_root_is_refused_like_any_other_path() {
        let e = plan(
            &layout(),
            &explain_request(),
            Some(Path::new("/elsewhere/builds/build-0")),
            "build-1",
            &agent(),
        )
        .expect_err("refusal");
        assert!(e.contains("leaves the bundle root"), "{e}");
    }

    #[test]
    fn control_characters_never_survive_into_an_argv() {
        let p = planned(
            r#"{"verb":"fork","args":{"parent":"base","stream":"keys-a","note":"one\ntwo three"}}"#,
        );
        let note = p.argv.last().unwrap();
        assert_eq!(note, "one two three");
    }
}
