//! Source spans and UTF-16 chunking adapted from OMP's `crates/knowledge/src/import.rs` (MIT).

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};

const MAX_SECTION_UNITS: usize = 18_000;
const OVERLAP_UNITS: usize = 500;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Section<'a> {
    pub(crate) text: &'a str,
    pub(crate) byte_start: usize,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
}

/// Returns ordered, lossless source slices. Only windows of an oversized line overlap.
pub(crate) fn sections(source: &str) -> Vec<Section<'_>> {
    let mut headings = heading_spans(source).map(|span| span.start).peekable();

    let mut at = Position { byte: 0, line: 1 };
    let mut chunks = Chunks {
        source,
        sections: Vec::new(),
        start: at,
        units: 0,
        blank: None,
    };
    for text in source_lines(source) {
        let after = Position {
            byte: at.byte + text.len(),
            line: at.line + 1,
        };
        // The parser can start a heading after indentation (or the BOM).
        // Move the entire physical line, not just the parsed heading, into its section.
        if headings.next_if(|&offset| offset < after.byte).is_some() {
            chunks.flush(at);
        }
        chunks.add_line(at, after, text);
        at = after;
    }
    chunks.flush(at);
    chunks.sections
}

#[derive(PartialEq, Eq)]
pub(crate) enum HeadingBoundary {
    Proven,
    NeedsEndOfFile,
    MayBeSizeSplit,
}

/// A local witness that a non-overlapping passage starts at a real heading.
/// If an ordinary predecessor and the first line fit together, the immediate
/// size limit could not have made the cut. A cut after a blank could also have
/// been a later overflow spilling to that blank, unless the next blank fit
/// or there was no more input to overflow.
pub(crate) fn heading_boundary(previous: &str, current: &str) -> HeadingBoundary {
    use HeadingBoundary::{MayBeSizeSplit, NeedsEndOfFile, Proven};
    let units = |text: &str| {
        if text.is_ascii() {
            text.len()
        } else {
            text.encode_utf16().count()
        }
    };
    let mut current_lines = source_lines(current);
    let Some(first) = current_lines.next() else {
        return MayBeSizeSplit;
    };
    let Some(remaining) = MAX_SECTION_UNITS.checked_sub(units(previous) + units(first)) else {
        return MayBeSizeSplit;
    };
    if !source_lines(previous)
        .last()
        .is_some_and(|line| line.chars().all(is_blank_char))
    {
        return Proven;
    }
    let mut remaining = remaining;
    for line in current_lines {
        let Some(after) = remaining.checked_sub(units(line)) else {
            return MayBeSizeSplit;
        };
        if line.chars().all(is_blank_char) {
            return Proven;
        }
        remaining = after;
    }
    NeedsEndOfFile
}

/// Top-level native Markdown headings, excluding nested blocks and fences.
pub(crate) fn heading_spans(source: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let markdown = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut base = source.len() - markdown.len();
    let mut events = Parser::new(markdown).into_offset_iter();
    let mut depth = 0;
    std::iter::from_fn(move || {
        while let Some((event, span)) = events.next() {
            match event {
                Event::Start(Tag::Heading { .. }) if depth == 0 => {
                    depth += 1;
                    return Some(base + span.start..base + span.end);
                }
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) if depth == 0 => {
                    let start = base + span.start;
                    if let Some(after_closer) = tab_fence_end(&source[start..base + span.end]) {
                        // pulldown-cmark's close scanner accepts only spaces after
                        // the marker run. Resume after OMP's tab-terminated fence.
                        base = start + after_closer;
                        events = Parser::new(&source[base..]).into_offset_iter();
                    } else {
                        depth += 1;
                    }
                }
                Event::Start(_) => depth += 1,
                Event::End(_) => depth -= 1,
                _ => {}
            }
        }
        None
    })
}

/// Looks only inside a parser-confirmed top-level fenced block, so tabs in
/// prose, lists, and other Markdown constructs cannot create a boundary.
fn tab_fence_end(block: &str) -> Option<usize> {
    if !block.as_bytes().contains(&b'\t') {
        return None;
    }
    let mut lines = source_lines(block);
    let opener = lines.next()?;
    let marker = *opener.as_bytes().first()?;
    let opening_run = opener.bytes().take_while(|&b| b == marker).count();
    let mut after = opener.len();
    for line in lines {
        after += line.len();
        let unindented = line.trim_start_matches(' ');
        if line.len() - unindented.len() > 3 {
            continue;
        }
        let candidate = unindented.trim_end_matches(['\r', '\n']);
        let run = candidate.bytes().take_while(|&b| b == marker).count();
        let suffix = &candidate[run..];
        if run >= opening_run
            && suffix.contains('\t')
            && suffix.bytes().all(|b| matches!(b, b' ' | b'\t'))
        {
            return Some(after);
        }
    }
    None
}

