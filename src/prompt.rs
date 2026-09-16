//! The prompt one consultation is made of (GyldAskAgent.md section 7).
//!
//! Two parts, and the split is the point:
//!
//! * [`Prompt::system`] is the STABLE prefix — the stance, the emitted facts of
//!   the envelope, and every resolved passage. It is what carries
//!   `cache_control`, so a follow-up on the same envelope reads the cache
//!   rather than paying for the passages again.
//! * [`Prompt::user`] is the volatile turn: what the reader typed, and nothing
//!   else.
//!
//! Composition is PURE — no clock, no filesystem, no network — so the whole
//! prompt is asserted as a golden over a fixture index.
//!
//! Every fact in it is a fact something EMITTED. An absence is written as an
//! absence: a stream that emitted no decide-now list says so, a record that
//! list carries no row for says so, and a tag the index resolved to nothing is
//! named with its reason rather than left out.

use crate::ask::AskContext;
use crate::sources::ResolvedSource;

/// The stance the system prompt holds to, in the four sentences section 7
/// spells, and the drafting stance of section 8 under them. It is a CONSTANT:
/// no request, no envelope and no passage composes any part of it — including
/// the date, which the model writes and a human owns.
pub const STANCE: &str = "\
You are the Gyld ask agent. You explain a decision graph as it was EMITTED.

1. The context below and the quoted passages are the whole of what you know.
2. Quote a passage only from the sources supplied, and name the tag it came
   from. Say when a tag resolved to nothing.
3. Name what is not emitted rather than filling it in. A status, a blocker, a
   lean or an edge you were not given does not exist.
4. You may propose an alternative and draft ruling text when asked. You never
   rule, you never submit, and you never claim a decision has been taken.

# Drafting

When the reader asks for a proposal, a recommendation, a lean or draft ruling
text — and not otherwise — say your reasoning in prose and then call the
`propose_draft` tool ONCE.

* Name the alternative by the QUALIFIED SLOT the context lists for it under
  `Alternatives`, and propose only an alternative that list offers. If it offers
  none that answers the question, say so in prose and do not call the tool.
* Write the ruling as ONE sentence, in the form the overlays use:
  `YYYY-MM-DD, owner: ...`.
* List the source tags the draft leans on, and only tags supplied above.

A draft is an OFFER. A human takes it, edits it or discards it; it is recorded
against the model that wrote it, never against a person. Calling the tool is not
ruling, is not submitting, and does not make a decision taken — say none of
those things about it.";

/// One consultation's prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prompt {
    /// The stable prefix: stance, context and passages.
    pub system: String,
    /// The reader's question, as they typed it.
    pub user: String,
}

