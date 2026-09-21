//! What a WRITING verb did to the stream it wrote, and how to put it back.
//!
//! A writing verb that makes things WORSE is refused and leaves no trace. The
//! defect this module exists for: an `answer` whose notebook text Gyld then
//! rejects used to leave the broken file on disk and abandon the whole rebuild,
//! so no stream advanced again until the owner repaired the file by hand — and
//! the desk said "accepted" the whole time, because the accept is answered
//! before the host has run.
//!
//! Three pieces, all of them readable on their own:
//!
//! * [`Snapshot`] — what the notebook's name held BEFORE the write, taken before
//!   anything is written, and put back byte for byte if the write is refused.
//! * [`classify`] — what the run did to the stream, read off the exit code and
//!   Gyld's own `validation.json`. Gyld reports a rejection two ways and this
//!   knows both.
//! * [`said`] — the one line a refusal leaves on stderr, so a failure that used
//!   to be silent is never silent again.

use std::path::{Path, PathBuf};

use crate::bundle::{self, Layout};
use crate::envelope::Refusal;
use crate::exec::RunOutput;
use crate::verbs::{self, Plan};

/// The code a refusal carries when the run failed and Gyld left no
/// `validation.json` to say why: a spawn failure, a timeout, a traceback, a host
/// that exited non-zero with nothing but a line on stderr. Deliberately not one
/// of Gyld's codes — the supplier does not invent a Gyld finding it did not read.
pub const RUN_FAILED: &str = "RUN_FAILED";

/// What one name in the tree HELD, so it can hold exactly that again.
///
/// A link and a real file are different things at the same name and a restore
/// must tell them apart: `overlays/` is seeded with one symlink per file of the
/// read-only Gyld checkout, and putting a link back as a regular file holding the
/// bytes it led to would quietly end copy-on-write for that sample.
#[derive(Debug, Clone, PartialEq)]
pub enum Held {
    /// A symlink, and where it led.
    Link(PathBuf),
    /// A regular file, and its bytes.
    File(Vec<u8>),
    /// Nothing of that name.
    Absent,
}

impl Held {
    /// What is at `path` NOW. A read that fails is an error rather than "absent":
    /// a snapshot that cannot be taken must refuse the verb, because the
    /// alternative is a restore that DELETES a notebook it could not read.
    fn of(path: &Path) -> Result<Held, String> {
        let meta = match path.symlink_metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Held::Absent);
            }
            Err(e) => {
                return Err(format!("cannot read {}: {e}", path.display()));
            }
        };
        if meta.file_type().is_symlink() {
            let led = std::fs::read_link(path)
                .map_err(|e| format!("cannot read the link {}: {e}", path.display()))?;
            return Ok(Held::Link(led));
        }
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Ok(Held::File(bytes))
    }

    /// Put it back at `path`, replacing whatever is there now. The same no-follow
    /// atomic sequence the write itself used ([`bundle::replace`],
    /// [`bundle::relink`]), and removal does not follow a link either.
    fn put(&self, path: &Path) -> Result<(), String> {
        match self {
            Held::Link(led) => bundle::relink(led, path)
                .map_err(|e| format!("cannot point {} at {}: {e}", path.display(), led.display())),
            Held::File(bytes) => bundle::replace(path, bytes)
                .map_err(|e| format!("cannot put {} back: {e}", path.display())),
            Held::Absent => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
            },
        }
    }
}

/// The state one writing verb is put back to if Gyld rejects what it wrote.
///
/// Taken BEFORE anything is written, which is the only moment it can be taken:
/// the write is a replace and the bytes it replaced are gone afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The notebook in its home — the decisions root when one is configured,
    /// else `overlays/` — and what that name held.
    pub notebook: (PathBuf, Held),
    /// `overlays/<file>` and what IT held, when that is a different name from
    /// the notebook. It is a different name exactly when a decisions root is
    /// configured, and then it is normally a link: to the notebook, or to the
    /// checkout's shipped sample of that name.
    pub staged: Option<(PathBuf, Held)>,
}

