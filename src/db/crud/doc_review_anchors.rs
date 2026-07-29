//! Where an annotation points after the document moved under it.
//!
//! A `doc_review_comments` row anchors to a 1-based line range, but every
//! pass appends a whole new version of the markdown. Insert one line at the
//! top and every anchor below it addresses the wrong passage — the document
//! pane tints the wrong block, the rail's `L12` lies, and the next pass
//! hands the review session line numbers that point somewhere else.
//!
//! So a revision remaps the anchors it moved. This module is the pure part
//! of that: diff the two documents, and say for each old line where it
//! ended up. `doc_reviews.rs` applies the answer in the same connection
//! lock that writes the version, so no reader ever sees the new document
//! next to the old document's anchors.

use similar::{DiffTag, TextDiff};

/// Below this many characters a quote is too generic to re-find a passage
/// by — "Yes." matches half a document.
const MIN_QUOTE_MATCH: usize = 12;

/// How much of a quote to match on. A quote covers a whole rendered block,
/// which the markdown source may wrap over several lines; its opening is
/// the part that reliably lives on one of them.
const QUOTE_HEAD_CHARS: usize = 60;

/// What became of one line of the old document.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Landed {
    /// Still in the document, character for character.
    Same(usize),
    /// Its passage was rewritten; this is the line that replaced it.
    Rewritten(usize),
    /// Deleted outright — nothing in the new document came from it.
    Gone,
}

/// Where each line of the old document ended up in the new one.
pub(crate) struct LineMap<'a> {
    /// Per old line, 0-based on both sides.
    moved: Vec<Landed>,
    /// Per old line (0-based): the new-side position to fall back on when
    /// it `Gone` — where the deleted text used to sit.
    gone_to: Vec<usize>,
    /// The new document's lines, for the quote fallback and for clamping.
    new_lines: Vec<&'a str>,
}

/// Diff `old` against `new` line by line and record where each old line
/// went. Both are compared as `str::lines()` splits them, which is exactly
/// how mdast numbers the lines an anchor was taken from.
pub(crate) fn line_map<'a>(old: &str, new: &'a str) -> LineMap<'a> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut moved = vec![Landed::Gone; old_lines.len()];
    let mut gone_to = vec![0usize; old_lines.len()];

    let diff = TextDiff::from_slices(&old_lines, &new_lines);
    for op in diff.ops() {
        let old_range = op.old_range();
        let new_range = op.new_range();
        match op.tag() {
            DiffTag::Equal => {
                for (i, oi) in old_range.enumerate() {
                    moved[oi] = Landed::Same(new_range.start + i);
                    gone_to[oi] = new_range.start + i;
                }
            }
            // Rewritten in place. The passage is still the passage the
            // annotation was about, so it follows its replacement rather
            // than counting as deleted — that is the single most common
            // shape of a review revision.
            DiffTag::Replace => {
                for (i, oi) in old_range.enumerate() {
                    let at = if new_range.is_empty() {
                        new_range.start
                    } else {
                        new_range.start + i.min(new_range.len() - 1)
                    };
                    moved[oi] = Landed::Rewritten(at);
                    gone_to[oi] = at;
                }
            }
            DiffTag::Delete => {
                for oi in old_range {
                    moved[oi] = Landed::Gone;
                    gone_to[oi] = new_range.start;
                }
            }
            DiffTag::Insert => {}
        }
    }

    LineMap {
        moved,
        gone_to,
        new_lines: new.lines().collect(),
    }
}

impl LineMap<'_> {
    /// Where a `(start, end)` anchor — 1-based, inclusive, in the old
    /// document — points in the new one.
    ///
    /// In precedence: lines the revision left alone, then the annotation's
    /// stored `quote` if the passage turns up elsewhere (a passage the
    /// revision *moved* reads to a diff as a delete here and an unrelated
    /// insert there), then whatever replaced the passage, then the point
    /// where it used to be.
    ///
    /// The quote only outranks the replacement when nothing in the range
    /// survived verbatim: a paragraph rewritten in place no longer matches
    /// its own quote, and it should stay where it is rather than chase a
    /// similar-looking passage across the document.
    pub(crate) fn remap(&self, start: i32, end: i32, quote: Option<&str>) -> (i32, i32) {
        let first = start.max(1) as usize - 1;
        let last = end.max(start).max(1) as usize - 1;

        let mut verbatim = false;
        let mut span: Option<(usize, usize)> = None;
        for oi in first..=last {
            let at = match self.moved.get(oi) {
                Some(Landed::Same(at)) => {
                    verbatim = true;
                    *at
                }
                Some(Landed::Rewritten(at)) => *at,
                _ => continue,
            };
            span = Some(match span {
                // A range that straddles an untouched line and a rewritten
                // one keeps its full extent — it is one passage either way.
                Some((lo, hi)) => (lo.min(at), hi.max(at)),
                None => (at, at),
            });
        }
        if verbatim {
            let (lo, hi) = span.expect("a verbatim line always contributes a span");
            return (self.clamp(lo), self.clamp(hi));
        }
        if let Some(at) = quote.and_then(|q| self.find_quote(q)) {
            return (self.clamp(at), self.clamp(at));
        }
        if let Some((lo, hi)) = span {
            return (self.clamp(lo), self.clamp(hi));
        }

        // Nothing left of it. The honest anchor is the point in the new
        // document where the passage used to be.
        let at = self
            .gone_to
            .get(first)
            .copied()
            .unwrap_or_else(|| self.new_lines.len().saturating_sub(1));
        (self.clamp(at), self.clamp(at))
    }

    /// A 0-based new-side index as a 1-based line inside the document.
    fn clamp(&self, at: usize) -> i32 {
        let last = self.new_lines.len().max(1);
        (at + 1).min(last) as i32
    }

    /// The line the quoted passage starts on, if it is still in the
    /// document somewhere.
    fn find_quote(&self, quote: &str) -> Option<usize> {
        let needle = normalise(quote);
        if needle.len() < MIN_QUOTE_MATCH {
            return None;
        }
        let head: String = needle.chars().take(QUOTE_HEAD_CHARS).collect();
        self.new_lines.iter().position(|line| {
            let hay = normalise(line);
            // Either the source line holds the opening of the quote, or —
            // when the block was wrapped across lines — the line is itself
            // a chunk of it.
            hay.len() >= MIN_QUOTE_MATCH && (hay.contains(&head) || head.contains(&hay))
        })
    }
}

