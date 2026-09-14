//! Share publication (GyldGrythPlugins.md 4.7, step 4.2): after a successful
//! build, the bundle's documents are appended to VALUE surfaces so every mount
//! in the UI converges without a second round trip.
//!
//! | surface | key | value |
//! | --- | --- | --- |
//! | `gyld.streams` | none | the build's `streams.json` |
//! | `gyld.stream` | stream id | that stream's `stream.json` |
//! | `gyld.decisions` | stream id | that stream's `decide-now.json` |
//! | `gyld.lens` | `<stream>/<perspective>` | a `{path, digest, bytes}` POINTER |
//! | `gyld.file` | `<stream>/<file>` | a `{path, digest, bytes}` POINTER |
//!
//! Lens files are the large ones, so they travel as pointer plus digest and are
//! fetched over HTTP from grazel's static path (owner ruling O5): grazel serves
//! the bundle root under the same prefix the supplier's `--static-base` names,
//! `/gyld` on both sides. The digest is checked by the consumer, never trusted.
//!
//! [`STREAM_FILES`] travel the same way. A glade root is otherwise the one place
//! the records cannot be read at all: `projection.json` is the record set every
//! Gyld record window renders, and before it was published a glade root could
//! only reach it by having the build directory added as a static root by hand.
//!
//! [`publications`] is the whole decision and it is a pure function of the
//! emitted directory: it reads the bundle and returns the ops to append. A
//! document over [`MAX_VALUE_BYTES`] is SKIPPED with a note rather than pushed
//! onto a share; it is on the static path like any other large file.

use std::path::Path;

use crate::bundle::{file_pointer, Layout};

/// The largest document published as a value. Everything emitted today is well
/// under it (`streams.json` is tens of kilobytes); the bound is here so a
/// surprising bundle cannot push a megabyte through the fold.
pub const MAX_VALUE_BYTES: u64 = 256 * 1024;

/// The publication surfaces and the static base the lens pointers are written
/// against.
#[derive(Clone, Debug, PartialEq)]
pub struct Surfaces {
    pub streams_id: String,
    pub stream_id: String,
    pub decisions_id: String,
    pub lens_id: String,
    pub file_id: String,
    pub static_base: String,
}

pub const DEFAULT_STREAMS_ID: &str = "gyld.streams";
pub const DEFAULT_STREAM_ID: &str = "gyld.stream";
pub const DEFAULT_DECISIONS_ID: &str = "gyld.decisions";
pub const DEFAULT_LENS_ID: &str = "gyld.lens";
pub const DEFAULT_FILE_ID: &str = "gyld.file";
/// The URL prefix grazel serves the bundle root under.
pub const DEFAULT_STATIC_BASE: &str = "/gyld";

/// The per-stream documents published as pointers on the `gyld.file` surface,
/// in the order they are published. They are the ones a consumer needs whole
/// and unsummarised: the record set, and the validation report over it.
pub const STREAM_FILES: [&str; 2] = ["projection.json", "validation.json"];

impl Default for Surfaces {
    fn default() -> Surfaces {
        Surfaces {
            streams_id: DEFAULT_STREAMS_ID.into(),
            stream_id: DEFAULT_STREAM_ID.into(),
            decisions_id: DEFAULT_DECISIONS_ID.into(),
            lens_id: DEFAULT_LENS_ID.into(),
            file_id: DEFAULT_FILE_ID.into(),
            static_base: DEFAULT_STATIC_BASE.into(),
        }
    }
}

/// One value op to append: a surface, an optional key, and the bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Publication {
    pub glade_id: String,
    pub key: Option<String>,
    pub payload: Vec<u8>,
}

/// What one publication pass produced, plus anything it could not publish. The
/// notes are logged, never fatal: a build that succeeded stays a build that
/// succeeded even if one document is unreadable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub publications: Vec<Publication>,
    pub notes: Vec<String>,
    /// How many streams the build's listing named — the census figure, which is
    /// what the supplier's `published …` log line reports. It counts the
    /// LISTING, not the documents found for it: a stream the bundle lists but
    /// holds no directory for is still a stream the census shows.
    pub streams: usize,
}

