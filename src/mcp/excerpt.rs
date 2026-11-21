use std::ops::Range;

use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    markdown::source_lines,
    search::{Category, Evidence, SearchError, SearchReply, Status},
    technical,
};

use super::fetch::SourceRef;

pub(super) const BYTE_LIMIT: usize = 4096;
const HIT_LIMIT: usize = 5;

#[derive(JsonSchema, Serialize)]
pub(super) struct SearchOutput<'a> {
    status: Status,
    #[schemars(length(max = 5))]
    matches: Vec<SearchHit<'a>>,
}

#[derive(JsonSchema, Serialize)]
struct SearchHit<'a> {
    #[serde(flatten)]
    excerpt: Excerpt<'a>,
    category: Category,
    #[serde(rename = "ref")]
    source_ref: String,
}

#[derive(JsonSchema, Serialize)]
pub(super) struct Excerpt<'a> {
    pub(super) text: &'a str,
    pub(super) library: &'a str,
    pub(super) path: &'a str,
    pub(super) line_start: usize,
    pub(super) line_end: usize,
    pub(super) truncated: bool,
}

struct Hit<'a> {
    evidence: &'a Evidence,
    anchor: Range<usize>,
}

impl<'a> SearchOutput<'a> {
    pub(super) fn from_reply(reply: &'a SearchReply) -> Result<Self, SearchError> {
        let mut selected = Vec::<Hit<'_>>::with_capacity(reply.matches.len().min(HIT_LIMIT));
        let mut reserved = 0;
        for evidence in &reply.matches {
            if selected
                .iter()
                .any(|other| overlaps(evidence, other.evidence))
            {
                continue;
            }
            let anchor = match evidence.category {
                Category::Exact => reply
                    .name
                    .as_deref()
                    .and_then(|name| technical::name_span(&evidence.text, name))
                    .ok_or(SearchError::Store)?,
                Category::Related => {
                    let end = evidence
                        .text
                        .chars()
                        .next()
                        .ok_or(SearchError::Store)?
                        .len_utf8();
                    0..if evidence.text.starts_with("\r\n") {
                        2
                    } else {
                        end
                    }
                }
            };
            if reserved + anchor.len() > BYTE_LIMIT {
                continue;
            }
            reserved += anchor.len();
            selected.push(Hit { evidence, anchor });
            if selected.len() == HIT_LIMIT {
                break;
            }
        }

        let mut remaining = BYTE_LIMIT;
        let count = selected.len();
        let mut matches = Vec::with_capacity(count);
        for (rank, Hit { evidence, anchor }) in selected.into_iter().enumerate() {
            reserved -= anchor.len();
            let slots = count - rank;
            let weight = if rank < 2 && slots > 1 { 2 } else { 1 };
            let budget = anchor.len()
                + (remaining - reserved - anchor.len()) * weight / (slots + weight - 1);
            let range = window(&evidence.text, &anchor, budget);
            let excerpt = excerpt(evidence, &range);
            remaining -= excerpt.text.len();
            let source_ref = SourceRef::encode(&evidence.source, &anchor);
            matches.push(SearchHit {
                excerpt,
                category: evidence.category,
                source_ref,
            });
        }
        Ok(Self {
            status: reply.status,
            matches,
        })
    }
}

fn overlaps(left: &Evidence, right: &Evidence) -> bool {
    if left.source.version != right.source.version || left.path != right.path {
        return false;
    }
    let (left_start, right_start) = (left.source.byte_start, right.source.byte_start);
    left_start < right_start + right.text.len() && right_start < left_start + left.text.len()
}

fn excerpt<'a>(evidence: &'a Evidence, range: &Range<usize>) -> Excerpt<'a> {
    let (line_start, line_end) = lines_for(&evidence.text, range, evidence.line_start);
    Excerpt {
        text: &evidence.text[range.clone()],
        library: &evidence.library,
        path: &evidence.path,
        line_start,
        line_end,
        truncated: range.len() < evidence.text.len(),
    }
}

pub(super) fn lines_for(text: &str, range: &Range<usize>, first_line: usize) -> (usize, usize) {
    let mut line_start = first_line;
    let mut offset = 0;
    for (index, line) in source_lines(text).enumerate() {
        offset += line.len();
        if offset <= range.start {
            line_start += 1;
        }
        if offset >= range.end {
            return (line_start, first_line + index);
        }
    }
    unreachable!("non-empty excerpt within the source text")
}

pub(super) fn window(text: &str, anchor: &Range<usize>, budget: usize) -> Range<usize> {
    if text.len() <= budget {
        return 0..text.len();
    }
    let start = anchor
        .start
        .saturating_sub((budget - anchor.len()) / 3)
        .min(text.len() - budget);
    let start = (start..=anchor.start)
        .find(|&at| boundary(text, at))
        .expect("anchor boundary");
    let end = (anchor.end..=start + budget)
        .rev()
        .find(|&at| boundary(text, at))
        .expect("anchor boundary");
    start..end
}

pub(super) fn boundary(text: &str, at: usize) -> bool {
    text.is_char_boundary(at) && !(at != 0 && text.as_bytes().get(at - 1..=at) == Some(b"\r\n"))
}
