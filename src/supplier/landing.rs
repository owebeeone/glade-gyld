use super::*;

/// Take what a Gyld host wrote into the staging tree over to the decisions root.
///
/// `fork` and `link` write their module themselves, into `<stage>/examples`, and
/// read it back to check it — none of which changes. Afterwards the file is the
/// OWNER's, so [`bundle::ensure_stage`] adopts it: moved to his folder, with a
/// link left in its place. Idempotent, so a verb that had already settled its
/// own notebook (`answer`, `ask`) costs one wasted directory listing and nothing
/// else. A failed run settles nothing — there is no file to take.
///
/// An adoption failure is a LOG LINE, not a refusal: the run itself succeeded,
/// the module is in the staging tree, and the stream builds either way.
pub(super) fn settle(config: &GyldConfig, plan: &Plan, out: &RunOutput) {
    if out.exit != 0 || plan.overlay.is_none() || config.layout.decisions_root.is_none() {
        return;
    }
    if let Err(e) = bundle::ensure_stage(&config.layout) {
        eprintln!("glade-gyld: could not adopt the notebook this run wrote: {e}");
    }
}

/// The notebook this verb has left behind, IF it is really there now.
///
/// A path that exists, not a path that was planned. `answer` and `ask` write the
/// file before the answer goes out, so they always name it; a streamed `fork`
/// answers before its host has run at all, and names nothing rather than promise
/// a file that may never arrive.
pub(super) fn overlay_left(plan: &Plan) -> Option<String> {
    let path = plan.overlay.as_ref()?;
    path.symlink_metadata().ok()?;
    Some(path.display().to_string())
}

/// Decide what a writing run DID, and make the bundle root agree with it.
///
/// A writing verb that makes things worse is refused and leaves no trace: the
/// notebook goes back exactly as it was ([`Snapshot::restore`]), the build this
/// run made is removed, nothing is recorded as the latest and nothing is
/// published. Answers the refusal and Gyld's own validation document, or `None`
/// when the write stands and the caller may go on to [`settle`] and [`finish`].
///
/// The build directory must GO and not merely be left unpublished:
/// [`bundle::latest_build`] falls back to the newest `builds/` directory holding a
/// `streams.json`, so a refused-but-COMPLETE build left behind would become the
/// current one by itself the next time the pointer was missing or stale.
///
/// One line on stderr, always. The defect this answers was silent: a rejected
/// answer left a broken file, abandoned the rebuild, and said nothing anywhere.
pub(super) fn land(
    layout: &Layout,
    plan: &Plan,
    ran: Result<&RunOutput, &str>,
    snapshot: Option<&Snapshot>,
    previous: Option<&std::path::Path>,
) -> Option<(Refusal, Option<serde_json::Value>)> {
    let (mut refusal, document) = match outcome::classify(plan, ran, previous) {
        outcome::Outcome::Accepted => {
            return None;
        }
        outcome::Outcome::Refused { refusal, document } => (refusal, document),
    };
    let put = match snapshot {
        Some(snapshot) => match snapshot.restore() {
            Ok(()) => {
                refusal.restored = true;
                Put::Restored
            }
            Err(e) => {
                eprintln!("glade-gyld: the notebook could NOT be put back: {e}");
                Put::Failed
            }
        },
        None => Put::Nothing,
    };
    discard(layout, plan);
    eprintln!("{}", outcome::said(&plan.verb, &refusal, put));
    Some((refusal, document))
}

/// Remove the build this run made, if it made one. Only ever a directory under
/// the root's own `builds/`: the recursive removal is checked against the layout
/// rather than trusted to the plan, cheap insurance on the one call here that
/// deletes a tree.
fn discard(layout: &Layout, plan: &Plan) {
    let dir = match plan.output_dir.as_ref() {
        Some(dir) => dir,
        None => {
            return;
        }
    };
    if !bundle::contained(&layout.builds(), dir) {
        eprintln!(
            "glade-gyld: not removing {}: it is not under {}",
            dir.display(),
            layout.builds().display()
        );
        return;
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("glade-gyld: could not remove {}: {e}", dir.display());
        }
    }
}

/// Record a successful build as the bundle root's latest, publish its documents
/// onto the value surfaces, and answer with the directory it built. A failed run
/// advertises nothing: the previous bundle stands untouched and nothing is
/// published over it.
pub(super) fn finish(
    writer: &Writer,
    config: &Arc<GyldConfig>,
    handle: &Handle,
    plan: &Plan,
    out: &RunOutput,
) -> Option<PathBuf> {
    let dir = plan.output_dir.as_ref()?;
    if out.exit != 0 || !dir.join("streams.json").is_file() {
        return None;
    }
    if let Err(e) = bundle::write_latest(&config.layout, dir) {
        eprintln!("glade-gyld: could not record the latest build: {e}");
    }
    spawn_publish(writer.clone(), config.clone(), handle.clone(), dir.clone());
    Some(dir.clone())
}
