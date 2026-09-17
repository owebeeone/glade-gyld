//! `search_sources`: a plain-text search over the corpus this build was
//! grounded in (GyldAskAgent.md phase C, the local half).
//!
//! [`crate::toolset::READ_SOURCE`] answers about a tag the reader, or the
//! index, already NAMED. That is the whole of retrieval up to here, and it has
//! a plain ceiling: a reader who asks *what do the sources say about rotation*
//! names no tag at all, and the honest answer has been *I was not given that*.
//! This tool is the one that can look.
//!
//! **Two corpora, one pass.**
//!
//! * The documents the build's `sources.json` LISTS, under the root that index
//!   recorded — the same document list and the same root
//!   [`crate::toolset::READ_SOURCE`] resolves a citation through. It is the
//!   first thing in this supplier that opens one of those documents; what it
//!   may open is still only what the index named.
//! * The build's own records: the question labels and ruling texts of every
//!   census stream's `decide-now.json`, and the labels and descriptions of its
//!   `projection.json`. A hit there names the stream and the slot, so the next
//!   call is a `gyld_query` rather than another guess.
//!
//! **No path a model chose ever reaches the filesystem.** The input is a query
//! and a limit; every file read is one the index or the census named, each
//! checked for containment under the root it was named relative to and bounded
//! in size. The strongest thing a question can do here is fail to match.
//!
//! **No index is built.** A linear pass over nine documents and three streams
//! is milliseconds, and the per-call clock of [`crate::tools::ToolBudgets`] is
//! the backstop. Building and invalidating a real one is a different feature
//! with a different design, and this build is nowhere near needing it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::bundle;
use crate::sources::{self, SourceRoot};
use crate::tools::{Tool, ToolContext, ToolOutput, ToolRefusal};
use crate::toolset::{array, text};
use crate::verbs::valid_stream_id;

/// The tool that searches the build's documents and records.
pub const SEARCH_SOURCES: &str = "search_sources";

/// Hits returned when the caller names no `limit`.
pub const DEFAULT_HITS: usize = 10;

/// The most hits one call returns, whatever it asked for. A search that handed
/// back a hundred lines would spend the per-result byte budget on material the
/// model then has to read twice; the count of what MATCHED is reported either
/// way, so a narrowing query is the answer to a broad one.
pub const MAX_HITS: usize = 25;

/// The largest document this tool reads into memory. The cited documents are
/// tens of kilobytes of markdown; the bound is here so a surprising entry in an
/// index cannot be read whole.
pub const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// The longest query this tool accepts, in characters.
pub const MAX_QUERY: usize = 200;

/// The most words one query may carry. Every one of them has to appear on a
/// line, so a query past this is a sentence rather than a search.
pub const MAX_WORDS: usize = 12;

/// The characters of the matched line one hit shows.
pub const EXCERPT: usize = 160;

/// The searcher, over one build and the root its index recorded.
pub struct SearchSources {
    context: ToolContext,
}

impl SearchSources {
    /// The TOOL, never the struct — a searcher nobody put in a registry is a
    /// reader with no budget behind it.
    pub fn offering(context: &ToolContext) -> Arc<dyn Tool> {
        Arc::new(SearchSources {
            context: context.clone(),
        })
    }
}

impl Tool for SearchSources {
    fn name(&self) -> &str {
        SEARCH_SOURCES
    }

