use super::*;

/// Publish the build's documents onto the value surfaces (step 4.2), off the
/// exchange's own thread: the answer already carried the build directory, and a
/// mount converges when the ops land.
///
/// The ONE publication path. A build the supplier just ran, a build it found in
/// the bundle root when it attached and the first build it made for itself all
/// arrive here, so all three land the same documents and log the same line.
pub(super) fn spawn_publish(
    writer: Writer,
    config: Arc<GyldConfig>,
    handle: Handle,
    output_dir: PathBuf,
) {
    handle.spawn(async move {
        let plan = publish::publications(&config.layout, &output_dir, &config.surfaces);
        for note in plan.notes.iter() {
            eprintln!("glade-gyld: not published: {note}");
        }
        // Pick the chains up before writing to them: this session's origin is
        // one an earlier session already wrote under, and appending without its
        // history is a write that goes nowhere. Every one of them first, so the
        // check below comes after the last replay.
        for publication in plan.publications.iter() {
            let key = publication.key.as_deref().unwrap_or_default().as_bytes();
            writer.resume(&publication.glade_id, key).await;
        }
        // A publication that has been overtaken must not land. Two of them can be
        // in flight at once — the build found at attach, and a Rebuild that
        // arrived while that one was still resuming — and the value fold takes
        // the HIGHEST lamport, not the newest build, so whichever appends last
        // wins however old it is. The bundle root's pointer arbitrates: `finish`
        // records a build as the latest BEFORE it spawns its publish, so an
        // output directory that is no longer the latest is a stale republish.
        let overtaken = match (output_dir.file_name(), bundle::latest_build(&config.layout)) {
            (Some(mine), Some(latest)) => latest.file_name().is_some_and(|newest| newest != mine),
            _ => false,
        };
        if overtaken {
            eprintln!(
                "glade-gyld: not published: {} — overtaken by a newer build",
                named(&config.layout, &output_dir)
            );
            return;
        }
        for publication in plan.publications.iter() {
            let key = publication.key.as_deref().unwrap_or_default().as_bytes();
            let written = writer
                .write(
                    &publication.glade_id,
                    "value",
                    publication.payload.clone(),
                    key,
                )
                .await;
            if let Err(e) = written {
                eprintln!(
                    "glade-gyld: could not publish {} {:?}: {e}",
                    publication.glade_id, publication.key
                );
            }
        }
        if !plan.publications.is_empty() {
            eprintln!(
                "glade-gyld: published {} ({} streams)",
                named(&config.layout, &output_dir),
                plan.streams
            );
        }
    });
}

/// A build directory as the bundle root names it — `builds/<stamp>` — which is
/// what `latest.json` records and what the static path serves it under. The
/// absolute path is the fallback for a directory that is somehow not under the
/// root, so a log line never silently drops where it was.
pub(super) fn named(layout: &Layout, output_dir: &std::path::Path) -> String {
    output_dir
        .strip_prefix(&layout.bundle_root)
        .unwrap_or(output_dir)
        .display()
        .to_string()
}