pub(crate) fn source_lines(mut rest: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.find(['\r', '\n']).map_or(rest.len(), |offset| {
            offset
                + if rest[offset..].starts_with("\r\n") {
                    2
                } else {
                    1
                }
        });
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

#[derive(Clone, Copy)]
struct Position {
    byte: usize,
    line: usize,
}

struct Chunks<'a> {
    source: &'a str,
    sections: Vec<Section<'a>>,
    start: Position,
    units: usize,
    // The position and cumulative UTF-16 count just after the latest blank line.
    blank: Option<(Position, usize)>,
}

impl Chunks<'_> {
    fn flush(&mut self, after: Position) {
        if self.start.byte < after.byte {
            self.sections.push(Section {
                text: &self.source[self.start.byte..after.byte],
                byte_start: self.start.byte,
                line_start: self.start.line,
                line_end: after.line - 1,
            });
        }
        self.start = after;
        self.units = 0;
        self.blank = None;
    }

    fn add_line(&mut self, at: Position, after: Position, text: &str) {
        let ascii = text.is_ascii();
        let units = if ascii {
            text.len()
        } else {
            text.encode_utf16().count()
        };
        if units > MAX_SECTION_UNITS {
            self.flush(at);
            self.split_line(at, after, ascii);
            self.start = after;
            return;
        }

        if self.units + units > MAX_SECTION_UNITS {
            if let Some((after_blank, through_blank)) = self.blank {
                let tail_units = self.units - through_blank;
                self.flush(after_blank);
                self.units = tail_units;
            }
            if self.units + units > MAX_SECTION_UNITS {
                self.flush(at);
            }
        }
        self.units += units;
        // Match OMP's ECMAScript trim for blank-line chunking.
        if text.chars().all(is_blank_char) {
            self.blank = Some((after, self.units));
        }
    }

    fn split_line(&mut self, at: Position, after: Position, ascii: bool) {
        let mut start = at.byte;
        loop {
            let rest = &self.source[start..after.byte];
            let limit = if ascii {
                rest.len().min(MAX_SECTION_UNITS)
            } else {
                utf16_prefix(rest)
            };
            // A CRLF is one physical line ending; never place a cut inside it.
            let end = if rest.as_bytes().get(limit) == Some(&b'\n')
                && rest.as_bytes()[limit - 1] == b'\r'
            {
                limit - 1
            } else {
                limit
            };
            let text = &rest[..end];
            self.sections.push(Section {
                text,
                byte_start: start,
                line_start: at.line,
                line_end: at.line,
            });
            if end == rest.len() {
                break;
            }
            // Round the next start backward if the overlap would split a surrogate
            // pair. Scan only the overlapping tail, not the whole emitted window.
            start += if ascii {
                end - OVERLAP_UNITS
            } else {
                overlap_start(text)
            };
        }
    }
}

fn is_blank_char(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{0085}' && c.is_whitespace())
}

fn utf16_prefix(text: &str) -> usize {
    let mut remaining = MAX_SECTION_UNITS;
    for (offset, c) in text.char_indices() {
        let width = c.len_utf16();
        if width > remaining {
            return offset;
        }
        remaining -= width;
    }
    text.len()
}

fn overlap_start(text: &str) -> usize {
    let mut units = 0;
    for (offset, c) in text.char_indices().rev() {
        units += c.len_utf16();
        if units >= OVERLAP_UNITS {
            return offset;
        }
    }
    unreachable!("a non-final window always has enough units for overlap");
}

#[cfg(test)]
mod tests {
    use super::{MAX_SECTION_UNITS, OVERLAP_UNITS, Section, sections};

    fn assert_span(
        span: &Section<'_>,
        source: &str,
        start: usize,
        expected: &str,
        lines: (usize, usize),
    ) {
        assert_eq!(span.text, expected);
        assert_eq!(span.byte_start, start);
        assert_eq!((span.line_start, span.line_end), lines);
        assert_eq!(
            span.text.as_ptr(),
            source[start..start + expected.len()].as_ptr()
        );
    }

    #[test]
    fn recognizes_top_level_headings_not_fenced_or_nested_lookalikes() {
        let first = concat!(
            "# Guide\n",
            "````md\n",
            "```\n",
            "# Still code\n",
            "Pseudo Setext\n",
            "------------\n",
            "```` trailing text\n",
            "## Still code\n",
            "````\t\n",
            "- item\n",
            "  ### Nested heading\n",
            "> Quoted Setext\n",
            "> -------------\n",
            "\n",
        );
        let second = "  ## Outside\n\n";
        let third = "Setext heading\ncontinued on another line\n--------------\nbody";
        let source = format!("{first}{second}{third}");
        let spans = sections(&source);
        assert_eq!(spans.len(), 3);
        assert_span(&spans[0], &source, 0, first, (1, 14));
        assert_span(&spans[1], &source, first.len(), second, (15, 16));
        assert_span(
            &spans[2],
            &source,
            first.len() + second.len(),
            third,
            (17, 20),
        );
    }