    fn schema(&self) -> Value {
        json!({
            "name": SEARCH_SOURCES,
            "description": "\
        Search THIS build's source documents and its own records for a plain-text \
        phrase. Call it when the reader asks what the sources say about something the \
        context above names no tag for. Matching is case-insensitive and by substring, \
        and a line matches only when EVERY word of the query is on it, so two words \
        narrow rather than widen. Each hit names the document (or the stream and slot), \
        the heading it sits under, the line and one line of text around the match — \
        enough to call `read_source` with that document and heading, or `gyld_query` \
        with that stream and slot, for the passage itself.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The words to look for. Every one of them \
        must appear on a line for that line to be a hit.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many hits to return, 1 to 25. The \
        default is 10, and the answer always says how many lines matched in all.",
                    },
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        })
    }

    fn run(&self, input: &Value) -> Result<ToolOutput, ToolRefusal> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(ToolRefusal::says(
                "search_sources needs a `query`; it was given none",
            ));
        }
        if query.chars().count() > MAX_QUERY {
            return Err(ToolRefusal::says(format!(
                "that query is {} characters; search_sources takes at most {MAX_QUERY}",
                query.chars().count()
            )));
        }
        let words: Vec<Vec<char>> = query.split_whitespace().map(fold).collect();
        if words.len() > MAX_WORDS {
            return Err(ToolRefusal::says(format!(
                "that query carries {} words and every one of them has to be on a line; \
                 search_sources takes at most {MAX_WORDS}",
                words.len()
            )));
        }
        let (limit, asked) = limit_of(input);
        let mut scan = Scan {
            words,
            limit,
            matched: 0,
            hits: Vec::new(),
        };
        let mut said: Vec<String> = Vec::new();
        let documents = self.documents(&mut scan, &mut said);
        let records = self.records(&mut scan);
        Ok(ToolOutput::text(render(
            &query, &documents, &records, &said, &scan, limit, asked,
        )))
    }
}

impl SearchSources {
    /// Every document the index lists, under the root it recorded — and one
    /// line saying what was searched, or why nothing was.
    fn documents(&self, scan: &mut Scan, said: &mut Vec<String>) -> String {
        let index = match sources::read(&self.context.sources) {
            Ok(index) => index,
            Err(reason) => {
                return format!("documents: none searched — {reason}");
            }
        };
        let root = match self.root(&index.root) {
            Ok(root) => root,
            Err(reason) => {
                return format!("documents: none searched — {reason}");
            }
        };
        let mut searched = 0usize;
        for document in index.documents.iter() {
            let path = root.join(&document.path);
            // The index's own path, checked against the index's own root: a
            // `..` somebody wrote into a document entry is the one way a list
            // of names becomes a path, and it is refused here.
            if !bundle::contained(&root, &path) {
                said.push(format!(
                    "skipped: {} ({}) — it is outside the sources root, so it is not read",
                    document.id, document.path
                ));
                continue;
            }
            let held = match read_text(&path) {
                Ok(held) => held,
                Err(reason) => {
                    said.push(format!(
                        "skipped: {} ({}) — {reason}",
                        document.id, document.path
                    ));
                    continue;
                }
            };
            searched += 1;
            let place = format!("document {} ({})", document.id, document.path);
            let mut heading = String::new();
            for (at, line) in held.lines().enumerate() {
                if let Some(found) = atx(line) {
                    heading = found;
                }
                scan.offer(&place, said_heading(&heading), at as u64 + 1, line);
            }
        }
        format!(
            "documents: {searched} of the {} this build's index lists, under the sources root {:?}",
            index.documents.len(),
            empty_is(&index.root.name, &index.root.given)
        )
    }

    /// Where the documents this index named actually are.
    ///
    /// The index records the root as the emitting run SPELLED it, and says what
    /// a relative spelling was measured against. Both answers are directories
    /// this supplier already has — the Gyld checkout it runs the hosts out of,
    /// and the staging repository it points them at — so resolving is a join
    /// and never a search of the filesystem.
    fn root(&self, root: &SourceRoot) -> Result<PathBuf, String> {
        let given = root.given.trim();
        if given.is_empty() {
            return Err(
                "this build's index records no sources root, so it names no document \
                        this supplier can open"
                    .to_string(),
            );
        }
        let path = Path::new(given);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        match root.relative_to.as_deref().unwrap_or_default() {
            sources::RELATIVE_TO_CHECKOUT => Ok(self.context.checkout.join(path)),
            sources::RELATIVE_TO_REPOSITORY => Ok(self.context.repository.join(path)),
            "" => Err(format!(
                "this build's index records the sources root {given:?} without saying what it is \
                 relative to, so it cannot be resolved here"
            )),
            other => Err(format!(
                "this build's index measured its sources root against {other:?}, which this \
                 supplier does not know"
            )),
        }
    }