/// Compose the prompt for one turn.
pub fn compose(context: &AskContext, sources: &[ResolvedSource]) -> Prompt {
    let mut s = String::new();
    s.push_str(STANCE);
    s.push_str("\n\n# The record\n\n");
    line(&mut s, "stream", &context.stream);
    line(&mut s, "perspective", &context.perspective);
    line(&mut s, "slot", &context.record.slot);
    line(&mut s, "label", &context.record.label);
    line(&mut s, "kind", &context.record.kind);
    line(&mut s, "definition", &context.record.definition);
    line(&mut s, "description", &context.record.description);
    if context.record.lines.is_empty() {
        s.push_str("drawn text: this picture draws no text for this record\n");
    } else {
        s.push_str("drawn text:\n");
        for drawn in context.record.lines.iter() {
            s.push_str("  | ");
            s.push_str(drawn);
            s.push('\n');
        }
    }

    s.push_str("\n# Status, as emitted\n\n");
    if !context.status.emitted {
        s.push_str("this stream emitted no decide-now list, so it has no status\n");
    } else if !context.status.listed {
        s.push_str("this stream's decide-now list carries no row for this record\n");
    } else {
        line(&mut s, "declared", &context.status.declared);
        line(&mut s, "effective", &context.status.effective);
        line(&mut s, "tier", &context.status.tier);
        s.push_str(&format!(
            "answerable now: {}\n",
            context.status.answerable_now
        ));
        if context.status.reason.is_empty() {
            s.push_str("reason: none emitted\n");
        } else {
            line(&mut s, "reason", &context.status.reason);
        }
    }

    s.push_str("\n# Alternatives\n\n");
    if context.alternatives.is_empty() {
        s.push_str("none emitted\n");
    } else {
        for offer in context.alternatives.iter() {
            s.push_str(&format!(
                "- {} ({}){}{}\n",
                offer.label,
                offer.slot,
                if offer.preferred {
                    " — the recorded lean"
                } else {
                    ""
                },
                if offer.description.is_empty() {
                    String::new()
                } else {
                    format!(": {}", offer.description)
                }
            ));
        }
    }
    match context.lean.as_deref() {
        Some(lean) => {
            s.push_str(&format!("recorded lean: {lean}\n"));
        }
        None => {
            s.push_str("recorded lean: none\n");
        }
    }

    s.push_str("\n# Ruling\n\n");
    match context.ruling.as_ref() {
        Some(ruling) => {
            s.push_str(&format!(
                "{} ({}), stamped {}, by {}\n",
                if ruling.live {
                    "a ruling that stands"
                } else {
                    "a ruling an ancestor made that this stream reopened"
                },
                ruling.slot,
                empty_is(&ruling.stamp, "no stamp"),
                empty_is(&ruling.principal, "no principal"),
            ));
            if !ruling.sources.is_empty() {
                s.push_str(&format!("it cites: {}\n", ruling.sources.join(", ")));
            }
            s.push_str("text:\n");
            for text in ruling.text.lines() {
                s.push_str("  | ");
                s.push_str(text);
                s.push('\n');
            }
        }
        None => {
            s.push_str("no ruling decides this record in this stream\n");
        }
    }

    s.push_str("\n# Edges, as emitted\n\n");
    listed(&mut s, "waits on", &context.requires);
    listed(&mut s, "unlocks", &context.unlocks);
    listed(&mut s, "gated by", &context.gates);

    s.push_str("\n# Neighbourhood\n\n");
    match context.neighbourhood.as_ref() {
        Some(pointer) => {
            s.push_str(&format!(
                "a {} lens for {} is emitted at {}; its geometry is NOT supplied here\n",
                pointer.perspective,
                if pointer.member {
                    "this record"
                } else {
                    "this stream"
                },
                pointer.path,
            ));
        }
        None => {
            s.push_str("this stream emitted no lens for this record\n");
        }
    }

    s.push_str("\n# Sources\n\n");
    if sources.is_empty() {
        s.push_str("this record cites no source tag in this build\n");
    } else {
        for source in sources.iter() {
            s.push_str(&passage(source));
        }
    }

    Prompt {
        system: s,
        user: context.question.trim().to_string(),
    }
}

/// One source, as the only quotable material an answer gets.
fn passage(source: &ResolvedSource) -> String {
    if !source.resolved {
        return format!(
            "## {} (cited by the record's `{}`)\n\nUNRESOLVED: {}. There is no passage for this \
             tag; say so rather than guessing at one.\n\n",
            source.tag,
            source.cites,
            source.reason.as_deref().unwrap_or("no reason emitted"),
        );
    }
    let lines = match source.lines.as_deref() {
        Some([first, last]) => format!("lines {first}-{last}"),
        _ => "lines not emitted".to_string(),
    };
    format!(
        "## {} (cited by the record's `{}`)\n\n{} — {}, {}, under {:?}{}\n\n```\n{}\n```\n\n",
        source.tag,
        source.cites,
        source.document.as_deref().unwrap_or(""),
        source.path.as_deref().unwrap_or(""),
        lines,
        source.heading.as_deref().unwrap_or(""),
        if source.truncated {
            " (the index capped this passage: it is a prefix)"
        } else {
            ""
        },
        source.passage.as_deref().unwrap_or(""),
    )
}

fn line(s: &mut String, name: &str, value: &str) {
    s.push_str(name);
    s.push_str(": ");
    s.push_str(empty_is(value, "not emitted"));
    s.push('\n');
}