/// Read an emitted bundle and decide everything to publish for it.
///
/// The stream listing is the authority for which streams exist: a stream the
/// bundle does not list is not published, even if a directory for it is lying
/// around.
pub fn publications(layout: &Layout, output_dir: &Path, surfaces: &Surfaces) -> Plan {
    let mut plan = Plan::default();
    let listing = output_dir.join("streams.json");
    let bytes = match read_value(&listing) {
        Ok(b) => b,
        Err(note) => {
            plan.notes.push(note);
            return plan;
        }
    };
    let ids = stream_ids(&bytes);
    plan.streams = ids.len();
    plan.publications.push(Publication {
        glade_id: surfaces.streams_id.clone(),
        key: None,
        payload: bytes,
    });

    for id in ids {
        let dir = output_dir.join("streams").join(&id);
        for (surface, file) in [
            (&surfaces.stream_id, "stream.json"),
            (&surfaces.decisions_id, "decide-now.json"),
        ] {
            let path = dir.join(file);
            if !path.is_file() {
                // A lineage that is not a decision stream carries no
                // decide-now list; that is data, not a fault.
                continue;
            }
            match read_value(&path) {
                Ok(payload) => plan.publications.push(Publication {
                    glade_id: surface.clone(),
                    key: Some(id.clone()),
                    payload,
                }),
                Err(note) => plan.notes.push(note),
            }
        }

        for file in STREAM_FILES {
            let path = dir.join(file);
            if !path.is_file() {
                // A stream built before the file existed, or a lineage that
                // carries no validation report: data, not a fault, as with
                // decide-now.json above.
                continue;
            }
            push_pointer(
                &mut plan,
                layout,
                surfaces,
                &surfaces.file_id,
                format!("{id}/{file}"),
                &path,
            );
        }

        for (perspective, path) in lens_files(&dir.join("lenses")) {
            push_pointer(
                &mut plan,
                layout,
                surfaces,
                &surfaces.lens_id,
                format!("{id}/{perspective}"),
                &path,
            );
        }
    }
    plan
}

/// Publish one file as a `{path, digest, bytes}` pointer onto `glade_id`, or
/// note why it could not be. Pointers never carry the bytes, so the value
/// budget does not apply to them.
fn push_pointer(
    plan: &mut Plan,
    layout: &Layout,
    surfaces: &Surfaces,
    glade_id: &str,
    key: String,
    path: &Path,
) {
    match file_pointer(layout, path, &surfaces.static_base) {
        Ok(pointer) => {
            let payload = serde_json::to_vec(&pointer).unwrap_or_default();
            plan.publications.push(Publication {
                glade_id: glade_id.to_string(),
                key: Some(key),
                payload,
            });
        }
        Err(e) => plan.notes.push(format!("pointer {}: {e}", path.display())),
    }
}

/// The stream ids a `gyld.streams.v1` listing carries, in the order it lists
/// them. A listing that does not parse yields none: the bundle is still
/// published, and the note says the rest could not be read.
fn stream_ids(listing: &[u8]) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_slice(listing) {
        Ok(v) => v,
        Err(_) => {
            return Vec::new();
        }
    };
    let streams = match value.get("streams").and_then(|v| v.as_array()) {
        Some(s) => s,
        None => {
            return Vec::new();
        }
    };
    streams
        .iter()
        .filter_map(|s| s.get("id").and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect()
}