    /// The build's own records, across the streams its census lists.
    fn records(&self, scan: &mut Scan) -> String {
        let listed = match read_json(&self.context.build.join("streams.json")) {
            Ok(listed) => listed,
            Err(reason) => {
                return format!("records: none searched — {reason}");
            }
        };
        let mut searched = 0usize;
        for held in array(&listed, "streams").iter() {
            let stream = text(held, "id");
            // The planner's own guard, applied where the id came off a file
            // rather than off a request: it still reaches a path.
            if !valid_stream_id(&stream) {
                continue;
            }
            searched += 1;
            self.decide_now(&stream, scan);
            self.projection(&stream, scan);
        }
        format!(
            "records: the labels, descriptions and ruling texts of the {searched} streams this \
             build's census lists"
        )
    }

    /// One stream's rows and rulings: what a question is CALLED, and what a
    /// ruling says.
    fn decide_now(&self, stream: &str, scan: &mut Scan) {
        let held = match self.emitted(stream, "decide-now.json") {
            Some(held) => held,
            None => {
                return;
            }
        };
        for row in array(&held, "questions").iter() {
            let place = format!("record {stream} {}", text(row, "slot"));
            scan.lines(&place, "label", &text(row, "label"));
        }
        for ruling in array(&held, "rulings").iter() {
            let place = format!("record {stream} {}", text(ruling, "slot"));
            scan.lines(&place, "ruling text", &text(ruling, "text"));
        }
    }

    /// One stream's definitions: the label and the docstring the Gyld source
    /// declared, which is the only prose a record carries of its own.
    fn projection(&self, stream: &str, scan: &mut Scan) {
        let held = match self.emitted(stream, "projection.json") {
            Some(held) => held,
            None => {
                return;
            }
        };
        let definitions = match held.get("definitions").and_then(|v| v.as_object()) {
            Some(definitions) => definitions,
            None => {
                return;
            }
        };
        for (id, definition) in definitions.iter() {
            let slot = definition
                .pointer("/source/qualified_slot")
                .and_then(|v| v.as_str())
                .unwrap_or(id.as_str())
                .to_string();
            let place = format!("record {stream} {slot}");
            scan.lines(&place, "label", &text(definition, "label"));
            scan.lines(&place, "description", &text(definition, "description"));
        }
    }

    /// One emitted file of one stream, or nothing. Absence is ordinary here —
    /// a stream that emitted no decide-now list is a stream with no rows, not a
    /// failed search.
    fn emitted(&self, stream: &str, name: &str) -> Option<Value> {
        let path = self.context.build.join("streams").join(stream).join(name);
        if !bundle::contained(&self.context.build, &path) {
            return None;
        }
        read_json(&path).ok()
    }
}

/// One line that carried every word of the query.
#[derive(Debug, Clone, PartialEq)]
struct Hit {
    /// `document <id> (<path>)`, or `record <stream> <slot>`.
    place: String,
    /// The markdown heading a document hit sits under, or the field a record
    /// hit was found in.
    under: String,
    line: u64,
    excerpt: String,
}

/// The pass itself: what is being looked for, what has been found, and how many
/// of the finds fit.
struct Scan {
    words: Vec<Vec<char>>,
    limit: usize,
    /// Every line that matched, INCLUDING the ones past the limit. The count is
    /// the difference between "there is nothing" and "there is more than you
    /// asked for", and a reader needs to be told which.
    matched: usize,
    hits: Vec<Hit>,
}

impl Scan {
    /// Offer one line of one place, under one heading.
    fn offer(&mut self, place: &str, under: &str, line: u64, held: &str) {
        let flat: Vec<char> = flatten(held);
        let folded = fold_chars(&flat);
        let mut first = usize::MAX;
        for word in self.words.iter() {
            match find(&folded, word) {
                Some(at) => {
                    first = first.min(at);
                }
                // Every word, or no hit: two words narrow a search rather than
                // widening it, which is what a reader typing two means.
                None => {
                    return;
                }
            }
        }
        self.matched += 1;
        if self.hits.len() >= self.limit {
            return;
        }
        self.hits.push(Hit {
            place: place.to_string(),
            under: under.to_string(),
            line,
            excerpt: excerpt(&flat, first.min(flat.len())),
        });
    }

    /// Offer every line of a field that may carry several.
    fn lines(&mut self, place: &str, under: &str, held: &str) {
        for (at, line) in held.lines().enumerate() {
            self.offer(place, under, at as u64 + 1, line);
        }
    }
}