impl Snapshot {
    /// The previous state of what `plan` is about to write, or `None` for a verb
    /// that leaves no notebook. A read failure REFUSES the verb before a byte is
    /// written.
    pub fn take(layout: &Layout, plan: &Plan) -> Result<Option<Snapshot>, String> {
        let notebook = match plan.overlay.as_ref() {
            Some(p) => p.clone(),
            None => {
                return Ok(None);
            }
        };
        let held = Held::of(&notebook)?;
        let staged = match notebook.file_name() {
            Some(name) => {
                let staged = layout.overlays().join(name);
                match staged == notebook {
                    true => None,
                    false => Some((staged.clone(), Held::of(&staged)?)),
                }
            }
            None => None,
        };
        Ok(Some(Snapshot {
            notebook: (notebook, held),
            staged,
        }))
    }

    /// Put both names back exactly as they were. The notebook first, so the
    /// staging link is never left pointing at a file that is about to change.
    ///
    /// `stage/examples` needs nothing: it is a directory LINK to `overlays/`, so
    /// the staging repository the hosts read follows this by itself.
    pub fn restore(&self) -> Result<(), String> {
        let (path, held) = &self.notebook;
        held.put(path)?;
        match self.staged.as_ref() {
            Some((path, held)) => held.put(path),
            None => Ok(()),
        }
    }
}

/// What the run did to the stream the verb wrote.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The write stands: the run landed and the stream is no worse than it was.
    Accepted,
    /// The write made things worse. `document` is Gyld's own `validation.json`,
    /// verbatim, when Gyld wrote one.
    Refused {
        refusal: Refusal,
        document: Option<serde_json::Value>,
    },
}

/// What became of the notebook, for the line that says so.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Put {
    /// It is back as it was.
    Restored,
    /// There was nothing to put back — a `rebuild`, which writes no notebook.
    Nothing,
    /// Putting it back FAILED. The reason goes on its own line; this one never
    /// reads as a restore that worked.
    Failed,
}

/// Did this run make the stream it wrote WORSE? Gyld says so two ways and this
/// reads both (README, "A refused write").
///
/// 1. **Structurally.** The capture raised, `emit_decision_streams.py` wrote
///    `streams/<name>/validation.json` and exited 1, and no `streams.json` was
///    written at all. The whole build is abandoned, so the reason is the first
///    failing document the half-written directory holds — which may name a CHILD
///    stream rather than the one that was written, and is reported as it is. A
///    run that failed without leaving a document at all is reported by the host's
///    last word on stderr.
/// 2. **As findings.** The build SUCCEEDS, complete and exit 0, and the written
///    stream's own document is `ok:false` with a code, a message and details —
///    the owner's-mistake class (an answer that is not among the offered ones, a
///    second ruling on one question, a ruling whose prerequisites are open).
///
/// A stream that was ALREADY `ok:false` before the write is not refused: the
/// owner may be part-way through repairing a notebook, and putting his text back
/// would undo the repair he is making.
///
/// `previous` is the latest build as it stood BEFORE this run, which is what
/// "already" is measured against.
pub fn classify(plan: &Plan, ran: Result<&RunOutput, &str>, previous: Option<&Path>) -> Outcome {
    let stream = plan.stream.clone().unwrap_or_default();
    let failed = match ran {
        Err(e) => Some(verbs::one_line(e)),
        Ok(out) if out.exit != 0 => Some(said_on_stderr(out)),
        Ok(_) => None,
    };
    if let Some(message) = failed {
        let found = plan.output_dir.as_deref().and_then(first_failing);
        return match found {
            Some(document) => refused(document),
            None => Outcome::Refused {
                refusal: Refusal {
                    stream,
                    code: RUN_FAILED.into(),
                    message,
                    details: None,
                    restored: false,
                },
                document: None,
            },
        };
    }
    // Exit 0. `fork` and `link` build nothing, so the exit was the whole rule;
    // a `rebuild` wrote no stream of its own, so it has none to have spoiled.
    let built = match plan.output_dir.as_deref() {
        Some(dir) => dir,
        None => {
            return Outcome::Accepted;
        }
    };
    if stream.is_empty() {
        return Outcome::Accepted;
    }
    let now = match validation(built, &stream) {
        Some(document) if !document.ok => document,
        _ => {
            return Outcome::Accepted;
        }
    };
    let before = previous.map(|dir| validation(dir, &stream));
    match before {
        Some(Some(document)) if !document.ok => Outcome::Accepted,
        _ => refused(now),
    }
}