/// The `<perspective>.lens.json` files of one stream, sorted, with the
/// perspective id each belongs to.
fn lens_files(lenses: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut found: Vec<(String, std::path::PathBuf)> = match std::fs::read_dir(lenses) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter_map(|p| {
                let name = p.file_name()?.to_str()?;
                let perspective = name.strip_suffix(".lens.json")?;
                Some((perspective.to_string(), p.clone()))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found
}

/// Read a document destined for a value surface, bounded.
fn read_value(path: &Path) -> Result<Vec<u8>, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "{} is {} bytes; over the {MAX_VALUE_BYTES} value budget, so it stays on the static path",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bundle(tag: &str) -> (Layout, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("glade-gyld-pub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let out = root.join("builds/build-1");
        let lenses = out.join("streams/base/lenses");
        std::fs::create_dir_all(&lenses).unwrap();
        std::fs::write(
            out.join("streams.json"),
            br#"{"format":"gyld.streams.v1","streams":[{"id":"base"},{"id":"stream-a"}]}"#,
        )
        .unwrap();
        std::fs::write(out.join("streams/base/stream.json"), br#"{"id":"base"}"#).unwrap();
        std::fs::write(
            out.join("streams/base/decide-now.json"),
            br#"{"questions":[]}"#,
        )
        .unwrap();
        std::fs::write(lenses.join("decisions.lens.json"), b"abc").unwrap();
        std::fs::write(lenses.join("branch.lens.json"), b"abc").unwrap();
        std::fs::write(lenses.join("decisions.svg"), b"<svg/>").unwrap();
        (Layout::new(root.join("gyld"), root.clone()), out)
    }

    #[test]
    fn every_surface_is_published_with_its_key() {
        let (layout, out) = bundle("all");
        let plan = publications(&layout, &out, &Surfaces::default());
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let keys: Vec<(String, Option<String>)> = plan
            .publications
            .iter()
            .map(|p| (p.glade_id.clone(), p.key.clone()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("gyld.streams".into(), None),
                ("gyld.stream".into(), Some("base".into())),
                ("gyld.decisions".into(), Some("base".into())),
                ("gyld.lens".into(), Some("base/branch".into())),
                ("gyld.lens".into(), Some("base/decisions".into())),
            ],
            "a stream the bundle lists but has no directory for publishes nothing, \
             and a stream with no projection or validation file publishes neither"
        );
        assert_eq!(
            plan.streams, 2,
            "the census figure is what the LISTING names, directory or not"
        );
        let _ = std::fs::remove_dir_all(layout.bundle_root);
    }

    #[test]
    fn a_lens_publication_is_a_pointer_not_the_file() {
        let (layout, out) = bundle("ptr");
        let plan = publications(&layout, &out, &Surfaces::default());
        let lens = plan
            .publications
            .iter()
            .find(|p| p.key.as_deref() == Some("base/decisions"))
            .expect("a lens publication");
        let pointer: crate::bundle::FilePointer = serde_json::from_slice(&lens.payload).unwrap();
        assert_eq!(
            pointer.path,
            "/gyld/builds/build-1/streams/base/lenses/decisions.lens.json"
        );
        assert_eq!(pointer.bytes, 3);
        assert_eq!(
            pointer.digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(layout.bundle_root);
    }

    #[test]
    fn projection_and_validation_travel_as_pointers_on_the_file_surface() {
        let (layout, out) = bundle("file");
        let dir = out.join("streams/base");
        std::fs::write(dir.join("projection.json"), b"abc").unwrap();
        std::fs::write(dir.join("validation.json"), br#"{"ok":true}"#).unwrap();

        let plan = publications(&layout, &out, &Surfaces::default());
        assert!(plan.notes.is_empty(), "{:?}", plan.notes);

        let files: Vec<Option<String>> = plan
            .publications
            .iter()
            .filter(|p| p.glade_id == "gyld.file")
            .map(|p| p.key.clone())
            .collect();
        assert_eq!(
            files,
            vec![
                Some("base/projection.json".into()),
                Some("base/validation.json".into())
            ],
            "each listed stream that has them publishes both, keyed <stream>/<file>"
        );

        let projection = plan
            .publications
            .iter()
            .find(|p| p.key.as_deref() == Some("base/projection.json"))
            .expect("a projection publication");
        let pointer: crate::bundle::FilePointer =
            serde_json::from_slice(&projection.payload).unwrap();
        assert_eq!(
            pointer.path, "/gyld/builds/build-1/streams/base/projection.json",
            "a pointer, not the records: fetched over the static path and checked"
        );
        assert_eq!(pointer.bytes, 3);
        assert_eq!(
            pointer.digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(layout.bundle_root);
    }

    #[test]
    fn a_document_over_the_value_budget_is_noted_not_published() {
        let (layout, out) = bundle("big");
        std::fs::write(
            out.join("streams/base/stream.json"),
            "x".repeat(MAX_VALUE_BYTES as usize + 1),
        )
        .unwrap();
        let plan = publications(&layout, &out, &Surfaces::default());
        assert!(
            !plan
                .publications
                .iter()
                .any(|p| p.glade_id == "gyld.stream"),
            "the oversize document stayed off the share"
        );
        assert!(
            plan.notes.iter().any(|n| n.contains("value budget")),
            "{:?}",
            plan.notes
        );
        // and the rest of the bundle still published.
        assert!(plan
            .publications
            .iter()
            .any(|p| p.glade_id == "gyld.decisions"));
        let _ = std::fs::remove_dir_all(layout.bundle_root);
    }

    #[test]
    fn a_missing_or_unreadable_listing_publishes_nothing_and_says_so() {
        let root = std::env::temp_dir().join(format!("glade-gyld-pub-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let layout = Layout::new(root.join("gyld"), root.clone());
        let plan = publications(&layout, &root.join("builds/gone"), &Surfaces::default());
        assert!(plan.publications.is_empty());
        assert_eq!(plan.streams, 0, "no listing, no census figure");
        assert!(
            plan.notes.len() == 1 && plan.notes[0].contains("cannot read"),
            "{:?}",
            plan.notes
        );

        // A listing that is not a listing still publishes the listing itself:
        // the consumer sees what the build wrote and nothing is invented.
        let out = root.join("builds/odd");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("streams.json"), b"not json").unwrap();
        let plan = publications(&layout, &out, &Surfaces::default());
        assert_eq!(plan.publications.len(), 1);
        assert_eq!(plan.publications[0].glade_id, "gyld.streams");
        let _ = std::fs::remove_dir_all(&root);
    }
}
