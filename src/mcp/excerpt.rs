use std::ops::Range;

use caseless::Caseless;
use schemars::JsonSchema;
use serde::{Serialize, Serializer};

use crate::{
    markdown::source_lines,
    search::{Category, Evidence, Page, SearchError, SearchReply, Status},
};

use super::fetch::SourceRef;

mod title;

pub(super) const BYTE_LIMIT: usize = 4096;
pub(super) const LINE_LIMIT: usize = 60;
const HIT_LIMIT: usize = 16;
const SHARED_PREFIX_MIN: usize = 512;

#[derive(JsonSchema, Serialize)]
pub(super) struct SearchOutput<'a> {
    status: Status,
    #[schemars(length(max = 16))]
    matches: Vec<SearchHit<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_cursor: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct SearchHit<'a> {
    #[serde(flatten)]
    excerpt: Excerpt<'a>,
    category: Category,
    #[serde(serialize_with = "serialize_match_ranges")]
    #[schemars(with = "Vec<MatchRange>", required, length(max = 1))]
    match_ranges: Option<MatchRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    section_title: Option<String>,
    #[serde(rename = "ref")]
    source_ref: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    duplicates: Vec<Location<'a>>,
    #[serde(skip)]
    evidence: &'a Evidence,
    #[serde(skip)]
    anchor_start: usize,
}

#[derive(JsonSchema, Serialize)]
struct MatchRange {
    line_start: usize,
    line_end: usize,
}

fn serialize_match_ranges<S: Serializer>(
    ranges: &Option<MatchRange>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    ranges.as_slice().serialize(serializer)
}

#[derive(JsonSchema, Serialize)]
struct Location<'a> {
    library: &'a str,
    path: &'a str,
    line_start: usize,
    line_end: usize,
    #[serde(rename = "ref")]
    source_ref: String,
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    kind: Option<LocationMatch>,
}

#[derive(Clone, Copy, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum LocationMatch {
    SharedPrefix,
}

impl<'a> Location<'a> {
    fn new(evidence: &'a Evidence, range: &Range<usize>, kind: Option<LocationMatch>) -> Self {
        let (line_start, line_end) = lines_for(&evidence.text, range, evidence.line_start);
        Self {
            library: &evidence.library,
            path: &evidence.path,
            line_start,
            line_end,
            source_ref: SourceRef::encode(&evidence.source, range),
            kind,
        }
    }
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

struct Candidate<'a> {
    evidence: &'a Evidence,
    anchor: Range<usize>,
}

struct Hit<'a> {
    evidence: &'a Evidence,
    anchor: Range<usize>,
    range: Range<usize>,
    budget: usize,
    duplicates: Vec<Location<'a>>,
}

struct Known<'a> {
    evidence: &'a Evidence,
    owner: usize,
    shared: Option<Range<usize>>,
}

impl<'a> Candidate<'a> {
    fn range(&self, budget: usize) -> Range<usize> {
        let mut range = window(&self.evidence.text, &self.anchor, budget);
        limit_lines(&self.evidence.text, &self.anchor, &mut range);
        range
    }

    fn new(evidence: &'a Evidence) -> Result<Option<Self>, SearchError> {
        let first_scalar = |start: usize| -> Result<Range<usize>, SearchError> {
            let rest = &evidence.text[start..];
            let len = if rest.starts_with("\r\n") {
                2
            } else {
                rest.chars().next().ok_or(SearchError::Store)?.len_utf8()
            };
            Ok(start..start + len)
        };
        let mut anchor = match &evidence.anchor {
            Some(anchor) => anchor.clone(),
            None if evidence.category == Category::Exact => return Err(SearchError::Store),
            None => first_scalar(0)?,
        };
        if anchor.len() > BYTE_LIMIT
            || source_lines(&evidence.text[anchor.clone()])
                .nth(LINE_LIMIT)
                .is_some()
        {
            if evidence.category == Category::Exact {
                return Ok(None);
            }
            // Related anchors are evidence windows, not literal spans. Keep
            // their starting context even if the composite window is too long.
            anchor = first_scalar(anchor.start)?;
        }
        Ok(Some(Self { evidence, anchor }))
    }
}