/// The one line a failed or refused build leaves on stderr, so a rejection that
/// used to be silent is never silent again. Bounded to ONE line, whatever a host
/// printed into the message.
pub fn said(verb: &str, refusal: &Refusal, put: Put) -> String {
    let on = match refusal.stream.is_empty() {
        true => String::new(),
        false => format!(" on {}", refusal.stream),
    };
    let fate = match put {
        Put::Restored => "notebook restored",
        Put::Nothing => "nothing to restore",
        Put::Failed => "the notebook could NOT be put back",
    };
    verbs::one_line(&format!(
        "glade-gyld: refused {verb}{on}: {}: {} — {fate}",
        refusal.code, refusal.message
    ))
}

/// A refusal built out of Gyld's own document, code, message, details and all.
fn refused(document: Validation) -> Outcome {
    Outcome::Refused {
        refusal: Refusal {
            stream: document.stream,
            code: document.code,
            message: document.message,
            details: document.details,
            restored: false,
        },
        document: Some(document.document),
    }
}

/// The host's last word: its last non-empty stderr line, bounded to one line. A
/// host that said nothing at all is reported by its exit code, because "refused:
/// " with nothing after it says less than the number does.
fn said_on_stderr(out: &RunOutput) -> String {
    let last = out
        .stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty());
    match last {
        Some(line) => verbs::one_line(line),
        None => format!("the host exited {} and said nothing on stderr", out.exit),
    }
}

/// One `validation.json`, as read: the three fields a refusal repeats out of it,
/// and the whole document to pass on verbatim.
#[derive(Debug, Clone, PartialEq)]
struct Validation {
    document: serde_json::Value,
    stream: String,
    ok: bool,
    code: String,
    message: String,
    details: Option<serde_json::Value>,
}

/// Read `<build>/streams/<stream>/validation.json`. A document that is absent or
/// does not decode is `None` — not a refusal of its own, because a build that
/// emitted no document for a stream said nothing about it.
fn validation(build: &Path, stream: &str) -> Option<Validation> {
    let path = build.join("streams").join(stream).join("validation.json");
    read_validation(&path, stream)
}

