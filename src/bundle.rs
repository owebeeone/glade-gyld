//! The bundle root: the ONE writable tree the supplier owns, and the staging
//! repository the Gyld hosts are pointed at.
//!
//! The Gyld capture hosts read every overlay module from `<repository>/examples`
//! (`capture_decision_stream.py::load_inputs`), and `manage_decision_streams.py`
//! writes a generated overlay to that same directory. The Gyld CHECKOUT's
//! `examples/` is committed source and the supplier must never write into it, so
//! the supplier hands the hosts a STAGING repository whose `examples` is the
//! bundle root's own overlays directory:
//!
//! ```text
//! <bundle-root>/
//!   overlays/           the examples tree the hosts read: one symlink per file
//!                       of <gyld-root>/examples, one symlink per NOTEBOOK in
//!                       the decisions root, and — only where no decisions root
//!                       is configured — the overlay modules themselves. A
//!                       seed is laid only where no name exists, so the owner's
//!                       notebook shadows the checkout's sample.
//!   stage/examples  ->  ../overlays      (`--repository <bundle-root>/stage`)
//!   builds/<stamp>/     one emitted bundle per build; never overwritten.
//!   latest.json         {"output_dir": "builds/<stamp>"} — swapped after a
//!                       successful build (section 4.7: "swaps a symlink or
//!                       index after a successful build").
//!   stage.lock          empty; locked by whichever caller is laying the stage
//!                       ([`ensure_stage`]), so callers take turns.
//! ```
//!
//! `--gyld-root` is therefore used for exactly two things: running the scripts
//! out of `<gyld-root>/scripts`, and seeding the overlays tree with the base
//! example sources. Nothing under it is ever written.
//!
//! That promise used to be one this module could only make on its own behalf.
//! `overlays/` is a tree of LINKS into the checkout, and a writer that opens a
//! target by name follows them: an `answer` on a shipped sample stream rewrote
//! the checkout's own `examples/glade-decisions-stream-a.gyld.py`, and neither
//! [`contained`] (lexical, and the path really was under the bundle root) nor
//! this header noticed. Every write here now goes to a sibling temporary file
//! and is RENAMED over its target, which replaces a link instead of following
//! it: [`relink`], [`adopt`] and the supplier's own `write_overlay`.
//!
//! ## Where a written overlay lives
//!
//! [`Layout::decisions_root`] — the app's `--decisions-root` — is the owner's
//! git-tracked folder for the modules the desk writes ("notebooks"). With one
//! configured, a notebook is written THERE and `overlays/` holds a link to it,
//! so what the desk writes is a file the owner reviews and commits on his own
//! schedule. The supplier never runs git; a notebook simply appears in
//! `git status`, and a notebook deleted through git is gone from the next build
//! ([`ensure_stage`] drops the link that no longer leads anywhere).
//!
//! Without one, nothing changes: a written module lives in `overlays/`, which is
//! under a running instance's data directory.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// The bundle-root layout, resolved once from the configured roots.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub gyld_root: PathBuf,
    pub bundle_root: PathBuf,
    /// Where a written overlay module — a NOTEBOOK — is left, when the app
    /// configured somewhere for them (`--decisions-root`).
    ///
    /// The owner's ruling: a notebook the desk writes is his, and his files live
    /// in a git-tracked folder he commits when he chooses, not in a running
    /// instance's scratch directory. `None` keeps the earlier arrangement, in
    /// which a written module lives in the bundle root's own overlays tree.
    pub decisions_root: Option<PathBuf>,
}

impl Layout {
    pub fn new(gyld_root: PathBuf, bundle_root: PathBuf) -> Layout {
        Layout {
            gyld_root,
            bundle_root,
            decisions_root: None,
        }
    }

    /// The same layout with a configured decisions root.
    pub fn with_decisions_root(mut self, decisions_root: Option<PathBuf>) -> Layout {
        self.decisions_root = decisions_root;
        self
    }

    /// The writable examples tree (also the STAGING home: every module the
    /// capture hosts read is reachable from here, written or seeded).
    pub fn overlays(&self) -> PathBuf {
        self.bundle_root.join("overlays")
    }

    /// The home of WRITTEN overlays: the decisions root when one is configured,
    /// else the overlays tree itself.
    ///
    /// The one answer to "where does a ruling land", asked by the planner (which
    /// plans the write) and by the writer (which refuses a write that leaves it).
    pub fn overlay_home(&self) -> PathBuf {
        match self.decisions_root.as_ref() {
            Some(root) => root.clone(),
            None => self.overlays(),
        }
    }

    /// The staging repository handed to the hosts as `--repository`.
    pub fn stage(&self) -> PathBuf {
        self.bundle_root.join("stage")
    }

    /// The staging repository's `examples` directory (a link to [`Layout::overlays`]).
    pub fn stage_examples(&self) -> PathBuf {
        self.stage().join("examples")
    }

    pub fn builds(&self) -> PathBuf {
        self.bundle_root.join("builds")
    }

    /// Where a REQUEST document a Gyld host has to read off disk is laid down: the
    /// fragment a `merge` is handed, and nothing else so far.
    ///
    /// The bundle root's own directory, never the decisions root: a fragment is a
    /// request on its way to a host, not a file the owner keeps. It is written
    /// immediately before the host runs and removed immediately after, whatever
    /// the host answered.
    pub fn requests(&self) -> PathBuf {
        self.bundle_root.join("requests")
    }

    pub fn latest_pointer(&self) -> PathBuf {
        self.bundle_root.join("latest.json")
    }

    /// The file [`ensure_stage`] holds an exclusive lock on while it lays the
    /// stage: one caller at a time per bundle root, whichever process it is in.
    /// Beside the trees rather than in them, so no host ever reads it.
    pub fn stage_lock(&self) -> PathBuf {
        self.bundle_root.join("stage.lock")
    }

    /// The Gyld host script `name` lives at `<gyld-root>/scripts/<name>`.
    pub fn script(&self, name: &str) -> PathBuf {
        self.gyld_root.join("scripts").join(name)
    }