/// The whole answer: what was asked, what was searched, what was skipped, and
/// the hits.
fn render(
    query: &str,
    documents: &str,
    records: &str,
    said: &[String],
    scan: &Scan,
    limit: usize,
    asked: Option<usize>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "query: {query:?} (a line is a hit only when every word of the query is on it, \
         case-insensitively)\n"
    ));
    out.push_str(documents);
    out.push('\n');
    out.push_str(records);
    out.push('\n');
    for note in said.iter() {
        out.push_str(note);
        out.push('\n');
    }
    if let Some(asked) = asked {
        out.push_str(&format!(
            "limit: {asked} was asked for and {limit} is this tool's most\n"
        ));
    }
    if scan.matched == 0 {
        out.push_str(
            "hits: none — no line of those documents and no record of those streams carries \
             every word\n",
        );
        return out;
    }
    if scan.matched > scan.hits.len() {
        out.push_str(&format!(
            "hits: {} of the {} lines that matched; raise `limit` (up to {MAX_HITS}) or narrow \
             the query with another word\n",
            scan.hits.len(),
            scan.matched
        ));
    } else {
        out.push_str(&format!("hits: {}\n", scan.matched));
    }
    for hit in scan.hits.iter() {
        out.push_str(&format!(
            "\n{} line {}, under {:?}\n  | {}\n",
            hit.place, hit.line, hit.under, hit.excerpt
        ));
    }
    out
}

/// The limit this call runs under, and what it asked for when that was not it.
fn limit_of(input: &Value) -> (usize, Option<usize>) {
    let asked = match input.get("limit").and_then(|v| v.as_u64()) {
        Some(asked) => asked as usize,
        None => {
            return (DEFAULT_HITS, None);
        }
    };
    let held = asked.clamp(1, MAX_HITS);
    if held == asked {
        return (held, None);
    }
    (held, Some(asked))
}

/// A markdown ATX heading's text, when this line is one.
fn atx(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim().to_string())
}

/// What a hit before the document's first heading says it sits under.
fn said_heading(heading: &str) -> &str {
    if heading.is_empty() {
        "no heading yet"
    } else {
        heading
    }
}