impl<'a> SearchOutput<'a> {
    /// Returns the page and the raw ranked prefix consumed, including aliases
    /// and passages that cannot yield a complete bounded literal excerpt.
    pub(super) fn from_reply(
        reply: &'a SearchReply,
        limit: usize,
    ) -> Result<(Self, usize), SearchError> {
        if !(1..=HIT_LIMIT).contains(&limit) {
            return Err(SearchError::InvalidInput("limit must be between 1 and 16"));
        }
        let candidates = reply
            .matches
            .iter()
            .map(Candidate::new)
            .collect::<Result<Vec<_>, _>>()?;
        let mut selected = Vec::<Hit<'_>>::with_capacity(candidates.len().min(limit));
        let mut known = Vec::<Known<'_>>::with_capacity(candidates.len());
        let mut remaining = BYTE_LIMIT;
        let mut consumed = 0;
        while let Some(pending) = candidates.get(consumed) {
            let Some(candidate) = pending else {
                consumed += 1;
                continue;
            };
            let evidence = candidate.evidence;
            if known.iter().any(|copy| overlaps(evidence, copy.evidence)) {
                consumed += 1;
                continue;
            }
            if let Some((owner, shared)) = known
                .iter()
                .find(|copy| evidence.text == copy.evidence.text)
                .map(|copy| (copy.owner, copy.shared.clone()))
            {
                let hit = &mut selected[owner];
                let location = if let Some(range) = &shared {
                    Location::new(evidence, range, Some(LocationMatch::SharedPrefix))
                } else {
                    Location::new(evidence, &hit.range, None)
                };
                hit.duplicates.push(location);
                known.push(Known {
                    evidence,
                    owner,
                    shared,
                });
                consumed += 1;
                continue;
            }

            let slots = limit - selected.len();
            let budget = (slots != 0 && candidate.anchor.len() <= remaining).then(|| {
                let (mut reserved, mut count) = (candidate.anchor.len(), 1);
                let mut reservations = [evidence; HIT_LIMIT];
                for other in candidates[consumed + 1..].iter().flatten() {
                    if count == slots {
                        break;
                    }
                    let already_counted = |previous: &Evidence| {
                        overlaps(previous, other.evidence) || previous.text == other.evidence.text
                    };
                    if reservations[..count]
                        .iter()
                        .any(|prior| already_counted(prior))
                        || known.iter().any(|copy| already_counted(copy.evidence))
                    {
                        continue;
                    }
                    if reserved + other.anchor.len() > remaining {
                        break;
                    }
                    reserved += other.anchor.len();
                    reservations[count] = other.evidence;
                    count += 1;
                }
                let weight = if selected.len() < 2 && count > 1 {
                    2
                } else {
                    1
                };
                candidate.anchor.len() + (remaining - reserved) * weight / (count + weight - 1)
            });
            let range = budget.map(|budget| candidate.range(budget));
            let alias = selected.iter().enumerate().find_map(|(index, hit)| {
                if evidence.category == Category::Exact && hit.evidence.category != Category::Exact
                {
                    return None;
                }
                let other = range
                    .clone()
                    .unwrap_or_else(|| candidate.range(hit.budget.max(candidate.anchor.len())));
                shared_prefix(hit, candidate, &other).map(|range| (index, range))
            });
            if let Some((owner, range)) = alias {
                selected[owner].duplicates.push(Location::new(
                    evidence,
                    &range,
                    Some(LocationMatch::SharedPrefix),
                ));
                known.push(Known {
                    evidence,
                    owner,
                    shared: Some(range),
                });
            } else if let (Some(budget), Some(range)) = (budget, range) {
                remaining -= range.len();
                known.push(Known {
                    evidence,
                    owner: selected.len(),
                    shared: None,
                });
                selected.push(Hit {
                    evidence,
                    anchor: candidate.anchor.clone(),
                    range,
                    budget,
                    duplicates: Vec::new(),
                });
            } else {
                // The next unique hit belongs to the next page. Do not consume
                // it, or reach past it to collect later aliases.
                break;
            }
            consumed += 1;
        }
        let matches = selected
            .into_iter()
            .map(|hit| {
                let match_ranges = (hit.evidence.category == Category::Exact).then(|| {
                    let (line_start, line_end) =
                        lines_for(&hit.evidence.text, &hit.anchor, hit.evidence.line_start);
                    MatchRange {
                        line_start,
                        line_end,
                    }
                });
                SearchHit {
                    excerpt: excerpt(hit.evidence, &hit.range),
                    category: hit.evidence.category,
                    match_ranges,
                    section_title: None,
                    source_ref: SourceRef::encode(&hit.evidence.source, &hit.range),
                    duplicates: hit.duplicates,
                    evidence: hit.evidence,
                    anchor_start: hit.anchor.start,
                }
            })
            .collect();
        Ok((
            Self {
                status: reply.status,
                matches,
                next_cursor: None,
            },
            consumed,
        ))
    }

