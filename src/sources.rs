//! The source index (`gyld.sources.v1`) and the resolver that grounds an
//! answer in it (GyldAskAgent.md section 5).
//!
//! A build emits `sources.json` beside `streams.json`: the documents it was
//! pointed at, every tag it could resolve to a table row or a numbered heading
//! with that row's or heading's own passage, `cited_by` — which record in which
//! stream cites which tags — and `unresolved`, the tags that resolve to nothing
//! and why.
//!
//! **The supplier never reads a cited document and never greps.** It takes the
//! index's entry WHOLE: document, path, heading, line range, passage, digest.
//! If the index carries no passage for a tag then there is no passage, and the
//! model is told exactly that. A tag's `path` is relative to the `root` the
//! emitting run recorded and is carried as PROVENANCE only — nothing here opens
//! it.
//!
//! An unresolved tag is SAID, never hidden (MDV-7): it travels into the prompt
//! with its reason, so the answer can say *this record cites `AZ-7` and this
//! build's index resolves it to nothing*. Hiding it would be an omission
//! invented by the supplier, which is exactly what rule 6.7 forbids.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ask::AskContext;

/// The format this reader accepts, and the only one.
pub const SOURCES_FORMAT: &str = "gyld.sources.v1";

/// The largest index the supplier reads. The emitted one is tens of kilobytes;
/// the bound is here so a surprising build cannot be read into memory whole.
pub const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;

/// The workzone the cited documents were read out of, as the emitting run
/// recorded it. Carried for provenance; never opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceRoot {
    #[serde(default)]
    pub option: String,
    #[serde(default)]
    pub given: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub found: bool,
}

/// One document the index was pointed at.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceDocument {
    pub id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub found: bool,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One tag the index resolved, with the passage it names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexedTag {
    pub tag: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub document: String,
    /// Relative to the recorded [`SourceRoot`].
    #[serde(default)]
    pub path: String,
    /// Which resolver matched: `table-row-id`, `heading-number`, `log-prose`.
    #[serde(default)]
    pub resolver: String,
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub lines: Vec<u64>,
    #[serde(default)]
    pub passage: String,
    #[serde(default)]
    pub digest: String,
    /// The emitter capped this passage: it is a prefix of the real one.
    #[serde(default)]
    pub truncated: bool,
}

/// One tag the index could not resolve, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedTag {
    pub tag: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub cited_by: Vec<String>,
}

/// One citation the emitted authoring modules carry: which record, in which
/// stream, cites which tags, through which field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CitedBy {
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub slot: String,
    /// `sources`, `matrix` or `ruling`.
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The whole index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceIndex {
    pub format: String,
    #[serde(default)]
    pub written: String,
    #[serde(default)]
    pub root: SourceRoot,
    #[serde(default)]
    pub documents: Vec<SourceDocument>,
    #[serde(default)]
    pub tags: Vec<IndexedTag>,
    #[serde(default)]
    pub cited_by: Vec<CitedBy>,
    #[serde(default)]
    pub unresolved: Vec<UnresolvedTag>,
}

/// One tag as the supplier resolved it: the index's own entry, or the reason
/// there is none. This is the only quotable material an answer ever gets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSource {
    pub tag: String,
    /// Which citation list named it: `record`, `ruling`, or the index's own
    /// field name when the envelope did not carry the tag at all.
    pub cites: String,
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub document: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lines: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub passage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub digest: Option<String>,
    /// The index says this passage is a prefix of the real one.
    #[serde(default)]
    pub truncated: bool,
    /// Why it resolved to nothing, when it did not.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

/// The reason a tag nothing in the index mentions gets. Named, because the
/// prompt and the `citation` record both say it.
pub const NOT_IN_INDEX: &str = "this build's index does not list this tag";