    /// `PYTHONPATH=src:.` relative to the Gyld root, as the hosts require.
    pub fn pythonpath(&self) -> String {
        format!(
            "{}:{}",
            self.gyld_root.join("src").display(),
            self.gyld_root.display()
        )
    }

    /// A fresh, never-yet-used output directory under `builds/`.
    pub fn new_build_dir(&self, stamp: &str) -> PathBuf {
        self.builds().join(stamp)
    }
}

/// A monotonic, sortable build stamp: milliseconds since the epoch, padded so a
/// lexicographic listing is a chronological one. No clock dependency beyond
/// `SystemTime`; a clock before the epoch degrades to `0`, never a panic.
pub fn build_stamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("build-{millis:013}")
}

/// Is `path` inside `root` after lexical normalization?
///
/// The check is LEXICAL on purpose: it refuses a `..` segment and an absolute
/// path outright rather than asking the filesystem, so it answers the same way
/// for a path that does not exist yet (every output directory is one of those).
pub fn contained(root: &Path, path: &Path) -> bool {
    let normalized = match lexically_normalize(path) {
        Some(p) => p,
        None => {
            return false;
        }
    };
    let root = match lexically_normalize(root) {
        Some(p) => p,
        None => {
            return false;
        }
    };
    normalized.starts_with(&root)
}

/// Normalize `.` and `..` textually. Returns `None` when a `..` would climb
/// above the path's own root, which is exactly the escape a containment check
/// must refuse.
pub fn lexically_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => {
                out.push(other.as_os_str());
            }
        }
    }
    Some(out)
}

/// Create the bundle root's directories and the staging repository.
///
/// Idempotent: every step is a create-if-absent. The overlays tree is seeded
/// with one link per file of `<gyld-root>/examples`; a name that already exists
/// there (a supplier-written overlay, or a link from an earlier run) is left
/// exactly as it is, so a written overlay always wins over the checkout's copy.
///
/// Safe CONCURRENTLY too, which is the case the supplier actually runs: every
/// verb ensures the stage on the exchange path, the first build ensures it on
/// its own task, and a streamed run ensures it again as it settles. One caller
/// lays the stage at a time, under an exclusive lock on [`Layout::stage_lock`];
/// the others wait their turn, which is a few directory listings long. See
/// [`Staging`] for why each step being idempotent on its own was not enough.
///
/// With a decisions root configured, three more steps run first, in this order:
/// the notebooks a host wrote into the staging tree are ADOPTED into the
/// decisions root ([`adopt`]), every notebook in the decisions root is LINKED
/// into the staging tree, and a link left DANGLING by a notebook the owner
/// deleted (through git, or by hand) is removed — so deleting a file really does
/// delete the notebook at the next build. Only then are the checkout's examples
/// seeded, and only where no name exists, which is what makes the owner's copy
/// shadow a shipped sample rather than fight it.
pub fn ensure_stage(layout: &Layout) -> io::Result<()> {
    ensure_stage_with(layout, &|_| {})
}

/// [`ensure_stage`] with the ADOPTION left out, for a caller that knows a Gyld
/// host may still be writing into the staging tree.
///
/// A streamed `fork` or `link` host writes its module into `<stage>/examples`
/// and reads it back to check it after the verb's accept has gone out, so a
/// regular notebook there may be that host's file mid-run, and adopting it
/// would move the file out from under the host (G2, 2026-09-27). The run's own
/// settle adopts it once the host is done. The linking, the sweep and the
/// seeding still run: none of them touches a regular file.
pub fn ensure_stage_without_adopting(layout: &Layout) -> io::Result<()> {
    lay_stage(layout, false, &|_| {})
}

