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
//!   overlays/           the writable examples tree: one symlink per file of
//!                       <gyld-root>/examples, plus the overlay modules the
//!                       supplier writes (a written file shadows nothing — the
//!                       symlinks are only seeded where no file exists).
//!   stage/examples  ->  ../overlays      (`--repository <bundle-root>/stage`)
//!   builds/<stamp>/     one emitted bundle per build; never overwritten.
//!   latest.json         {"output_dir": "builds/<stamp>"} — swapped after a
//!                       successful build (section 4.7: "swaps a symlink or
//!                       index after a successful build").
//! ```
//!
//! `--gyld-root` is therefore used for exactly two things: running the scripts
//! out of `<gyld-root>/scripts`, and seeding the overlays tree with the base
//! example sources. Nothing under it is ever written.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// The bundle-root layout, resolved once from the two configured roots.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub gyld_root: PathBuf,
    pub bundle_root: PathBuf,
}

impl Layout {
    pub fn new(gyld_root: PathBuf, bundle_root: PathBuf) -> Layout {
        Layout {
            gyld_root,
            bundle_root,
        }
    }

    /// The writable examples tree (also the overlays home).
    pub fn overlays(&self) -> PathBuf {
        self.bundle_root.join("overlays")
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

    pub fn latest_pointer(&self) -> PathBuf {
        self.bundle_root.join("latest.json")
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
/// Idempotent CONCURRENTLY too, which is the case the supplier actually runs:
/// every verb ensures the stage on the exchange path while the first build
/// ensures it on its own task, so on a fresh bundle root two callers reach the
/// same absent link at once. See [`already_laid`].
pub fn ensure_stage(layout: &Layout) -> io::Result<()> {
    let overlays = layout.overlays();
    std::fs::create_dir_all(&overlays)?;
    std::fs::create_dir_all(layout.builds())?;
    std::fs::create_dir_all(layout.stage())?;

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
/// Every other error is still an error: a read-only root, a missing parent, a
/// permission refusal all come straight back.
fn already_laid(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
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