/// One line with its whitespace runs collapsed, as characters — so a padded
/// table row reads as a sentence and costs the budget what it reads.
fn flatten(line: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(line.len());
    let mut space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// A string folded for comparison: lowercase, ONE character for one.
///
/// The one-for-one fold is deliberate. A `char::to_lowercase` that yields two
/// characters — the dotted capital I is the one that does — would move every
/// index after it, and an excerpt window is computed on these indices. This is
/// a plain-text search and not a collation, so a fold that keeps the positions
/// honest is worth more than the two letters it rounds off.
fn fold(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn fold_chars(chars: &[char]) -> Vec<char> {
    chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect()
}

/// Where `needle` first appears in `haystack`, both already folded.
fn find(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

/// One line of text around the match, bounded.
fn excerpt(chars: &[char], at: usize) -> String {
    if chars.len() <= EXCERPT {
        return chars.iter().collect();
    }
    let start = at
        .saturating_sub(EXCERPT / 2)
        .min(chars.len().saturating_sub(EXCERPT));
    let end = (start + EXCERPT).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// One document, bounded, as a reason rather than an error.
fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read it: {e}"))?;
    if meta.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "it is {} bytes, and this tool reads at most {MAX_DOCUMENT_BYTES}",
            meta.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read it: {e}"))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} did not decode: {e}", path.display()))
}

fn empty_is<'a>(value: &'a str, absent: &'a str) -> &'a str {
    if value.trim().is_empty() {
        absent
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document under a sources root, a build beside it, and a checkout the
    /// root is measured from — the three directories a real desk has, small
    /// enough to assert line by line.
    const KEY_ROTATION: &str = "\
# KeyRotation

## 1. Custody

The root key is held by the owner.

## 2. Rotation

Key rotation happens yearly and is manual.
A second line mentioning ROTATION in capitals.
Nothing here about it.
";

    fn fixture(tag: &str) -> (PathBuf, ToolContext) {
        let root =
            std::env::temp_dir().join(format!("glade-gyld-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let build = root.join("bundle/builds/build-1");
        std::fs::create_dir_all(build.join("streams/base")).unwrap();
        std::fs::create_dir_all(root.join("gyld")).unwrap();
        std::fs::create_dir_all(root.join("glade-wz/dev-docs")).unwrap();
        std::fs::write(root.join("glade-wz/dev-docs/KeyRotation.md"), KEY_ROTATION).unwrap();
        // A document the sources root does not hold, and one whose path climbs
        // out of it. Both are entries an index can carry and neither is read.
        write(
            &build.join("sources.json"),
            &json!({
                "format": crate::sources::SOURCES_FORMAT,
                "written": "2026-09-17T09:00:00Z",
                "root": {"option": "--sources-root", "chosen": "default",
                         "given": "../glade-wz", "relative_to": "checkout",
                         "name": "glade-wz", "found": true},
                "documents": [
                    {"id": "KeyRotation", "path": "dev-docs/KeyRotation.md",
                     "keys": ["heading-number"], "found": true},
                    {"id": "Missing", "path": "dev-docs/Missing.md", "keys": [], "found": false},
                    {"id": "Escape", "path": "../../etc/passwd", "keys": [], "found": false}
                ],
                "tags": [], "cited_by": [], "unresolved": []
            }),
        );
        write(
            &build.join("streams.json"),
            &json!({
                "format": "gyld.streams.v1",
                "streams": [
                    {"id": "base", "kind": "fork", "questions": [{"slot": "s1"}]},
                    {"id": "stream-a", "kind": "link", "questions": []}
                ]
            }),
        );
        write(
            &build.join("streams/base/decide-now.json"),
            &json!({
                "format": "gyld.decide-now.v1",
                "stream": "base",
                "questions": [
                    {"slot": "glade_decisions:GladeDecisions.key_rotation",
                     "label": "key_rotation"},
                    {"slot": "glade_decisions:GladeDecisions.key_custody", "label": "key_custody"}
                ],
                "rulings": [
                    {"slot": "glade_decisions_base:Base.key_rotation_ruling",
                     "decides": "glade_decisions:GladeDecisions.key_rotation",
                     "text": "2026-09-13, owner: yearly rotation of the root key."}
                ]
            }),
        );
        write(
            &build.join("streams/base/projection.json"),
            &json!({
                "format": "gyld.projection.v1",
                "occurrences": [],
                "definitions": {
                    "def1": {
                        "kind": "entity", "label": "KeyRotation",
                        "description": "Root key rotation and recovery posture.\nA second line.",
                        "source": {"qualified_slot": "glade_decisions:KeyRotation"}
                    }
                }
            }),
        );
        let context = ToolContext {
            sources: build.join("sources.json"),
            build,
            checkout: root.join("gyld"),
            repository: root.join("bundle/stage"),
        };
        (root, context)
    }

    fn write(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn search(context: &ToolContext, input: Value) -> Result<String, String> {
        SearchSources::offering(context)
            .run(&input)
            .map(|output| output.text)
            .map_err(|refusal| refusal.reason)
    }

    /// The `place line N, under "H"` headers of an answer, in order.
    fn places(said: &str) -> Vec<String> {
        said.lines()
            .filter(|line| line.starts_with("document ") || line.starts_with("record "))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_hit_in_a_document_names_the_document_the_heading_and_the_line() {
        let (root, context) = fixture("document");
        let said = search(&context, json!({"query": "rotation"})).expect("hits");
        assert!(
            said.contains(
                "document KeyRotation (dev-docs/KeyRotation.md) line 9, under \"2. Rotation\""
            ),
            "{said}"
        );
        assert!(
            said.contains("  | Key rotation happens yearly and is manual."),
            "one line of text around the match: {said}"
        );
        // The first line sits under the heading it IS, and a hit before any
        // heading would say so rather than claiming one.
        assert!(
            said.contains(
                "document KeyRotation (dev-docs/KeyRotation.md) line 1, under \"KeyRotation\""
            ),
            "{said}"
        );
        // Case-insensitive, both ways.
        assert!(
            said.contains("line 10, under \"2. Rotation\""),
            "ROTATION in capitals is the same word: {said}"
        );
        assert!(
            said.contains(
                "documents: 1 of the 3 this build's index lists, under the sources \
                           root \"glade-wz\""
            ),
            "{said}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_hit_in_a_record_names_the_stream_the_slot_and_the_field() {
        let (root, context) = fixture("record");
        let said = search(&context, json!({"query": "rotation"})).expect("hits");
        assert!(
            said.contains(
                "record base glade_decisions:GladeDecisions.key_rotation line 1, under \"label\""
            ),
            "the question's own label: {said}"
        );
        assert!(
            said.contains(
                "record base glade_decisions_base:Base.key_rotation_ruling line 1, under \
                 \"ruling text\""
            ),
            "the ruling's text: {said}"
        );
        assert!(
            said.contains("  | 2026-09-13, owner: yearly rotation of the root key."),
            "{said}"
        );
        assert!(
            said.contains("record base glade_decisions:KeyRotation line 1, under \"description\""),
            "the definition's docstring, under the slot the projection declares: {said}"
        );
        assert!(
            said.contains(
                "records: the labels, descriptions and ruling texts of the 2 streams this \
                 build's census lists"
            ),
            "a stream that emitted nothing is still a stream that was asked: {said}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_word_of_the_query_has_to_be_on_the_line() {
        let (root, context) = fixture("every-word");
        let broad = search(&context, json!({"query": "rotation"})).expect("hits");
        assert!(broad.contains("hits: 8\n"), "{broad}");

        let narrowed = search(&context, json!({"query": "key rotation"})).expect("hits");
        assert!(
            narrowed.contains("hits: 6\n"),
            "a second word narrows rather than widens: {narrowed}"
        );
        // The line that carries only one of the two words is gone, and the one
        // that carries both in one word is not.
        assert!(
            broad.contains("line 10,") && !narrowed.contains("line 10,"),
            "ROTATION with no `key` on the line: {narrowed}"
        );
        assert!(
            narrowed.contains("line 1, under \"KeyRotation\""),
            "substring, so one word may carry two: {narrowed}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_limit_bounds_what_is_shown_and_never_what_is_counted() {
        let (root, context) = fixture("limit");
        let two = search(&context, json!({"query": "rotation", "limit": 2})).expect("hits");
        assert_eq!(places(&two).len(), 2, "{two}");
        assert!(
            two.contains("hits: 2 of the 8 lines that matched"),
            "what was NOT shown is said, so a reader can widen or narrow: {two}"
        );
        assert!(two.contains("raise `limit` (up to 25)"), "{two}");

        // The default is ten, and eight fit inside it without a word about it.
        let held = search(&context, json!({"query": "rotation"})).expect("hits");
        assert_eq!(places(&held).len(), 8);
        assert!(!held.contains("lines that matched"), "{held}");

        // A limit past the most is CLAMPED and said, not refused.
        let greedy = search(&context, json!({"query": "rotation", "limit": 99})).expect("hits");
        assert!(
            greedy.contains("limit: 99 was asked for and 25 is this tool's most"),
            "{greedy}"
        );
        assert_eq!(places(&greedy).len(), 8);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nothing_found_is_an_answer_and_not_a_refusal() {
        let (root, context) = fixture("empty");
        let said = search(&context, json!({"query": "zebra"})).expect("an answer, not a refusal");
        assert!(
            said.contains(
                "hits: none — no line of those documents and no record of those streams \
                 carries every word"
            ),
            "{said}"
        );
        // And it still says what WAS searched, so the model can tell an empty
        // corpus from an empty result.
        assert!(said.contains("documents: 1 of the 3"), "{said}");
        assert!(said.contains("records: the labels"), "{said}");
        assert!(places(&said).is_empty(), "{said}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_document_that_cannot_be_read_is_skipped_and_named() {
        let (root, context) = fixture("skipped");
        let said = search(&context, json!({"query": "rotation"})).expect("hits");
        assert!(
            said.contains("skipped: Missing (dev-docs/Missing.md) — cannot read it:"),
            "the one that is not there is named, never quietly dropped: {said}"
        );
        // And an entry whose path climbs out of the root is refused before it
        // is a path at all.
        assert!(
            said.contains(
                "skipped: Escape (../../etc/passwd) — it is outside the sources root, so it is \
                 not read"
            ),
            "{said}"
        );
        // The searchable document was still searched.
        assert!(said.contains("hits: 8"), "{said}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_build_whose_index_or_census_is_absent_says_so_and_searches_the_other_half() {
        let (root, context) = fixture("half");
        std::fs::remove_file(&context.sources).unwrap();
        let said = search(&context, json!({"query": "rotation"})).expect("the records half");
        assert!(
            said.contains("documents: none searched — cannot read the source index"),
            "{said}"
        );
        assert!(said.contains("hits: 4"), "the four record hits: {said}");

        std::fs::remove_file(context.build.join("streams.json")).unwrap();
        let neither = search(&context, json!({"query": "rotation"})).expect("neither half");
        assert!(
            neither.contains("records: none searched — cannot read"),
            "{neither}"
        );
        assert!(neither.contains("hits: none"), "{neither}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_query_this_tool_will_not_run_is_refused_as_data() {
        let (root, context) = fixture("refusals");
        let e = search(&context, json!({})).expect_err("no query");
        assert!(e.contains("needs a `query`"), "{e}");
        let e = search(&context, json!({"query": "   "})).expect_err("a blank query");
        assert!(e.contains("needs a `query`"), "{e}");
        let e =
            search(&context, json!({"query": "x".repeat(MAX_QUERY + 1)})).expect_err("too long");
        assert!(e.contains("at most 200"), "{e}");
        let e = search(&context, json!({"query": "a b c d e f g h i j k l m"}))
            .expect_err("too many words");
        assert!(e.contains("at most 12"), "{e}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_root_the_index_did_not_say_how_to_measure_is_named_rather_than_guessed() {
        let (root, context) = fixture("root");
        let held = SearchSources {
            context: context.clone(),
        };
        // Absolute wins outright, and neither of the supplier's directories is
        // consulted.
        let absolute = held
            .root(&SourceRoot {
                given: "/somewhere/else".into(),
                ..Default::default()
            })
            .expect("an absolute root");
        assert_eq!(absolute, PathBuf::from("/somewhere/else"));

        assert_eq!(
            held.root(&SourceRoot {
                given: "../glade-wz".into(),
                relative_to: Some(sources::RELATIVE_TO_CHECKOUT.into()),
                ..Default::default()
            })
            .expect("the checkout"),
            root.join("gyld").join("../glade-wz")
        );
        assert_eq!(
            held.root(&SourceRoot {
                given: "docs".into(),
                relative_to: Some(sources::RELATIVE_TO_REPOSITORY.into()),
                ..Default::default()
            })
            .expect("the repository"),
            root.join("bundle/stage").join("docs")
        );

        let e = held
            .root(&SourceRoot::default())
            .expect_err("no root at all");
        assert!(e.contains("records no sources root"), "{e}");
        let e = held
            .root(&SourceRoot {
                given: "docs".into(),
                ..Default::default()
            })
            .expect_err("relative to nothing");
        assert!(e.contains("without saying what it is relative to"), "{e}");
        let e = held
            .root(&SourceRoot {
                given: "docs".into(),
                relative_to: Some("the-moon".into()),
                ..Default::default()
            })
            .expect_err("relative to something else");
        assert!(
            e.contains("measured its sources root against \"the-moon\""),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_excerpt_is_one_bounded_line_around_the_match() {
        let long: Vec<char> = format!("{}NEEDLE{}", "a".repeat(400), "b".repeat(400))
            .chars()
            .collect();
        let held = excerpt(&long, 400);
        assert_eq!(held.chars().count(), EXCERPT + 2, "the two ellipses");
        assert!(held.starts_with('…') && held.ends_with('…'), "{held}");
        assert!(
            held.contains("NEEDLE"),
            "the match is IN the window: {held}"
        );

        // A short line is itself, with no ellipsis either side.
        let short: Vec<char> = "a needle".chars().collect();
        assert_eq!(excerpt(&short, 2), "a needle");

        // And a padded table row reads as a sentence.
        assert_eq!(
            flatten("  | Q11 |   Key   custody |  buy |  ")
                .iter()
                .collect::<String>(),
            "| Q11 | Key custody | buy |"
        );
    }
}