    #[test]
    fn tabbed_fence_recovery_preserves_crlf_and_requires_matching_delimiter() {
        let first = concat!(
            "  ~~~~\r\n",
            "~~~~~`\t\r\n",
            "~~~\t\r\n",
            "# Hidden\r\n",
            " ~~~~~\t \r\n",
        );
        let second = "# Visible\r\n```\r\n# Hidden\r\n```\t\r\n";
        let third = "Last\r\n----\r\n";
        let source = format!("{first}{second}{third}");
        let spans = sections(&source);
        assert_eq!(spans.len(), 3);
        assert_span(&spans[0], &source, 0, first, (1, 5));
        assert_span(&spans[1], &source, first.len(), second, (6, 9));
        assert_span(
            &spans[2],
            &source,
            first.len() + second.len(),
            third,
            (10, 11),
        );
    }

    #[test]
    fn retains_bom_crlf_and_exact_byte_ranges() {
        let first = "\u{feff}# 标题\r\n正文\r\n";
        let second = "## 下一节\r\n后续\r\n";
        let source = format!("{first}{second}");
        let spans = sections(&source);
        assert_eq!(spans.len(), 2);
        assert_span(&spans[0], &source, 0, first, (1, 2));
        assert_span(&spans[1], &source, first.len(), second, (3, 4));
    }

    #[test]
    fn keeps_standalone_whitespace_and_heading_only_sections() {
        let source = "\r\n# One\r# Two\n";
        let spans = sections(source);
        assert_eq!(spans.len(), 3);
        assert_span(&spans[0], source, 0, "\r\n", (1, 1));
        assert_span(&spans[1], source, 2, "# One\r", (2, 2));
        assert_span(&spans[2], source, 8, "# Two\n", (3, 3));
        assert!(sections("").is_empty());
    }

    #[test]
    fn oversized_unicode_line_overlaps_without_losing_source_or_line_anchors() {
        let heading = "# Reference\r\n";
        // A 17,999-unit prefix followed by a surrogate pair. The overlap
        // target also falls inside a pair, because the prefix ends in ASCII.
        let prefix = format!("{}x", "🧪".repeat(8_999));
        let paragraph = format!("{prefix}{}尾\r\n", "🧪".repeat(10_000));
        let next = "## Next\r\n末尾";
        let source = format!("{heading}{paragraph}{next}");
        let spans = sections(&source);
        assert_eq!(spans.len(), 5);
        assert_span(&spans[0], &source, 0, heading, (1, 1));
        assert_span(&spans[1], &source, heading.len(), &prefix, (2, 2));
        assert_span(
            &spans[4],
            &source,
            heading.len() + paragraph.len(),
            next,
            (3, 4),
        );

        let mut covered = heading.len();
        for (index, span) in spans[1..4].iter().enumerate() {
            let end = span.byte_start + span.text.len();
            assert_span(
                span,
                &source,
                span.byte_start,
                &source[span.byte_start..end],
                (2, 2),
            );
            assert!(span.text.encode_utf16().count() <= MAX_SECTION_UNITS);
            assert!(end > covered);
            if index > 0 {
                assert!(span.byte_start > spans[index].byte_start);
                assert!(span.byte_start < covered);
                assert_eq!(
                    source[span.byte_start..covered].encode_utf16().count(),
                    OVERLAP_UNITS + if index == 1 { 1 } else { 0 },
                );
            }
            covered = end;
        }
        assert_eq!(covered, heading.len() + paragraph.len());
    }

    #[test]
    fn prefers_blank_lines_then_falls_back_to_physical_lines() {
        let first = format!("{}\n\n", "a".repeat(9_999));
        let second = format!("{}\n{}\n", "b".repeat(6_000), "c".repeat(7_000));
        let third = "d".repeat(6_000);
        let source = format!("{first}{second}{third}");
        let spans = sections(&source);
        assert_eq!(spans.len(), 3);
        assert_span(&spans[0], &source, 0, &first, (1, 2));
        assert_span(&spans[1], &source, first.len(), &second, (3, 4));
        assert_span(
            &spans[2],
            &source,
            first.len() + second.len(),
            &third,
            (5, 5),
        );
    }

    #[test]
    fn does_not_split_crlf_at_the_window_limit() {
        let source = format!("{}\r\n", "x".repeat(MAX_SECTION_UNITS - 1));
        let spans = sections(&source);
        assert_eq!(spans.len(), 2);
        let start = MAX_SECTION_UNITS - 1 - OVERLAP_UNITS;
        assert_span(
            &spans[0],
            &source,
            0,
            &source[..MAX_SECTION_UNITS - 1],
            (1, 1),
        );
        assert_span(&spans[1], &source, start, &source[start..], (1, 1));
    }
}
