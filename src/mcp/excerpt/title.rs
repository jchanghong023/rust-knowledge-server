use std::borrow::Cow;

use pulldown_cmark::{Event, Parser};

use crate::{
    markdown::{HeadingBoundary, heading_boundary, heading_spans},
    mcp::fetch::{Reader, TextPassage},
    search::{Evidence, Page},
};

use super::{HIT_LIMIT, SearchHit};

const FILE_CONTEXT_BYTES: usize = 256 * 1024;
const PAGE_CONTEXT_BYTES: usize = 1024 * 1024;
const PAGE_PASSAGE_READS: usize = 256;
const HEADING_SOURCE_BYTES: usize = 8 * 1024;
const TITLE_BYTES: usize = 1024;

pub(super) fn add(page: &Page, hits: &mut [SearchHit<'_>]) {
    let mut budget = Budget {
        bytes: PAGE_CONTEXT_BYTES,
        reads: PAGE_PASSAGE_READS,
    };
    let mut seen = [false; HIT_LIMIT];
    for rank in 0..hits.len() {
        if seen[rank] {
            continue;
        }
        seen[rank] = true;
        let seed = hits[rank].evidence;
        let Some((origin, source)) = context(page, seed, &mut budget) else {
            continue;
        };
        let end = origin + source.len();
        let mut group = [rank; HIT_LIMIT];
        let mut count = 1;
        for index in rank + 1..hits.len() {
            if !seen[index]
                && same_file(seed, hits[index].evidence)
                && (origin..end).contains(&anchor(&hits[index]))
            {
                seen[index] = true;
                group[count] = index;
                count += 1;
            }
        }
        let group = &mut group[..count];
        group.sort_unstable_by_key(|&index| anchor(&hits[index]));
        set_titles(&source, origin, group, hits);
    }
}

fn same_file(left: &Evidence, right: &Evidence) -> bool {
    left.source.library == right.source.library
        && left.source.version == right.source.version
        && left.path == right.path
}

fn anchor(hit: &SearchHit<'_>) -> usize {
    hit.evidence.source.byte_start + hit.anchor_start
}

struct Budget {
    bytes: usize,
    reads: usize,
}

// Start at file byte zero or at a proven top-level heading cut. Parsing from an
// arbitrary slice instead could promote a heading-like line inside a fence.
fn context<'a>(
    page: &Page,
    tail: &'a Evidence,
    budget: &mut Budget,
) -> Option<(usize, Cow<'a, str>)> {
    budget.bytes = budget.bytes.checked_sub(tail.text.len())?;
    if tail.source.byte_start == 0 {
        return Some((0, Cow::Borrowed(&tail.text)));
    }
    let reader = Reader::new(page.version(&tail.source)?.open().ok()?);
    let mut id = tail.source.point;
    let mut length = tail.text.len();
    let mut previous_parts = Vec::<(TextPassage, usize)>::new();
    let origin = loop {
        let (text, byte_start, line_start) = previous_parts.last().map_or(
            (tail.text.as_str(), tail.source.byte_start, tail.line_start),
            |(part, _)| (part.text.as_str(), part.byte_start, part.line_start),
        );
        if byte_start == 0 {
            break 0;
        }
        let current_id = id;
        budget.reads = budget.reads.checked_sub(1)?;
        id = id.checked_sub(1).filter(|&id| id != 0)?;
        let previous = reader.passage(id).ok()??;
        budget.bytes = budget.bytes.checked_sub(previous.text.len())?;
        let prefix = previous
            .prefix_before(&tail.path, text, byte_start, line_start)
            .ok()?;
        if prefix.len() == previous.text.len()
            && (previous.byte_start == 0 || previous.spans_multiple_lines())
        {
            let boundary = heading_boundary(prefix, text);
            if boundary != HeadingBoundary::MayBeSizeSplit
                && heading_spans(text)
                    .next()
                    .is_some_and(|span| !text[..span.start].contains(['\r', '\n']))
                && (boundary == HeadingBoundary::Proven
                    || file_ends_at(&reader, current_id, &tail.path, budget)?)
            {
                break byte_start;
            }
        }
        let len = prefix.len();
        length += len;
        if length > FILE_CONTEXT_BYTES {
            return None;
        }
        previous_parts.push((previous, len));
    };
    if previous_parts.is_empty() {
        return Some((origin, Cow::Borrowed(&tail.text)));
    }
    let mut source = String::with_capacity(length);
    for (part, len) in previous_parts.into_iter().rev() {
        source.push_str(&part.text[..len]);
    }
    source.push_str(&tail.text);
    Some((origin, Cow::Owned(source)))
}

fn file_ends_at(reader: &Reader, id: u64, path: &str, budget: &mut Budget) -> Option<bool> {
    budget.reads = budget.reads.checked_sub(1)?;
    if let Some(next) = reader.passage(id.checked_add(1)?).ok()? {
        budget.bytes = budget.bytes.checked_sub(next.text.len())?;
        Some(next.path != path)
    } else {
        Some(true)
    }
}

fn set_titles(source: &str, origin: usize, indices: &[usize], hits: &mut [SearchHit<'_>]) {
    let mut headings = heading_spans(source).peekable();
    let mut previous: Option<usize> = None;
    for &index in indices {
        let anchor = anchor(&hits[index]) - origin;
        let latest = std::iter::from_fn(|| {
            headings.next_if(|span| {
                // Include the physical heading line even when the parser
                // excludes indentation or an initial BOM from its span.
                source[..span.start]
                    .rfind(['\r', '\n'])
                    .map_or(0, |line_break| line_break + 1)
                    <= anchor
            })
        })
        .last();
        if let Some(span) = latest {
            hits[index].section_title = heading_text(&source[span]);
        } else if let Some(previous) = previous {
            hits[index].section_title = hits[previous].section_title.clone();
        }
        previous = Some(index);
    }
}

fn heading_text(raw: &str) -> Option<String> {
    // Omit oversized headings, including one that may be cut at this prefix's
    // end because its physical line exceeded the index's chunk size.
    if raw.len() > HEADING_SOURCE_BYTES {
        return None;
    }
    let mut title = String::new();
    for event in Parser::new(raw) {
        match event {
            Event::Text(text) | Event::Code(text) => {
                if title.len() + text.len() > TITLE_BYTES {
                    return None;
                }
                title.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak => title.push(' '),
            _ => {}
        }
    }
    let start = title.len() - title.trim_start().len();
    let end = title.trim_end().len();
    if start >= end || end - start > TITLE_BYTES {
        return None;
    }
    title.truncate(end);
    if start != 0 {
        drop(title.drain(..start));
    }
    Some(title)
}