fn read_validation(path: &Path, stream: &str) -> Option<Validation> {
    let text = std::fs::read_to_string(path).ok()?;
    let document: serde_json::Value = serde_json::from_str(&text).ok()?;
    let said = |key: &str| -> Option<String> {
        document
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let code = said("code").unwrap_or_else(|| RUN_FAILED.to_string());
    // A document with a code and no message says what it can: the code. An empty
    // reason reads as a desk with nothing to say, which is the state this whole
    // module exists to end.
    let message = verbs::one_line(&said("message").unwrap_or_default());
    Some(Validation {
        stream: said("stream").unwrap_or_else(|| stream.to_string()),
        ok: document
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        message: match message.is_empty() {
            true => code.clone(),
            false => message,
        },
        code,
        details: document.get("details").cloned(),
        document,
    })
}

/// The first `ok:false` document a failed output directory holds, in sorted
/// order so two runs of one failure report the same stream.
fn first_failing(build: &Path) -> Option<Validation> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(build.join("streams"))
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for dir in names.iter() {
        let found = read_validation(
            &dir.join("validation.json"),
            &dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );
        match found {
            Some(document) if !document.ok => {
                return Some(document);
            }
            _ => {
                continue;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verbs::PlannedWrite;

    fn tmp(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("glade-gyld-outcome-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A writing plan: the notebook in its home, the build it would make.
    fn writing(layout: &Layout, stream: &str, build: Option<PathBuf>) -> Plan {
        let notebook = layout.overlay_home().join(verbs::overlay_file(stream));
        Plan {
            verb: "answer".into(),
            write: Some(PlannedWrite {
                path: notebook.clone(),
                text: "the ruling\n".into(),
                force: true,
            }),
            argv: Vec::new(),
            cwd: layout.gyld_root.clone(),
            pythonpath: layout.pythonpath(),
            output_dir: build,
            read: None,
            consult: None,
            overlay: Some(notebook),
            stream: Some(stream.into()),
        }
    }

    /// A `validation.json` for `stream` in `build`, valid or not.
    fn validated(build: &Path, stream: &str, ok: bool, code: &str) {
        let dir = build.join("streams").join(stream);
        std::fs::create_dir_all(&dir).unwrap();
        let body = if ok {
            serde_json::json!({
                "format": "gyld.validation.v1", "stream": stream, "built": "b",
                "ok": true, "findings": [],
            })
        } else {
            serde_json::json!({
                "format": "gyld.validation.v1", "stream": stream, "built": "b",
                "ok": false, "code": code, "message": format!("{stream} is not well formed"),
                "details": { "ruling": "R" }, "findings": [],
            })
        };
        std::fs::write(dir.join("validation.json"), format!("{body}\n")).unwrap();
    }

    fn ran(exit: i32, stderr: &str) -> RunOutput {
        RunOutput {
            exit,
            stdout: String::new(),
            stderr: stderr.into(),
            truncated: false,
        }
    }

    fn refusal(outcome: &Outcome) -> &Refusal {
        match outcome {
            Outcome::Refused { refusal, .. } => refusal,
            Outcome::Accepted => panic!("the write was accepted"),
        }
    }

    #[test]
    fn a_notebook_that_was_there_is_put_back_byte_for_byte() {
        let dir = tmp("was-there");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let plan = writing(&layout, "stream-a", None);
        let notebook = plan.overlay.clone().unwrap();
        std::fs::create_dir_all(notebook.parent().unwrap()).unwrap();
        std::fs::write(&notebook, "the ruling as it was\n").unwrap();

        let snapshot = Snapshot::take(&layout, &plan)
            .expect("a snapshot is taken")
            .expect("a writing verb has one");
        std::fs::write(&notebook, "the broken ruling\n").unwrap();
        snapshot.restore().expect("the notebook goes back");

        assert_eq!(
            std::fs::read_to_string(&notebook).unwrap(),
            "the ruling as it was\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_notebook_that_was_absent_is_absent_again() {
        let dir = tmp("was-absent");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let plan = writing(&layout, "stream-a", None);
        let notebook = plan.overlay.clone().unwrap();

        let snapshot = Snapshot::take(&layout, &plan)
            .expect("a snapshot is taken")
            .expect("a writing verb has one");
        std::fs::create_dir_all(notebook.parent().unwrap()).unwrap();
        std::fs::write(&notebook, "the first ruling in this stream\n").unwrap();
        snapshot.restore().expect("the notebook goes away again");

        assert!(
            notebook.symlink_metadata().is_err(),
            "a first ruling that is refused leaves no file at all"
        );
        // Restoring an absent notebook twice is not an error: a second restore
        // has nothing left to remove and must not say the desk is broken.
        snapshot.restore().expect("idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The staging tree is put back EXACTLY: a seed link into the checkout is a
    /// link again, and not a real file holding the bytes it led to.
    #[test]
    fn a_seed_link_at_the_notebooks_name_is_a_link_again() {
        let dir = tmp("seed-link");
        let examples = dir.join("gyld/examples");
        std::fs::create_dir_all(&examples).unwrap();
        let shipped = examples.join(verbs::overlay_file("stream-a"));
        std::fs::write(&shipped, "the shipped sample\n").unwrap();

        // No decisions root: the notebook's home IS the staging tree, and the
        // name holds the seed link.
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        bundle::ensure_stage(&layout).unwrap();
        let plan = writing(&layout, "stream-a", None);
        let notebook = plan.overlay.clone().unwrap();
        assert!(notebook
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());

        let snapshot = Snapshot::take(&layout, &plan)
            .expect("a snapshot is taken")
            .expect("a writing verb has one");
        bundle::replace(&notebook, b"the broken ruling\n").unwrap();
        snapshot.restore().expect("the link goes back");

        assert_eq!(
            std::fs::read_link(&notebook).unwrap(),
            shipped,
            "the seed link is a link again, not a copy of what it led to"
        );
        assert_eq!(
            std::fs::read_to_string(&shipped).unwrap(),
            "the shipped sample\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a decisions root there are TWO names to put back: the notebook in
    /// the owner's folder and the link to it in the staging tree.
    #[test]
    fn a_decisions_root_puts_the_notebook_and_the_staging_link_back() {
        let dir = tmp("decisions-root");
        let examples = dir.join("gyld/examples");
        std::fs::create_dir_all(&examples).unwrap();
        let shipped = examples.join(verbs::overlay_file("stream-a"));
        std::fs::write(&shipped, "the shipped sample\n").unwrap();
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"))
            .with_decisions_root(Some(dir.join("decisions")));
        bundle::ensure_stage(&layout).unwrap();

        let plan = writing(&layout, "stream-a", None);
        let notebook = plan.overlay.clone().unwrap();
        let staged = layout.overlays().join(verbs::overlay_file("stream-a"));
        assert_eq!(std::fs::read_link(&staged).unwrap(), shipped);
        assert!(notebook.symlink_metadata().is_err(), "no notebook yet");

        let snapshot = Snapshot::take(&layout, &plan)
            .expect("a snapshot is taken")
            .expect("a writing verb has one");
        // What a refused `answer` on a shipped sample does: writes the owner's
        // copy and points the stage at it.
        std::fs::create_dir_all(notebook.parent().unwrap()).unwrap();
        bundle::replace(&notebook, b"the broken ruling\n").unwrap();
        bundle::relink(&notebook, &staged).unwrap();

        snapshot.restore().expect("both names go back");
        assert!(
            notebook.symlink_metadata().is_err(),
            "the owner's folder is as it was: no file of that name"
        );
        assert_eq!(
            std::fs::read_link(&staged).unwrap(),
            shipped,
            "the staging tree reads the shipped sample again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real file in the staging tree — the arrangement from before there was a
    /// decisions root — is put back as a real file with its own bytes.
    #[test]
    fn a_real_staged_file_is_put_back_as_a_file() {
        let dir = tmp("staged-file");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"))
            .with_decisions_root(Some(dir.join("decisions")));
        let plan = writing(&layout, "stream-a", None);
        let notebook = plan.overlay.clone().unwrap();
        let staged = layout.overlays().join(verbs::overlay_file("stream-a"));
        std::fs::create_dir_all(layout.overlays()).unwrap();
        std::fs::write(&staged, "the module from before\n").unwrap();

        let snapshot = Snapshot::take(&layout, &plan)
            .expect("a snapshot is taken")
            .expect("a writing verb has one");
        std::fs::create_dir_all(notebook.parent().unwrap()).unwrap();
        bundle::replace(&notebook, b"the broken ruling\n").unwrap();
        bundle::relink(&notebook, &staged).unwrap();
        snapshot.restore().expect("both names go back");

        assert!(!staged.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(&staged).unwrap(),
            "the module from before\n"
        );
        assert!(notebook.symlink_metadata().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_verb_that_writes_no_notebook_has_no_snapshot() {
        let dir = tmp("no-write");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let mut plan = writing(&layout, "stream-a", None);
        plan.write = None;
        plan.overlay = None;
        assert_eq!(Snapshot::take(&layout, &plan).expect("no refusal"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rule B, first bullet: a failed run is refused, and the reason is the
    /// validation document the half-written output directory holds.
    #[test]
    fn a_failed_run_is_refused_by_the_document_the_failed_build_left() {
        let dir = tmp("failed-doc");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let build = layout.new_build_dir("build-1");
        let plan = writing(&layout, "stream-a", Some(build.clone()));
        validated(&build, "stream-a", false, "SELECTS_UNKNOWN");

        let outcome = classify(&plan, Ok(&ran(1, "Traceback\nValueError: no\n")), None);
        let refused = refusal(&outcome);
        assert_eq!(refused.stream, "stream-a");
        assert_eq!(refused.code, "SELECTS_UNKNOWN");
        assert_eq!(refused.message, "stream-a is not well formed");
        assert_eq!(
            refused.details,
            Some(serde_json::json!({ "ruling": "R" })),
            "the document's own details, not a rendering of them"
        );
        assert!(!refused.restored, "the caller says whether it put it back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The document may name a CHILD stream rather than the one that was
    /// written. It is reported as it is: the owner needs the stream Gyld named.
    #[test]
    fn a_document_naming_another_stream_is_reported_as_it_is() {
        let dir = tmp("child-doc");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let build = layout.new_build_dir("build-1");
        let plan = writing(&layout, "stream-a", Some(build.clone()));
        // Sorted order, and only the FAILING one counts.
        validated(&build, "base", true, "");
        validated(&build, "fork-a", false, "REQUIRES_OPEN");

        let refused = refusal(&classify(&plan, Ok(&ran(1, "")), None)).clone();
        assert_eq!(refused.stream, "fork-a");
        assert_eq!(refused.code, "REQUIRES_OPEN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No document at all: the last non-empty stderr line, bounded to one line.
    #[test]
    fn a_failed_run_with_no_document_is_refused_by_its_last_stderr_line() {
        let dir = tmp("failed-stderr");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let plan = writing(&layout, "stream-a", Some(layout.new_build_dir("build-1")));

        let out = ran(1, "warming up\nValueError: Selects[NoSuchAlternative]\n\n");
        let refused = refusal(&classify(&plan, Ok(&out), None)).clone();
        assert_eq!(refused.stream, "stream-a");
        assert_eq!(refused.code, RUN_FAILED);
        assert_eq!(refused.message, "ValueError: Selects[NoSuchAlternative]");
        assert_eq!(refused.details, None);

        // Nothing on stderr either: the exit code is what there is to say.
        let quiet = refusal(&classify(&plan, Ok(&ran(1, "")), None)).clone();
        assert!(quiet.message.contains("exited 1"), "{}", quiet.message);

        // A spawn failure or a timeout is a run ERROR, not an exit code.
        let broken = refusal(&classify(&plan, Err("timed out after 120s"), None)).clone();
        assert_eq!(broken.code, RUN_FAILED);
        assert_eq!(broken.message, "timed out after 120s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rule B, second bullet: the FINDINGS class. The build succeeds and is
    /// complete, and the stream that was just written is invalid in it.
    #[test]
    fn a_stream_that_went_invalid_on_a_clean_exit_is_refused() {
        let dir = tmp("findings");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let before = layout.new_build_dir("build-1");
        let after = layout.new_build_dir("build-2");
        let plan = writing(&layout, "stream-a", Some(after.clone()));
        validated(&before, "stream-a", true, "");
        validated(&after, "stream-a", false, "SELECTION_NOT_OFFERED");

        let refused = refusal(&classify(&plan, Ok(&ran(0, "")), Some(&before))).clone();
        assert_eq!(refused.stream, "stream-a");
        assert_eq!(refused.code, "SELECTION_NOT_OFFERED");

        // A stream the previous build did not carry at all is the same case: a
        // fork's first ruling that is invalid is still a write that made things
        // worse.
        let fresh = writing(&layout, "stream-c", Some(after.clone()));
        validated(&after, "stream-c", false, "RULING_NOT_PLACED");
        let refused = refusal(&classify(&fresh, Ok(&ran(0, "")), Some(&before))).clone();
        assert_eq!(refused.code, "RULING_NOT_PLACED");

        // And with no previous build at all.
        let refused = refusal(&classify(&fresh, Ok(&ran(0, "")), None)).clone();
        assert_eq!(refused.code, "RULING_NOT_PLACED");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The owner may be part-way through REPAIRING a notebook: a stream that was
    /// already invalid before the write is not refused, because putting his text
    /// back would undo the repair he is making.
    #[test]
    fn a_stream_that_was_already_invalid_is_not_refused() {
        let dir = tmp("repairing");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let before = layout.new_build_dir("build-1");
        let after = layout.new_build_dir("build-2");
        let plan = writing(&layout, "stream-a", Some(after.clone()));
        validated(&before, "stream-a", false, "SELECTION_NOT_OFFERED");
        validated(&after, "stream-a", false, "SELECTION_MISSING");

        assert_eq!(
            classify(&plan, Ok(&ran(0, "")), Some(&before)),
            Outcome::Accepted
        );

        // A clean stream is accepted, of course.
        validated(&after, "stream-a", true, "");
        assert_eq!(
            classify(&plan, Ok(&ran(0, "")), Some(&before)),
            Outcome::Accepted
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `fork` and `link` build nothing of their own: the host captures what it
    /// wrote and exits non-zero on an error finding, so the exit IS the rule.
    #[test]
    fn fork_and_link_are_judged_by_the_exit_alone() {
        let dir = tmp("fork");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let mut plan = writing(&layout, "fork-b", None);
        plan.verb = "fork".into();
        plan.write = None;
        plan.output_dir = None;

        assert_eq!(classify(&plan, Ok(&ran(0, "")), None), Outcome::Accepted);
        let refused = refusal(&classify(&plan, Ok(&ran(1, "RULING_CONFLICT\n")), None)).clone();
        assert_eq!(refused.stream, "fork-b");
        assert_eq!(refused.code, RUN_FAILED);
        assert_eq!(refused.message, "RULING_CONFLICT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plain `rebuild` writes no notebook, so a failure of it is a refusal with
    /// no stream to name and nothing to put back.
    #[test]
    fn a_failed_rebuild_is_refused_with_nothing_to_restore() {
        let dir = tmp("rebuild");
        let layout = Layout::new(dir.join("gyld"), dir.join("bundle"));
        let mut plan = writing(&layout, "stream-a", Some(layout.new_build_dir("build-1")));
        plan.verb = "rebuild".into();
        plan.write = None;
        plan.overlay = None;
        plan.stream = None;

        let refused = refusal(&classify(&plan, Ok(&ran(1, "no such bundle\n")), None)).clone();
        assert_eq!(refused.stream, "");
        assert_eq!(
            said("rebuild", &refused, Put::Nothing),
            "glade-gyld: refused rebuild: RUN_FAILED: no such bundle — nothing to restore",
            "no stream to name, so no `on <stream>` clause"
        );
        // A rebuild whose build came out clean is accepted: it wrote no stream,
        // so there is no stream of its own to have made worse.
        assert_eq!(classify(&plan, Ok(&ran(0, "")), None), Outcome::Accepted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_line_a_refusal_leaves_says_the_verb_the_stream_the_code_and_the_fate() {
        let refused = Refusal {
            stream: "stream-a".into(),
            code: "SELECTION_NOT_OFFERED".into(),
            message: "VersionPin does not offer SdaxRs".into(),
            details: None,
            restored: true,
        };
        assert_eq!(
            said("answer", &refused, Put::Restored),
            "glade-gyld: refused answer on stream-a: SELECTION_NOT_OFFERED: \
             VersionPin does not offer SdaxRs — notebook restored"
        );
        let stuck = Refusal {
            restored: false,
            ..refused.clone()
        };
        assert!(
            said("answer", &stuck, Put::Failed).ends_with("— the notebook could NOT be put back"),
            "a restore that failed never reads as a restore that worked"
        );

        // Bounded to ONE line, whatever a host printed.
        let long = Refusal {
            message: format!("line one\nline two{}", "x".repeat(400)),
            ..refused
        };
        let line = said("answer", &long, Put::Restored);
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.len() <= 240, "{} bytes", line.len());
    }
}