/// Where [`ensure_stage_with`] has got to, told to its `on_step` as it gets
/// there.
///
/// Production listens to none of it ([`ensure_stage`]). It is here so a test
/// can hold one caller at the exact step another caller's race needs, and
/// replay an interleaving a loaded machine produces once in a while every time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step<'a> {
    /// About to take the stage lock; the caller waits here while another holds
    /// it.
    Locking,
    /// About to claim this host-written notebook for adoption.
    Claiming(&'a Path),
    /// Claimed it: the name stays empty until the adoption lays its link.
    Claimed(&'a Path),
    /// About to ask whether this link in the staging tree leads anywhere.
    Checking(&'a Path),
    /// Found that it leads nowhere, and about to remove it.
    Dropping(&'a Path),
}

/// [`ensure_stage`], telling `on_step` each [`Step`] it reaches.
pub fn ensure_stage_with(layout: &Layout, on_step: &dyn Fn(Step<'_>)) -> io::Result<()> {
    lay_stage(layout, true, on_step)
}

/// The work of [`ensure_stage_with`], ADOPTING or not.
fn lay_stage(layout: &Layout, adopting: bool, on_step: &dyn Fn(Step<'_>)) -> io::Result<()> {
    let overlays = layout.overlays();
    std::fs::create_dir_all(&overlays)?;
    std::fs::create_dir_all(layout.builds())?;
    std::fs::create_dir_all(layout.stage())?;
    // Everything below reads a tree and then acts on what it read: one caller
    // at a time.
    let _staging = Staging::take(layout, on_step)?;

    if let Some(decisions) = layout.decisions_root.as_ref() {
        std::fs::create_dir_all(decisions)?;
        if adopting {
            for note in adopt_held(layout, on_step)?.notes() {
                eprintln!("glade-gyld: {note}");
            }
        }
        link_notebooks(decisions, &overlays)?;
        drop_dangling(&overlays, on_step)?;
    }

    let source = layout.gyld_root.join("examples");
    if source.is_dir() {
        for entry in std::fs::read_dir(&source)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let target = overlays.join(entry.file_name());
            if target.symlink_metadata().is_ok() {
                continue;
            }
            already_laid(platform::link_file(&entry.path(), &target))?;
        }
    }

    let examples = layout.stage_examples();
    if examples.symlink_metadata().is_err() {
        already_laid(platform::link_dir(&overlays, &examples))?;
    }
    Ok(())
}

/// The stage lock, HELD: an exclusive lock on [`Layout::stage_lock`] for as long
/// as this lives.
///
/// Each of [`ensure_stage`]'s steps was made safe against another caller doing
/// the SAME step ([`already_laid`], the claim in [`adopt`]). None was safe
/// against a caller doing the step before it, because a caller's listing of
/// `overlays/` is a snapshot it goes on acting on after the tree has moved. G1,
/// 2026-09-27, an adoption lost among eight racing callers:
///
/// 1. LATE lists a host-written `forked1.gyld.py`: a regular file.
/// 2. WINNER adopts it: the notebook moves to the decisions root and a link
///    takes its place.
/// 3. LATE claims the name its listing gave it, which is now the WINNER's link,
///    and the name is empty until LATE lays the link again.
/// 4. SWEEPER, whose listing already held that link, asks in that gap whether it
///    leads anywhere: nothing is there.
/// 5. LATE lays its link, and SWEEPER removes what holds the name — the good
///    link. Every caller's linking pass had already run, so nothing put it back.
///
/// The notebook itself stayed safe in the decisions root, but its stream was
/// gone from the stage until the next verb linked it again, and a build in
/// between would not have seen it. Under the lock one caller lays the stage at
/// a time, so what a caller listed is still there when it acts.
///
/// A lock on a FILE in the bundle root, because the root is what is guarded:
/// every caller shares it, whatever `Layout` value, thread or process it comes
/// from — the exchange handler, a streamed run's `settle`, the first build's
/// task, a second supplier pointed at the same root. Nothing is kept in the
/// process, and the lock goes with the open file, so a caller that panics or
/// dies cannot leave the stage locked.
struct Staging(std::fs::File);

impl Staging {
    /// Take the lock, waiting while another caller holds it. `on_step` hears
    /// [`Step::Locking`] first.
    fn take(layout: &Layout, on_step: &dyn Fn(Step<'_>)) -> io::Result<Staging> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.stage_lock())?;
        on_step(Step::Locking);
        loop {
            match file.lock() {
                Ok(()) => {
                    return Ok(Staging(file));
                }
                // A signal landing while this caller waits is no reason to
                // refuse the verb: wait again.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }
}

impl Drop for Staging {
    /// Unlock rather than leave it to the close: Windows frees a closed
    /// handle's lock only when it gets round to it.
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The authoring suffix every overlay module carries.
const NOTEBOOK_SUFFIX: &str = ".gyld.py";

/// Is `name` a notebook — an overlay module the owner's folder holds?
fn is_notebook(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().ends_with(NOTEBOOK_SUFFIX)
}

/// What one [`adopt`] pass did, so the caller can say it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Adopted {
    /// The notebooks now in the decisions root, linked back into the stage.
    pub taken: Vec<PathBuf>,
    /// `(staged, kept)` — a name both trees hold, with different bytes. NEITHER
    /// was touched.
    pub conflicts: Vec<(PathBuf, PathBuf)>,
}

impl Adopted {
    /// One line per conflict. An adoption that went through says nothing: the
    /// answer already named the file.
    pub fn notes(&self) -> Vec<String> {
        self.conflicts
            .iter()
            .map(|(staged, kept)| {
                format!(
                    "{} and {} are two different notebooks of the same name; neither was changed",
                    staged.display(),
                    kept.display()
                )
            })
            .collect()
    }
}

/// Move every notebook a Gyld host WROTE into the staging tree over to the
/// decisions root, and leave a link behind in its place.
///
/// `fork` and `link` are the hosts' own business: they write a real module into
/// `<stage>/examples` — which is the overlays tree — and read it back to check
/// it. Nothing about that changes. Afterwards the file is the OWNER's, so it is
/// moved to where his files live and the staging tree points at it. A regular,
/// non-symlink `*.gyld.py` in the overlays tree is exactly "a module a host
/// wrote": a seed is a link, and an adopted notebook is a link.
///
/// A name the decisions root already holds is not overwritten in either
/// direction. Identical bytes are the idempotent case — the adoption simply
/// finishes. DIFFERENT bytes are two notebooks, and losing one of them to a
/// tidying step is not something a tool gets to do: both stay, and the caller
/// says so.
///
/// Concurrency-safe by the stage lock ([`Staging`]), which this takes. The
/// staged file is still CLAIMED — renamed to a sibling temporary before
/// anything else — but the claim alone never made it safe: a rename takes
/// whatever holds the name, and a caller whose listing predated another's
/// adoption claimed the winner's LINK. It laid the link again, but the name was
/// empty in between, and a sweep that looked in that gap lost the adoption (G1).
pub fn adopt(layout: &Layout) -> io::Result<Adopted> {
    if layout.decisions_root.is_none() || !layout.overlays().is_dir() {
        return Ok(Adopted::default());
    }
    let _staging = Staging::take(layout, &|_| {})?;
    adopt_held(layout, &|_| {})
}

/// The work of [`adopt`], for a caller HOLDING the stage lock, telling
/// `on_step` as it claims each file.
fn adopt_held(layout: &Layout, on_step: &dyn Fn(Step<'_>)) -> io::Result<Adopted> {
    let decisions = match layout.decisions_root.as_ref() {
        Some(root) => root,
        None => {
            return Ok(Adopted::default());
        }
    };
    let overlays = layout.overlays();
    let mut adopted = Adopted::default();
    if !overlays.is_dir() {
        return Ok(adopted);
    }
    std::fs::create_dir_all(decisions)?;
    for entry in std::fs::read_dir(&overlays)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !is_notebook(&entry.file_name()) {
            continue;
        }
        let staged = entry.path();
        let kept = decisions.join(entry.file_name());
        let claimed = sibling_temp(&staged);
        on_step(Step::Claiming(&staged));
        match std::fs::rename(&staged, &claimed) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            other => other?,
        }
        on_step(Step::Claimed(&staged));
        if kept.symlink_metadata().is_ok() {
            if std::fs::read(&claimed)? == std::fs::read(&kept)? {
                std::fs::remove_file(&claimed)?;
            } else {
                // Put it back exactly where the host left it and say so. A
                // stream whose notebook is in dispute keeps building from the
                // staged one until somebody reads the line.
                std::fs::rename(&claimed, &staged)?;
                adopted.conflicts.push((staged.clone(), kept.clone()));
                continue;
            }
        } else {
            move_file(&claimed, &kept)?;
            adopted.taken.push(kept.clone());
        }
        relink(&kept, &staged)?;
    }
    Ok(adopted)
}

/// Move `from` onto `to`, across filesystems if it comes to that. The decisions
/// root is the owner's folder and need not be on the bundle root's volume.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
    }
}

/// Link every notebook of the decisions root into the staging tree, RE-POINTING
/// a link that leads somewhere else — a seed link into the Gyld checkout is
/// exactly that, and re-pointing it is how the owner's copy of a shipped sample
/// comes to be the one the hosts read.
///
/// A real file at that name is left alone: it is one of [`adopt`]'s conflicts,
/// or a module a running host is still writing
/// ([`ensure_stage_without_adopting`]).
fn link_notebooks(decisions: &Path, overlays: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(decisions)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !is_notebook(&entry.file_name()) {
            continue;
        }
        let kept = entry.path();
        let target = overlays.join(entry.file_name());
        match target.symlink_metadata() {
            Err(_) => {
                already_laid(platform::link_file(&kept, &target))?;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let points_at = std::fs::read_link(&target).ok();
                if points_at.as_deref() != Some(kept.as_path()) {
                    relink(&kept, &target)?;
                }
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Remove every link in the staging tree that leads nowhere.
///
/// That is what a notebook the owner deleted looks like from here: the link is
/// still in the tree and the file it named is gone. Left there, the capture host
/// would fail on it and take the build down with it; removed, the stream is
/// simply no longer declared — and if the checkout ships a sample of that name,
/// the seeding step below puts the sample back.
///
/// Check, then remove: the removal takes whatever holds the name BY THEN, and
/// only the stage lock makes that the link the check looked at. No other caller
/// can empty the name and fill it again in between, which is how G1 lost an
/// adoption ([`Staging`]).
fn drop_dangling(overlays: &Path, on_step: &dyn Fn(Step<'_>)) -> io::Result<()> {
    for entry in std::fs::read_dir(overlays)? {
        let entry = entry?;
        if !entry.file_type()?.is_symlink() {
            continue;
        }
        let link = entry.path();
        on_step(Step::Checking(&link));
        if !link.exists() {
            on_step(Step::Dropping(&link));
            already_gone(std::fs::remove_file(&link))?;
        }
    }
    Ok(())
}

/// Treat `AlreadyExists` from laying a link as the success it is.
///
/// The seeding above is check-then-act — `symlink_metadata`, then link — and
/// the supplier calls it from two places that run at the same time: the
/// exchange handler ensures the stage for EVERY verb, and `prepare_first_build`
/// ensures it on the bootstrap task. On a fresh bundle root both see nothing
/// and both lay the link; the loser gets `EEXIST`. The guard above already says
/// what to do when the name is taken — leave it alone — so the loser has the
/// outcome it asked for and there is nothing to report. It surfaced as
/// `bundle root unusable: File exists (os error 17)`, which refused the verb or,
/// worse, abandoned the first build and left the root with no bundle at all.
///
/// The stage lock ([`Staging`]) now keeps two of those callers from meeting
/// here at all. This stays: the supplier's own write path and a restore lay
/// links in `overlays/` without the lock, and a name one of them took is still
/// a name to leave alone.
///
/// Every other error is still an error: a read-only root, a missing parent, a
/// permission refusal all come straight back.
fn already_laid(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// And the other half of it: treat `NotFound` from REMOVING something as the
/// success it is — a restore took the same dangling link first, say, and the
/// link is gone either way. What a removal must never do is take a link laid
/// AFTER the check; that is the stage lock's job, not this one's.
fn already_gone(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Lay a link at `target` pointing at `source`, failing with `AlreadyExists`
/// when the name is taken. The overlays tree's one linking primitive, public so
/// a caller outside this module lays a seed the same way the seeding does — and
/// so no test needs a `#[cfg]` of its own to make one.
pub fn link(source: &Path, target: &Path) -> io::Result<()> {
    platform::link_file(source, target)
}

/// Point `target` at `source`, REPLACING whatever is at `target` — a link into
/// the Gyld checkout, an older real file, nothing at all.
///
/// The link is laid at a sibling temporary name and RENAMED over the target,
/// because that is the one sequence with no window in which the name is absent
/// and no step that follows the link already there. `remove_file` then
/// `symlink` would have both.
pub fn relink(source: &Path, target: &Path) -> io::Result<()> {
    let temp = sibling_temp(target);
    platform::link_file(source, &temp)?;
    match std::fs::rename(&temp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Put `bytes` at `path`, REPLACING whatever holds that name and never writing
/// THROUGH it.
///
/// The one write every overlay module goes through: the supplier's own
/// [`crate::supplier`] write path and the restore of a
/// [`crate::outcome::Snapshot`] are the same sequence, because a refused write
/// must be undone exactly as carefully as it was made. `std::fs::write` opens
/// the target by name and FOLLOWS a symlink, and `overlays/` is full of links
/// into the read-only Gyld checkout; a sibling temporary file renamed over the
/// target replaces the link itself, and makes the write atomic into the bargain.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = sibling_temp(path);
    std::fs::write(&temp, bytes)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// A sibling temporary name for an atomic replace: `<dir>/.<name>.tmp-<pid>-<n>`.
///
/// A SIBLING, not a name in the system temporary directory: `rename` is only
/// atomic within one filesystem, and the whole point of the temporary is the
/// rename. Hidden and counted, so two callers replacing the same target at the
/// same moment never collide on the temporary itself.
pub fn sibling_temp(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "overlay".to_string());
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let temp = format!(".{name}.tmp-{}-{n}", std::process::id());
    match path.parent() {
        Some(dir) => dir.join(temp),
        None => PathBuf::from(temp),
    }
}

/// Platform-specific linking, behind an explicit module boundary (the workzone's
/// conditional-compilation rule: no bare `#[cfg]` on a declaration).
#[cfg(unix)]
mod platform {
    use std::io;
    use std::path::Path;

    /// Link a seed example into the overlays tree.
    pub fn link_file(source: &Path, target: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(source, target)
    }

    /// Point the staging repository's `examples` at the overlays tree.
    pub fn link_dir(source: &Path, target: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(source, target)
    }
}

#[cfg(not(unix))]
mod platform {
    use std::io;
    use std::path::Path;

    /// No symlink guarantee off unix: copy the seed instead. A supplier-written
    /// overlay still wins, because a seed is only laid where no file exists.
    pub fn link_file(source: &Path, target: &Path) -> io::Result<()> {
        std::fs::copy(source, target).map(|_| ())
    }

    /// Without a directory symlink the staging examples tree is a real
    /// directory; the seeds are copied into it by the caller's next run.
    pub fn link_dir(_source: &Path, target: &Path) -> io::Result<()> {
        std::fs::create_dir_all(target)
    }
}

/// Record `output_dir` as the bundle root's latest successful build.
pub fn write_latest(layout: &Layout, output_dir: &Path) -> io::Result<()> {
    let relative = output_dir
        .strip_prefix(&layout.bundle_root)
        .unwrap_or(output_dir);
    let body = serde_json::json!({ "output_dir": relative.display().to_string() });
    std::fs::write(layout.latest_pointer(), format!("{body}\n"))
}

/// The latest successful build, if there is one. The pointer is authoritative;
/// when it is missing or stale, the newest `builds/` directory holding a
/// `streams.json` answers instead, so a hand-copied bundle is still usable.
pub fn latest_build(layout: &Layout) -> Option<PathBuf> {
    if let Ok(text) = std::fs::read_to_string(layout.latest_pointer()) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(relative) = value.get("output_dir").and_then(|v| v.as_str()) {
                let candidate = layout.bundle_root.join(relative);
                if candidate.join("streams.json").is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(layout.builds())
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("streams.json").is_file())
        .collect();
    candidates.sort();
    candidates.pop()
}

/// A file pointer for the large-file transport: `{path, digest, bytes}`, where
/// `path` is the URL the static server exposes and `digest` is the sha256 of the
/// bytes. The consumer checks the digest; it never trusts the pointer.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FilePointer {
    pub path: String,
    pub digest: String,
    pub bytes: u64,
}

/// Build a pointer for `file`, whose URL path is `<static_base>/<relative>`.
/// Reads the file to digest it, so a caller holding a pointer holds a promise it
/// has actually checked.
pub fn file_pointer(layout: &Layout, file: &Path, static_base: &str) -> io::Result<FilePointer> {
    let bytes = std::fs::read(file)?;
    let relative = file.strip_prefix(&layout.bundle_root).unwrap_or(file);
    let base = static_base.trim_end_matches('/');
    Ok(FilePointer {
        path: format!("{}/{}", base, relative.display()),
        digest: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
    })
}

/// Lowercase hex sha256 — the digest every emitted Gyld artefact is identified
/// by, and the one the wire's chain hash uses.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("glade-gyld-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn layout_places_every_path_under_the_two_roots() {
        let l = Layout::new(PathBuf::from("/g"), PathBuf::from("/b"));
        assert_eq!(l.overlays(), PathBuf::from("/b/overlays"));
        assert_eq!(l.stage_examples(), PathBuf::from("/b/stage/examples"));
        // Beside the trees the hosts read, never in them.
        assert_eq!(l.stage_lock(), PathBuf::from("/b/stage.lock"));
        assert_eq!(
            l.new_build_dir("build-1"),
            PathBuf::from("/b/builds/build-1")
        );
        assert_eq!(
            l.script("emit_decision_streams.py"),
            PathBuf::from("/g/scripts/emit_decision_streams.py")
        );
        assert_eq!(l.pythonpath(), "/g/src:/g");
    }

    #[test]
    fn the_decisions_root_is_the_home_of_written_overlays_when_there_is_one() {
        // With none configured, a written module lives in the staging tree, as
        // it always did.
        let plain = Layout::new(PathBuf::from("/g"), PathBuf::from("/b"));
        assert_eq!(plain.overlay_home(), PathBuf::from("/b/overlays"));
        assert_eq!(plain.decisions_root, None);

        // With one, that is the home — and the staging tree is unchanged: it is
        // still where the capture hosts read, now through links.
        let owned = plain.with_decisions_root(Some(PathBuf::from("/glade-wz/decisions")));
        assert_eq!(owned.overlay_home(), PathBuf::from("/glade-wz/decisions"));
        assert_eq!(owned.overlays(), PathBuf::from("/b/overlays"));
        assert_eq!(owned.stage_examples(), PathBuf::from("/b/stage/examples"));
    }

    #[test]
    fn containment_refuses_escapes_and_absolutes() {
        let root = Path::new("/b");
        assert!(contained(root, Path::new("/b/overlays/x.py")));
        assert!(contained(root, Path::new("/b")));
        assert!(!contained(root, Path::new("/b/../etc/passwd")));
        assert!(!contained(root, Path::new("/etc/passwd")));
        assert!(!contained(root, Path::new("/bb/x")));
        // A climb above the path's own root is refused rather than clamped.
        assert!(!contained(root, Path::new("../../etc")));
    }

    #[test]
    fn build_stamps_sort_chronologically() {
        let a = build_stamp();
        let b = build_stamp();
        assert!(a <= b, "{a} then {b}");
        assert!(a.starts_with("build-") && a.len() == 19, "{a}");
    }

    #[test]
    fn ensure_stage_seeds_examples_without_touching_the_checkout() {
        let root = tmp("stage");
        let gyld = root.join("gyld");
        std::fs::create_dir_all(gyld.join("examples")).unwrap();
        std::fs::write(gyld.join("examples/glade-decisions.gyld.py"), "base\n").unwrap();
        let bundle = root.join("bundle");
        let layout = Layout::new(gyld.clone(), bundle.clone());

        ensure_stage(&layout).unwrap();
        let seeded = layout.stage_examples().join("glade-decisions.gyld.py");
        assert_eq!(std::fs::read_to_string(&seeded).unwrap(), "base\n");

        // An overlay written into the tree wins, and a second ensure leaves it.
        let mine = layout.overlays().join("glade-decisions-keys.gyld.py");
        std::fs::write(&mine, "overlay\n").unwrap();
        ensure_stage(&layout).unwrap();
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "overlay\n");

        // The checkout is untouched: exactly the one file it started with.
        let listed = std::fs::read_dir(gyld.join("examples")).unwrap().count();
        assert_eq!(listed, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensuring_one_fresh_stage_from_several_callers_at_once_is_not_an_error() {
        // The supplier's own shape: the exchange handler ensures the stage for
        // every verb while the bootstrap task ensures it for the first build,
        // so a FRESH root is staged by several callers at the same moment.
        // Each one used to check the link was absent and then lay it, and the
        // loser's `EEXIST` refused a verb or abandoned the first build.
        let root = tmp("stage-race");
        let gyld = root.join("gyld");
        std::fs::create_dir_all(gyld.join("examples")).unwrap();
        for n in 0..8 {
            std::fs::write(gyld.join(format!("examples/s{n}.gyld.py")), "base\n").unwrap();
        }

        // Several fresh roots: the window is between the check and the link, so
        // one root is one sample of it.
        for attempt in 0..8 {
            let bundle = root.join(format!("bundle-{attempt}"));
            let layout = Layout::new(gyld.clone(), bundle.clone());
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let start = &start;
                let layout = &layout;
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(move || {
                            start.wait();
                            ensure_stage(layout)
                        })
                    })
                    .collect();
                for handle in handles {
                    handle
                        .join()
                        .expect("the staging thread ran")
                        .expect("a concurrent ensure_stage is not a failure");
                }
            });
            // And the stage they raced to build is the one stage: every seed
            // reachable through `stage/examples`, exactly once.
            for n in 0..8 {
                let seeded = layout.stage_examples().join(format!("s{n}.gyld.py"));
                assert_eq!(std::fs::read_to_string(&seeded).unwrap(), "base\n");
            }
            assert_eq!(std::fs::read_dir(layout.overlays()).unwrap().count(), 8);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A checkout holding `named` samples, a bundle root beside it, and the
    /// owner's decisions folder — the three trees of the real arrangement.
    fn three_trees(tag: &str, named: &[&str]) -> (PathBuf, Layout) {
        let root = tmp(tag);
        let gyld = root.join("gyld");
        std::fs::create_dir_all(gyld.join("examples")).unwrap();
        for name in named.iter() {
            std::fs::write(gyld.join("examples").join(name), "shipped\n").unwrap();
        }
        let layout = Layout::new(gyld, root.join("bundle"))
            .with_decisions_root(Some(root.join("decisions")));
        (root, layout)
    }

    /// Where a name in the staging tree leads: `None` for a real file.
    fn points_at(overlays: &Path, name: &str) -> Option<PathBuf> {
        std::fs::read_link(overlays.join(name)).ok()
    }

    #[test]
    fn a_host_written_notebook_is_adopted_into_the_decisions_root_and_linked_back() {
        let (root, layout) = three_trees("adopt", &["glade-decisions.gyld.py"]);
        ensure_stage(&layout).unwrap();

        // What `fork` does: the host writes a real module into the staging tree.
        let name = "glade-decisions-keys-a.gyld.py";
        let staged = layout.overlays().join(name);
        std::fs::write(&staged, "forked\n").unwrap();

        let adopted = adopt(&layout).unwrap();
        let kept = root.join("decisions").join(name);
        assert_eq!(adopted.taken, vec![kept.clone()]);
        assert!(adopted.conflicts.is_empty() && adopted.notes().is_empty());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "forked\n");
        assert_eq!(
            points_at(&layout.overlays(), name),
            Some(kept.clone()),
            "the staging tree points at the owner's file"
        );
        // And the hosts still read it through the staging repository.
        assert_eq!(
            std::fs::read_to_string(layout.stage_examples().join(name)).unwrap(),
            "forked\n"
        );

        // Twice over is nothing: a link is not a host-written file.
        let again = adopt(&layout).unwrap();
        assert_eq!(again, Adopted::default());
        ensure_stage(&layout).unwrap();
        assert_eq!(points_at(&layout.overlays(), name), Some(kept));
        assert_eq!(
            std::fs::read_dir(root.join("decisions")).unwrap().count(),
            1
        );

        // With no decisions root, nothing is adopted at all.
        let plain = Layout::new(layout.gyld_root.clone(), root.join("plain"));
        std::fs::create_dir_all(plain.overlays()).unwrap();
        std::fs::write(plain.overlays().join(name), "forked\n").unwrap();
        assert_eq!(adopt(&plain).unwrap(), Adopted::default());
        assert!(plain.overlays().join(name).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_different_notebooks_of_one_name_are_both_left_where_they_are() {
        let (root, layout) = three_trees("adopt-clash", &[]);
        ensure_stage(&layout).unwrap();
        let name = "glade-decisions-keys-a.gyld.py";
        let kept = root.join("decisions").join(name);
        std::fs::write(&kept, "the owner's\n").unwrap();
        let staged = layout.overlays().join(name);
        std::fs::write(&staged, "the host's\n").unwrap();

        let adopted = adopt(&layout).unwrap();
        assert_eq!(adopted.taken, Vec::<PathBuf>::new());
        assert_eq!(adopted.conflicts, vec![(staged.clone(), kept.clone())]);
        let note = adopted.notes().join("");
        assert!(
            note.contains(&staged.display().to_string())
                && note.contains(&kept.display().to_string())
                && note.contains("neither was changed"),
            "{note}"
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "the owner's\n");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "the host's\n");
        assert_eq!(
            points_at(&layout.overlays(), name),
            None,
            "the staged file is still the real file the host wrote"
        );

        // The same bytes are the idempotent case, not a clash: the adoption
        // finishes rather than reporting one.
        std::fs::write(&staged, "the owner's\n").unwrap();
        let adopted = adopt(&layout).unwrap();
        assert!(adopted.conflicts.is_empty() && adopted.taken.is_empty());
        assert_eq!(points_at(&layout.overlays(), name), Some(kept));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_decisions_root_shadows_the_checkout_and_a_deleted_notebook_disappears() {
        let sample = "glade-decisions-stream-a.gyld.py";
        let (root, layout) = three_trees("seeding", &["glade-decisions.gyld.py", sample]);
        let overlays = layout.overlays();

        // First pass: nothing of the owner's yet, so both samples are seeded
        // from the checkout exactly as they always were.
        ensure_stage(&layout).unwrap();
        assert_eq!(
            points_at(&overlays, sample),
            Some(layout.gyld_root.join("examples").join(sample)),
            "with no copy of his own, the owner reads the shipped sample"
        );

        // The owner answers a question in the shipped sample: his copy lands in
        // the decisions root, and the seed link is RE-POINTED at it.
        let kept = root.join("decisions").join(sample);
        std::fs::write(&kept, "the owner's copy\n").unwrap();
        ensure_stage(&layout).unwrap();
        assert_eq!(points_at(&overlays, sample), Some(kept.clone()));
        assert_eq!(
            std::fs::read_to_string(layout.stage_examples().join(sample)).unwrap(),
            "the owner's copy\n"
        );
        // And the shipped sample is untouched: copy-on-write, not an edit.
        assert_eq!(
            std::fs::read_to_string(layout.gyld_root.join("examples").join(sample)).unwrap(),
            "shipped\n"
        );

        // A notebook with no sample behind it: `git checkout` takes it away, and
        // the next build must not see a stream that is no longer declared.
        let own = "glade-decisions-keys-a.gyld.py";
        std::fs::write(root.join("decisions").join(own), "mine\n").unwrap();
        ensure_stage(&layout).unwrap();
        assert!(overlays.join(own).exists());
        std::fs::remove_file(root.join("decisions").join(own)).unwrap();
        ensure_stage(&layout).unwrap();
        assert!(
            overlays.join(own).symlink_metadata().is_err(),
            "a dangling link is removed, so deleting the file deletes the notebook"
        );

        // Reverting the owner's copy of a SAMPLE puts the sample back.
        std::fs::remove_file(&kept).unwrap();
        ensure_stage(&layout).unwrap();
        assert_eq!(
            points_at(&overlays, sample),
            Some(layout.gyld_root.join("examples").join(sample))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensuring_one_fresh_stage_with_a_decisions_root_from_several_callers_is_not_an_error() {
        // The no-decisions case above; now with the owner's folder in the middle,
        // which adds the adoption, the linking and the dangling sweep to the
        // steps two callers reach at the same moment.
        let root = tmp("stage-race-decisions");
        let gyld = root.join("gyld");
        std::fs::create_dir_all(gyld.join("examples")).unwrap();
        for n in 0..8 {
            std::fs::write(gyld.join(format!("examples/s{n}.gyld.py")), "shipped\n").unwrap();
        }

        for attempt in 0..8 {
            let decisions = root.join(format!("decisions-{attempt}"));
            std::fs::create_dir_all(&decisions).unwrap();
            // Four the owner already holds, and four a host has just written
            // into the staging tree for the adoption to find.
            for n in 0..4 {
                std::fs::write(decisions.join(format!("own{n}.gyld.py")), "mine\n").unwrap();
            }
            let layout = Layout::new(gyld.clone(), root.join(format!("bundle-{attempt}")))
                .with_decisions_root(Some(decisions.clone()));
            std::fs::create_dir_all(layout.overlays()).unwrap();
            for n in 0..4 {
                std::fs::write(
                    layout.overlays().join(format!("forked{n}.gyld.py")),
                    "forked\n",
                )
                .unwrap();
            }

            let start = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let start = &start;
                let layout = &layout;
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(move || {
                            start.wait();
                            ensure_stage(layout)
                        })
                    })
                    .collect();
                for handle in handles {
                    handle
                        .join()
                        .expect("the staging thread ran")
                        .expect("a concurrent ensure_stage is not a failure");
                }
            });

            // One stage, whoever laid it: every seed, every notebook of the
            // owner's and every adopted fork reachable once through the stage.
            for n in 0..8 {
                assert_eq!(
                    std::fs::read_to_string(layout.stage_examples().join(format!("s{n}.gyld.py")))
                        .unwrap(),
                    "shipped\n"
                );
            }
            for n in 0..4 {
                let own = format!("own{n}.gyld.py");
                assert_eq!(
                    points_at(&layout.overlays(), &own),
                    Some(decisions.join(&own))
                );
                let forked = format!("forked{n}.gyld.py");
                assert_eq!(
                    points_at(&layout.overlays(), &forked),
                    Some(decisions.join(&forked)),
                    "the adoption happened exactly once"
                );
                assert_eq!(
                    std::fs::read_to_string(decisions.join(&forked)).unwrap(),
                    "forked\n"
                );
            }
            assert_eq!(std::fs::read_dir(layout.overlays()).unwrap().count(), 16);
            assert_eq!(std::fs::read_dir(&decisions).unwrap().count(), 8);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Who a step came from, in the race the next test replays.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Caller {
        /// Listed the host's file before anyone adopted it, and claims it late.
        Late,
        /// Adopts the file, start to finish.
        Winner,
        /// Drops the links that lead nowhere.
        Sweeper,
    }

    /// What a caller has done: a [`Step`] it reached, or its return.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Did {
        Locking,
        Claiming,
        Claimed,
        Checking,
        Dropping,
        Returned,
    }

    /// Every `(caller, did)` so far, and a way to wait for one.
    #[derive(Default)]
    struct Score {
        seen: std::sync::Mutex<std::collections::HashSet<(Caller, Did)>>,
        moved: std::sync::Condvar,
    }

    impl Score {
        fn did(&self, who: Caller, what: Did) {
            self.seen.lock().unwrap().insert((who, what));
            self.moved.notify_all();
        }

        /// Wait until `who` has done any of `any`. Bounded, so a replay that
        /// stalls fails the test instead of hanging the suite.
        fn until(&self, who: Caller, any: &[Did]) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut seen = self.seen.lock().unwrap();
            while !any.iter().any(|what| seen.contains(&(who, *what))) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !left.is_zero(),
                    "the replay stalled: {who:?} never did any of {any:?}"
                );
                seen = self.moved.wait_timeout(seen, left).unwrap().0;
            }
        }
    }

    #[test]
    fn an_adoption_survives_a_late_claim_and_a_sweep_by_two_other_callers() {
        // G1, 2026-09-27: the eight-caller race above lost an adoption once, on
        // a loaded machine. This is the interleaving that loses it, held step by
        // step through `ensure_stage_with`:
        //
        // 1. LATE lists `overlays/`: the host's file, a regular file.
        // 2. WINNER adopts it — the notebook moves to the decisions root and a
        //    link takes its place — and returns.
        // 3. SWEEPER lists `overlays/`: the winner's link.
        // 4. LATE acts on its listing and claims the NAME: the winner's link.
        // 5. SWEEPER asks whether that link leads anywhere. The name is empty.
        // 6. LATE finds the notebook kept already and lays its link again.
        // 7. SWEEPER removes what now holds the name — the good link — and
        //    nothing lays it again: every caller's linking pass has run.
        //
        // With the stage lock, WINNER and SWEEPER stop at `Locking` while LATE
        // is inside. The replay takes that as its cue to let LATE go on, so the
        // same steps run to the end instead of deadlocking, one caller at a time.
        let (root, layout) = three_trees("adopt-late", &[]);
        let name = "glade-decisions-forked.gyld.py";
        let staged = layout.overlays().join(name);
        let kept = root.join("decisions").join(name);
        std::fs::create_dir_all(layout.overlays()).unwrap();
        std::fs::write(&staged, "forked\n").unwrap();

        let score = Score::default();
        let on_late = |step: Step<'_>| match step {
            Step::Claiming(path) if path == staged.as_path() => {
                score.did(Caller::Late, Did::Claiming);
                score.until(Caller::Winner, &[Did::Returned, Did::Locking]);
                score.until(Caller::Sweeper, &[Did::Checking, Did::Locking]);
            }
            Step::Claimed(path) if path == staged.as_path() => {
                score.did(Caller::Late, Did::Claimed);
                score.until(Caller::Sweeper, &[Did::Dropping, Did::Locking]);
            }
            _ => {}
        };
        let on_winner = |step: Step<'_>| {
            if matches!(step, Step::Locking) {
                score.did(Caller::Winner, Did::Locking);
            }
        };
        let on_sweeper = |step: Step<'_>| match step {
            Step::Locking => {
                score.did(Caller::Sweeper, Did::Locking);
            }
            Step::Checking(path) if path == staged.as_path() => {
                score.did(Caller::Sweeper, Did::Checking);
                score.until(Caller::Late, &[Did::Claimed]);
            }
            Step::Dropping(path) if path == staged.as_path() => {
                score.did(Caller::Sweeper, Did::Dropping);
                score.until(Caller::Late, &[Did::Returned]);
            }
            _ => {}
        };

        let returned = std::thread::scope(|scope| {
            let late = scope.spawn(|| {
                let done = ensure_stage_with(&layout, &on_late);
                score.did(Caller::Late, Did::Returned);
                done
            });
            score.until(Caller::Late, &[Did::Claiming]);
            let winner = scope.spawn(|| {
                let done = ensure_stage_with(&layout, &on_winner);
                score.did(Caller::Winner, Did::Returned);
                done
            });
            score.until(Caller::Winner, &[Did::Returned, Did::Locking]);
            let sweeper = scope.spawn(|| {
                let done = ensure_stage_with(&layout, &on_sweeper);
                score.did(Caller::Sweeper, Did::Returned);
                done
            });
            [late, winner, sweeper].map(|caller| caller.join().expect("the staging thread ran"))
        });
        for done in returned {
            done.expect("a concurrent ensure_stage is not a failure");
        }

        assert_eq!(
            points_at(&layout.overlays(), name),
            Some(kept.clone()),
            "the adoption the winner finished is still in the stage"
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "forked\n");
        // Nothing left behind: no claimed temporary, no second copy.
        assert_eq!(std::fs::read_dir(layout.overlays()).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_dir(root.join("decisions")).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stage_lock_that_cannot_be_taken_refuses_the_staging_rather_than_run_it_unlocked() {
        let (root, layout) = three_trees("stage-lock", &[]);
        let name = "glade-decisions-forked.gyld.py";
        let staged = layout.overlays().join(name);
        std::fs::create_dir_all(layout.overlays()).unwrap();
        std::fs::write(&staged, "forked\n").unwrap();
        // A lock file nothing can open for writing: a directory of that name.
        std::fs::create_dir_all(layout.stage_lock()).unwrap();

        ensure_stage(&layout).expect_err("no lock, no staging");
        adopt(&layout).expect_err("and no adoption either");
        // Nothing moved: the host's file is where it left it, and the owner's
        // folder was never even made.
        assert!(staged.symlink_metadata().unwrap().file_type().is_file());
        assert!(root.join("decisions").symlink_metadata().is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_pointer_round_trips_and_falls_back() {
        let root = tmp("latest");
        let layout = Layout::new(root.join("gyld"), root.clone());
        std::fs::create_dir_all(layout.builds()).unwrap();
        assert_eq!(latest_build(&layout), None);

        let one = layout.new_build_dir("build-0000000000001");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::write(one.join("streams.json"), "{}").unwrap();
        // No pointer yet: the newest build directory answers.
        assert_eq!(latest_build(&layout).as_deref(), Some(one.as_path()));

        let two = layout.new_build_dir("build-0000000000002");
        std::fs::create_dir_all(&two).unwrap();
        std::fs::write(two.join("streams.json"), "{}").unwrap();
        write_latest(&layout, &two).unwrap();
        assert_eq!(latest_build(&layout).as_deref(), Some(two.as_path()));

        // A pointer at a directory with no bundle in it falls back rather than
        // reporting a build that is not there.
        write_latest(&layout, &layout.new_build_dir("build-gone")).unwrap();
        assert_eq!(latest_build(&layout).as_deref(), Some(two.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_pointers_carry_the_url_path_digest_and_size() {
        let root = tmp("ptr");
        let layout = Layout::new(root.join("gyld"), root.clone());
        let file = root.join("builds/b1/streams/base/lenses/decisions.lens.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"abc").unwrap();

        let p = file_pointer(&layout, &file, "/gyld/").unwrap();
        assert_eq!(
            p.path,
            "/gyld/builds/b1/streams/base/lenses/decisions.lens.json"
        );
        assert_eq!(p.bytes, 3);
        assert_eq!(
            p.digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