/// Read and check one `sources.json`. Absence, an unreadable file, an unknown
/// format and a file over the bound are each a readable refusal.
pub fn read(path: &Path) -> Result<SourceIndex, String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the source index {}: {e}", path.display()))?;
    if meta.len() > MAX_INDEX_BYTES {
        return Err(format!(
            "the source index {} is {} bytes; the limit is {MAX_INDEX_BYTES}",
            path.display(),
            meta.len()
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read the source index {}: {e}", path.display()))?;
    let index: SourceIndex = serde_json::from_slice(&bytes)
        .map_err(|e| format!("bad source index {}: {e}", path.display()))?;
    if index.format != SOURCES_FORMAT {
        return Err(format!(
            "the source index {} says {:?}; this supplier reads {SOURCES_FORMAT}",
            path.display(),
            index.format
        ));
    }
    Ok(index)
}

impl SourceIndex {
    /// The index's entry for one tag, if it has one.
    pub fn tag(&self, tag: &str) -> Option<&IndexedTag> {
        self.tags.iter().find(|held| held.tag == tag)
    }

    /// Every tag the index itself says this record cites, in the order it lists
    /// them, with the field that named each.
    ///
    /// The index is the only place a QUESTION's own `sources` and `matrix` tags
    /// are emitted at all (section 5), so this is not a second opinion about
    /// the envelope's list — it is the emitted half of it.
    pub fn cited(&self, stream: &str, slot: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for entry in self.cited_by.iter() {
            if entry.stream != stream || entry.slot != slot {
                continue;
            }
            for tag in entry.tags.iter() {
                let tag = tag.trim();
                if !tag.is_empty() && !out.iter().any(|(held, _)| held == tag) {
                    out.push((tag.to_string(), entry.field.clone()));
                }
            }
        }
        out
    }

    /// Resolve every tag this record and its ruling cite.
    ///
    /// The list is the envelope's tags first, in the order the page gave them,
    /// then the tags this build's own `cited_by` adds for the same record —
    /// united, never duplicated. Each one is looked up whole; a tag the index
    /// does not resolve passes through with the index's own reason, or with
    /// [`NOT_IN_INDEX`] when the index has never heard of it.
    pub fn resolve(&self, context: &AskContext) -> Vec<ResolvedSource> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for source in context.sources.iter() {
            let tag = source.tag.trim();
            if !tag.is_empty() && !wanted.iter().any(|(held, _)| held == tag) {
                wanted.push((tag.to_string(), source.cites.clone()));
            }
        }
        for (tag, field) in self.cited(&context.stream, &context.record.slot) {
            if !wanted.iter().any(|(held, _)| held == &tag) {
                wanted.push((tag, field));
            }
        }
        wanted
            .into_iter()
            .map(|(tag, cites)| self.one(&tag, &cites))
            .collect()
    }

    /// One tag, resolved or said to be unresolved.
    fn one(&self, tag: &str, cites: &str) -> ResolvedSource {
        if let Some(found) = self.tag(tag) {
            return ResolvedSource {
                tag: tag.to_string(),
                cites: cites.to_string(),
                resolved: true,
                document: Some(found.document.clone()),
                path: Some(found.path.clone()),
                heading: Some(found.heading.clone()),
                lines: Some(found.lines.clone()),
                passage: Some(found.passage.clone()),
                digest: Some(found.digest.clone()),
                truncated: found.truncated,
                reason: None,
            };
        }
        let reason = self
            .unresolved
            .iter()
            .find(|held| held.tag == tag)
            .map(|held| held.reason.clone())
            .unwrap_or_else(|| NOT_IN_INDEX.to_string());
        ResolvedSource {
            tag: tag.to_string(),
            cites: cites.to_string(),
            resolved: false,
            reason: Some(reason),
            ..Default::default()
        }
    }
}

/// How a resolution went, for the one log line a consultation prints.
pub fn counted(sources: &[ResolvedSource]) -> (usize, usize) {
    let resolved = sources.iter().filter(|s| s.resolved).count();
    (resolved, sources.len() - resolved)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fixture index in the shape the emitter writes, small enough to read
    /// and wide enough to cover a resolved row, a resolved heading, a capped
    /// passage, an unresolved tag with a reason and a tag nobody has heard of.
    pub(crate) fn index() -> SourceIndex {
        serde_json::from_value(serde_json::json!({
            "format": SOURCES_FORMAT,
            "written": "2026-09-16T10:37:26Z",
            "root": {
                "option": "--sources-root", "given": "../../glade-wz",
                "name": "glade-wz", "found": true
            },
            "documents": [
                {"id": "GladeBuyBuildMatrix", "path": "dev-docs/GladeBuyBuildMatrix.md",
                 "keys": ["table-row-id", "heading-number"], "found": true,
                 "digest": "901a33df", "reason": null}
            ],
            "tags": [
                {"tag": "Q11", "family": "matrix-row", "document": "GladeBuyBuildMatrix",
                 "path": "dev-docs/GladeBuyBuildMatrix.md", "resolver": "table-row-id",
                 "heading": "4. The questions", "lines": [158, 158],
                 "passage": "| Q11 | Key custody and recovery posture | buy |",
                 "digest": "d8eb379e", "truncated": false},
                {"tag": "IrohReview §11", "family": "heading", "document": "IrohReview",
                 "path": "glade/dev-docs/IrohReview.md", "resolver": "heading-number",
                 "heading": "11. Observations for glade", "lines": [201, 240],
                 "passage": "## 11. Observations for glade\n\nThe first is", "digest": "aa11",
                 "truncated": true}
            ],
            "cited_by": [
                {"stream": "base", "slot": "glade_decisions:GladeDecisions.key_custody",
                 "field": "matrix", "tags": ["Q11"]},
                {"stream": "base", "slot": "glade_decisions:GladeDecisions.key_custody",
                 "field": "sources", "tags": ["WD-1", "AZ-7", "SEC-55-D5"]}
            ],
            "unresolved": [
                {"tag": "AZ-7", "family": "row-id",
                 "reason": "no document in this index declares it",
                 "cited_by": ["glade_decisions:GladeDecisions.key_custody"]}
            ]
        }))
        .expect("the fixture index")
    }

    fn context() -> AskContext {
        AskContext::parse(Some(&crate::ask::tests::envelope())).expect("the fixture envelope")
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("glade-gyld-sources-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_resolved_tag_is_taken_from_the_index_whole() {
        let held = index().resolve(&context());
        let q11 = held.iter().find(|s| s.tag == "Q11").expect("Q11");
        assert!(q11.resolved);
        assert_eq!(q11.document.as_deref(), Some("GladeBuyBuildMatrix"));
        assert_eq!(
            q11.path.as_deref(),
            Some("dev-docs/GladeBuyBuildMatrix.md"),
            "the path is the index's own, relative to the recorded root"
        );
        assert_eq!(q11.heading.as_deref(), Some("4. The questions"));
        assert_eq!(q11.lines.as_deref(), Some([158u64, 158].as_slice()));
        assert!(q11.passage.as_deref().unwrap().contains("Key custody"));
        assert_eq!(q11.digest.as_deref(), Some("d8eb379e"));
        assert!(!q11.truncated && q11.reason.is_none());
    }

    #[test]
    fn an_unresolved_tag_passes_through_with_its_reason() {
        let held = index().resolve(&context());
        let az = held.iter().find(|s| s.tag == "AZ-7").expect("AZ-7");
        assert!(!az.resolved, "{az:?}");
        assert_eq!(
            az.reason.as_deref(),
            Some("no document in this index declares it"),
            "the index's OWN reason, not one the supplier made up"
        );
        assert!(az.passage.is_none() && az.digest.is_none());

        // And a tag nothing in the index mentions is said too, never dropped.
        let wd = held.iter().find(|s| s.tag == "WD-1").expect("WD-1");
        assert!(!wd.resolved);
        assert_eq!(wd.reason.as_deref(), Some(NOT_IN_INDEX));
    }

    #[test]
    fn the_index_adds_the_tags_only_it_emits_for_this_record() {
        let held = index().resolve(&context());
        let tags: Vec<&str> = held.iter().map(|s| s.tag.as_str()).collect();
        assert_eq!(
            tags,
            vec!["Q11", "AZ-7", "WD-1", "SEC-55-D5"],
            "the envelope's tags first, then the ones only `cited_by` carries"
        );
        let cites: Vec<&str> = held.iter().map(|s| s.cites.as_str()).collect();
        assert_eq!(cites, vec!["record", "record", "sources", "sources"]);
        assert_eq!(counted(&held), (1, 3));
    }

    #[test]
    fn a_capped_passage_says_it_is_one() {
        let index = index();
        let mut context = context();
        context.sources = vec![crate::ask::AskSource {
            tag: "IrohReview §11".into(),
            cites: "ruling".into(),
        }];
        context.record.slot = "no-citations".into();
        let held = index.resolve(&context);
        assert_eq!(held.len(), 1);
        assert!(held[0].resolved && held[0].truncated, "{held:?}");
    }

    #[test]
    fn reading_an_index_refuses_absence_and_a_wrong_format_as_data() {
        let dir = tmp("read");
        let path = dir.join("sources.json");
        let e = read(&path).unwrap_err();
        assert!(e.contains("cannot read the source index"), "{e}");

        std::fs::write(&path, b"not json").unwrap();
        let e = read(&path).unwrap_err();
        assert!(e.contains("bad source index"), "{e}");

        std::fs::write(&path, br#"{"format":"gyld.sources.v2"}"#).unwrap();
        let e = read(&path).unwrap_err();
        assert!(e.contains("this supplier reads gyld.sources.v1"), "{e}");

        std::fs::write(&path, serde_json::to_vec(&index()).unwrap()).unwrap();
        let held = read(&path).expect("the index");
        assert_eq!(held.tags.len(), 2);
        assert_eq!(held.root.name, "glade-wz");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_emitted_index_reads_and_resolves() {
        // The build this feature was designed against. Skipped loudly when the
        // Gyld checkout is not beside this workspace.
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../gyld-wz/gyld/artifacts/decision-streams-v7/sources.json");
        if !path.is_file() {
            eprintln!("SKIP: {} is absent", path.display());
            return;
        }
        let index = read(&path).expect("the emitted index");
        assert!(!index.tags.is_empty() && !index.cited_by.is_empty());
        let cited = index.cited("base", "glade_decisions:GladeDecisions.key_custody");
        assert!(
            cited.iter().any(|(tag, _)| tag == "Q11"),
            "key_custody cites Q11: {cited:?}"
        );
        assert!(
            index.tag("Q11").is_some_and(|t| !t.passage.is_empty()),
            "Q11 resolves to a passage"
        );
        // Which tags resolve is the EMITTING run's business — the documents it
        // was pointed at decide it — so this asserts the shape rather than a
        // list: every unresolved tag carries a reason and is absent from
        // `tags`, and the two lists never overlap.
        assert!(
            !index.unresolved.is_empty(),
            "the index says what it could not do"
        );
        for held in index.unresolved.iter() {
            assert!(!held.reason.is_empty(), "{held:?} must say why");
            assert!(index.tag(&held.tag).is_none(), "{held:?} is in both lists");
        }
    }
}