fn listed(s: &mut String, name: &str, values: &[String]) {
    if values.is_empty() {
        s.push_str(&format!("{name}: nothing emitted\n"));
    } else {
        s.push_str(&format!("{name}: {}\n", values.join(", ")));
    }
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
    use crate::ask::AskContext;
    use crate::sources::tests::index;

    fn context() -> AskContext {
        AskContext::parse(Some(&crate::ask::tests::envelope())).expect("the fixture envelope")
    }

    /// The whole prompt, over the fixture envelope and the fixture index.
    const GOLDEN: &str = include_str!("../tests/fixtures/explain-prompt.txt");

    #[test]
    fn the_prompt_is_the_golden_one_over_the_fixture_index() {
        let context = context();
        let sources = index().resolve(&context);
        let prompt = compose(&context, &sources);
        assert_eq!(
            prompt.system, GOLDEN,
            "the composed system prompt drifted from tests/fixtures/explain-prompt.txt"
        );
        assert_eq!(prompt.user, "why is this blocked?");
    }

    #[test]
    fn the_stance_is_a_constant_no_request_composes() {
        let prompt = compose(&context(), &[]);
        assert!(prompt.system.starts_with(STANCE), "{}", prompt.system);
        assert!(STANCE.contains("Quote a passage only from the sources supplied"));
        assert!(STANCE.contains("You never\n   rule, you never submit"));
        assert!(STANCE.contains("Name what is not emitted rather than filling it in"));

        // The drafting stance (section 8) is part of the same constant: the
        // form the overlays use, the qualified slot, and the boundary.
        assert!(STANCE.contains("`YYYY-MM-DD, owner: ...`"));
        assert!(STANCE.contains("QUALIFIED SLOT"));
        assert!(STANCE.contains("propose only an alternative that list offers"));
        assert!(STANCE.contains("Calling the tool is not\nruling"));
        assert!(
            !STANCE.contains("20"),
            "the stance carries no date of its own: a clock in the cached prefix \
             would move it every turn"
        );
    }

    #[test]
    fn every_absence_is_written_as_an_absence() {
        let mut context = context();
        context.status.emitted = false;
        context.status.listed = false;
        context.alternatives.clear();
        context.lean = None;
        context.ruling = None;
        context.requires.clear();
        context.neighbourhood = None;
        context.record.lines.clear();
        let prompt = compose(&context, &[]);
        for said in [
            "this stream emitted no decide-now list",
            "this picture draws no text for this record",
            "recorded lean: none",
            "no ruling decides this record in this stream",
            "waits on: nothing emitted",
            "this stream emitted no lens for this record",
            "this record cites no source tag in this build",
        ] {
            assert!(prompt.system.contains(said), "missing {said:?}");
        }

        // A list with no row for this record is a DIFFERENT fact, and says so.
        context.status.emitted = true;
        let prompt = compose(&context, &[]);
        assert!(prompt
            .system
            .contains("decide-now list carries no row for this record"));
    }

    #[test]
    fn an_unresolved_tag_reaches_the_prompt_with_its_reason() {
        let context = context();
        let sources = index().resolve(&context);
        let prompt = compose(&context, &sources);
        assert!(
            prompt
                .system
                .contains("## AZ-7 (cited by the record's `record`)"),
            "{}",
            prompt.system
        );
        assert!(prompt
            .system
            .contains("UNRESOLVED: no document in this index declares it"));
        assert!(prompt
            .system
            .contains("There is no passage for this tag; say so rather than guessing"));
        // and the resolved one carries the index's own passage, verbatim.
        assert!(prompt
            .system
            .contains("| Q11 | Key custody and recovery posture | buy |"));
    }
}

/// Rewrite the golden fixture from the composer, for when the prompt changes on
/// purpose. Ignored by default — `cargo test -- --ignored rewrite_the_golden`
/// runs it, and the diff on `tests/fixtures/explain-prompt.txt` is then the
/// change under review.
#[cfg(test)]
mod golden {
    use super::*;
    use crate::sources::tests::index;

    #[test]
    #[ignore]
    fn rewrite_the_golden() {
        let context = crate::ask::AskContext::parse(Some(&crate::ask::tests::envelope())).unwrap();
        let sources = index().resolve(&context);
        let prompt = compose(&context, &sources);
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/explain-prompt.txt"
            ),
            prompt.system,
        )
        .unwrap();
    }
}