/// A quote is the *rendered* block text, so it carries none of the markdown
/// that produced it. Comparison strips what the renderer would have eaten
/// and collapses whitespace, so `**Ship it** now.` still matches the stored
/// `Ship it now.`.
fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !at_space {
                out.push(' ');
                at_space = true;
            }
            continue;
        }
        at_space = false;
        if matches!(
            c,
            '#' | '*' | '_' | '`' | '>' | '~' | '[' | ']' | '(' | ')' | '|' | '\\'
        ) {
            continue;
        }
        out.extend(c.to_lowercase());
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Title\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";

    /// Remap the anchor on "Second paragraph." (line 5) into `new`.
    fn second(new: &str, quote: Option<&str>) -> (i32, i32) {
        line_map(DOC, new).remap(5, 5, quote)
    }

    #[test]
    fn an_untouched_document_leaves_every_anchor_alone() {
        assert_eq!(second(DOC, None), (5, 5));
        assert_eq!(line_map(DOC, DOC).remap(1, 3, None), (1, 3));
    }

    #[test]
    fn lines_added_above_push_the_anchor_down() {
        let new = "# Title\n\nA new opener.\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
        assert_eq!(second(new, None), (7, 7));
    }

    #[test]
    fn lines_removed_above_pull_the_anchor_up() {
        let new = "# Title\n\nSecond paragraph.\n\nThird paragraph.\n";
        assert_eq!(second(new, None), (3, 3));
    }

    #[test]
    fn a_passage_rewritten_in_place_keeps_its_anchor() {
        let new = "# Title\n\nFirst paragraph.\n\nSecond paragraph, revised.\n\nThird paragraph.\n";
        assert_eq!(second(new, Some("Second paragraph.")), (5, 5));
    }

    #[test]
    fn a_multi_line_anchor_spans_what_its_lines_became() {
        // Lines 3-5 cover "First paragraph." through "Second paragraph.";
        // an insert above moves the whole range down together.
        let new = "# Title\n\nA new opener.\n\nFirst paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
        assert_eq!(line_map(DOC, new).remap(3, 5, None), (5, 7));
    }

    #[test]
    fn a_range_half_rewritten_keeps_its_whole_extent() {
        // Line 3 survives, line 5 is rewritten: the anchor still covers
        // both, not just the untouched half.
        let new = "# Title\n\nFirst paragraph.\n\nSecond paragraph, revised.\n\nThird paragraph.\n";
        assert_eq!(line_map(DOC, new).remap(3, 5, None), (3, 5));
    }

    #[test]
    fn a_moved_passage_is_found_by_its_quote() {
        // Deleted from the middle, reinserted at the end: the diff sees no
        // surviving line, the quote does.
        let new = "# Title\n\nFirst paragraph.\n\nThird paragraph.\n\nSecond paragraph.\n";
        assert_eq!(second(new, Some("Second paragraph.")), (7, 7));
    }

    #[test]
    fn a_quote_matches_through_the_markdown_that_rendered_it() {
        let new = "# Title\n\nFirst paragraph.\n\nThird paragraph.\n\n**Second paragraph.**\n";
        assert_eq!(second(new, Some("Second paragraph.")), (7, 7));
    }

    #[test]
    fn a_deleted_passage_anchors_where_it_used_to_be() {
        let new = "# Title\n\nFirst paragraph.\n";
        assert_eq!(second(new, Some("Second paragraph.")), (3, 3));
    }

    #[test]
    fn a_quote_too_short_to_identify_a_passage_is_not_trusted() {
        let new = "# Title\n\nFirst paragraph.\n";
        assert_eq!(line_map(DOC, new).remap(5, 5, Some("ok")), (3, 3));
    }

    #[test]
    fn an_anchor_never_points_past_the_end_of_the_document() {
        assert_eq!(second("# Title\n", None), (1, 1));
    }

    #[test]
    fn an_emptied_document_still_yields_a_legal_anchor() {
        assert_eq!(second("", None), (1, 1));
    }

    #[test]
    fn an_anchor_past_the_end_of_the_old_document_is_clamped_in() {
        assert_eq!(line_map(DOC, DOC).remap(99, 120, None), (7, 7));
    }
}
