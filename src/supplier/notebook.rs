use super::*;

/// Write the planned overlay module, refusing to clobber one unless the plan
/// says so. A write failure is data.
///
/// NEVER THROUGH A SYMLINK. `overlays/` holds one seed link per file of the Gyld
/// checkout's `examples`, and `std::fs::write` follows a link: a write to
/// `overlays/glade-decisions-stream-a.gyld.py` used to land in the owner's
/// checkout, which the containment check could not see because it is lexical and
/// the path it was handed really is under the bundle root. So the text goes to a
/// sibling temporary file and is RENAMED over the target — `rename` replaces the
/// link itself instead of following it, and makes the write atomic into the
/// bargain: a reader of that name sees the old module or the new one, never half
/// of either.
///
/// The `exists` test does not follow a link either ([`std::fs::symlink_metadata`]):
/// a seed link IS something at that name, and an unforced write must refuse it
/// rather than ask what it points at.
///
/// AND NOT THROUGH A SYMLINKED DIRECTORY. The planner's containment check is
/// lexical, which is what keeps it pure; that leaves the filesystem's own
/// question — where does this directory actually lead — to be asked here, where
/// there is a filesystem to ask. The target's parent is canonicalized and must
/// come out inside the canonicalized [`Layout::overlay_home`], so a directory
/// link laid in the tree cannot redirect a ruling somewhere else.
pub(super) fn write_overlay(layout: &Layout, plan: &Plan) -> Result<(), String> {
    let write = match plan.write.as_ref() {
        Some(w) => w,
        None => {
            return Ok(());
        }
    };
    if !write.force && write.path.symlink_metadata().is_ok() {
        return Err(format!(
            "{} exists; nothing is overwritten",
            write.path.display()
        ));
    }
    let home = layout.overlay_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    // Lexically first, so nothing is CREATED outside the home on the way to
    // finding out that the write does not belong there.
    if !bundle::contained(&home, &write.path) {
        return Err(format!(
            "cannot write {}: it is not in the overlay home {}",
            write.path.display(),
            home.display()
        ));
    }
    let parent = write
        .path
        .parent()
        .ok_or_else(|| format!("cannot write {}: it has no directory", write.path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let real_home = canonical(&home)?;
    let real_parent = canonical(parent)?;
    if !real_parent.starts_with(&real_home) {
        return Err(format!(
            "cannot write {}: {} leads outside the overlay home {}",
            write.path.display(),
            real_parent.display(),
            real_home.display()
        ));
    }

    bundle::replace(&write.path, write.text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", write.path.display()))
}

/// Where a directory really is, links resolved. A failure is data, like every
/// other refusal on the write path.
fn canonical(dir: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(dir).map_err(|e| format!("cannot resolve {}: {e}", dir.display()))
}

/// Point the staging tree at the notebook the write just left in the decisions
/// root, REPLACING whatever held that name — a seed link into the Gyld checkout,
/// or a real file from before there was a decisions root.
///
/// This is the whole of copy-on-write for a shipped sample. An `answer` on
/// `stream-a` writes the owner's copy into his folder; this points the staging
/// tree at it; the sample in the checkout is never touched and is shadowed from
/// here on. With no decisions root the write already landed in the staging tree
/// and there is nothing to point anywhere.
pub(super) fn stage_notebook(layout: &Layout, plan: &Plan) -> Result<(), String> {
    let decisions = match layout.decisions_root.as_ref() {
        Some(root) => root,
        None => {
            return Ok(());
        }
    };
    let written = match plan.write.as_ref() {
        Some(w) => &w.path,
        None => {
            return Ok(());
        }
    };
    let name = match written.file_name() {
        Some(n) => n,
        None => {
            return Ok(());
        }
    };
    if written.parent() != Some(decisions.as_path()) {
        return Ok(());
    }
    let staged = layout.overlays().join(name);
    bundle::relink(written, &staged).map_err(|e| {
        format!(
            "cannot point {} at {}: {e}",
            staged.display(),
            written.display()
        )
    })
}