    pub(super) fn add_section_titles(&mut self, page: &Page) {
        title::add(page, &mut self.matches);
    }
}

fn shared_prefix(
    hit: &Hit<'_>,
    candidate: &Candidate<'_>,
    other: &Range<usize>,
) -> Option<Range<usize>> {
    let text = &hit.evidence.text;
    let alternate = &candidate.evidence.text;
    let mut shared = None;
    for (i, ((left, left_end), (right, right_end))) in normalized(&text[hit.range.clone()])
        .zip(normalized(&alternate[other.clone()]))
        .enumerate()
    {
        if left != right {
            break;
        }
        if i + 1 >= SHARED_PREFIX_MIN
            && let (Some(left_end), Some(right_end)) = (left_end, right_end)
        {
            shared = Some((hit.range.start + left_end, other.start + right_end));
        }
    }
    let (left_end, right_end) = shared?;
    // A shared preamble is insufficient when the anchor's own line differs.
    let anchor_line_end = |text: &str, anchor_end: usize| {
        anchor_end
            + source_lines(&text[anchor_end..])
                .next()
                .map_or(0, |line| line.trim_end().len())
    };
    (left_end >= anchor_line_end(text, hit.anchor.end)
        && right_end >= anchor_line_end(alternate, candidate.anchor.end))
    .then_some(other.start..right_end)
}

// Track each source's own raw endpoint. A folded expansion such as ß -> ss
// can be used as an endpoint only once its entire source scalar has matched.
fn normalized(text: &str) -> impl Iterator<Item = (char, Option<usize>)> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let start = text.len() - trimmed.len();
    let mut source = trimmed.char_indices().peekable();
    std::iter::from_fn(move || {
        let (index, ch) = source.next()?;
        if !ch.is_whitespace() {
            return Some((ch, Some(start + index + ch.len_utf8())));
        }
        while source.peek().is_some_and(|(_, ch)| ch.is_whitespace()) {
            source.next();
        }
        source.peek()?;
        Some((' ', None))
    })
    .flat_map(|(ch, end)| {
        let mut fold = std::iter::once(ch).default_case_fold().peekable();
        std::iter::from_fn(move || {
            let ch = fold.next()?;
            let end = end.filter(|_| fold.peek().is_none());
            Some((ch, end))
        })
    })
}

fn overlaps(left: &Evidence, right: &Evidence) -> bool {
    if left.source.library != right.source.library
        || left.source.version != right.source.version
        || left.path != right.path
    {
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

pub(super) fn limit_lines(source: &str, anchor: &Range<usize>, range: &mut Range<usize>) {
    let text = &source[range.clone()];
    let count = source_lines(text).count();
    if count <= LINE_LIMIT {
        return;
    }
    let anchor = anchor.start - range.start..anchor.end - range.start;
    let (line_start, line_end) = lines_for(text, &anchor, 1);
    let skip = (line_start - 1)
        .saturating_sub(LINE_LIMIT / 3)
        .min(count - LINE_LIMIT)
        .max(line_end.saturating_sub(LINE_LIMIT));
    let mut lines = source_lines(text);
    range.start += lines.by_ref().take(skip).map(str::len).sum::<usize>();
    range.end = range.start + lines.take(LINE_LIMIT).map(str::len).sum::<usize>();
}

pub(super) fn boundary(text: &str, at: usize) -> bool {
    text.is_char_boundary(at) && !(at != 0 && text.as_bytes().get(at - 1..=at) == Some(b"\r\n"))
}
