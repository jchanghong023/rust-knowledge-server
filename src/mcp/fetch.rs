use std::{ops::Range, sync::Arc};

use qdrant_edge::{EdgeShard, JsonPath, PointId, RetrieveRequest, WithPayloadInterface};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    catalog::{Catalog, Version},
    markdown::source_lines,
    search::Source,
};

use super::excerpt::{BYTE_LIMIT, Excerpt, LINE_LIMIT, boundary, limit_lines, lines_for, window};

#[derive(Debug)]
pub(super) enum FetchError {
    InvalidRef,
    StaleRef,
    Store,
}

pub(super) struct SourceRef {
    library: Uuid,
    version: Uuid,
    point: u64,
    target: Target,
}

enum Target {
    Evidence(Range<usize>),
    Next(usize),
    Previous(usize),
}

impl SourceRef {
    pub(super) fn encode(source: &Source, range: &Range<usize>) -> String {
        Self::encode_target(
            source.library,
            source.version,
            source.point,
            source.byte_start + range.start,
            range.len(),
        )
    }

    fn encode_target(
        library: Uuid,
        version: Uuid,
        point: u64,
        start: usize,
        mode: impl std::fmt::Display,
    ) -> String {
        format!(
            "v1.{}.{}.{point}.{start}.{mode}",
            library.simple(),
            version.simple()
        )
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        let mut fields = value.split('.');
        if fields.next()? != "v1" {
            return None;
        }
        let library = Uuid::try_parse(fields.next()?).ok()?;
        let version = Uuid::try_parse(fields.next()?).ok()?;
        let point: u64 = fields.next()?.parse().ok()?;
        let start: usize = fields.next()?.parse().ok()?;
        // Numeric lengths also accept previously issued short-anchor references.
        let target = match fields.next()? {
            "n" => Target::Next(start),
            "p" => Target::Previous(start),
            len => {
                let len: usize = len.parse().ok()?;
                if !(1..=BYTE_LIMIT).contains(&len) {
                    return None;
                }
                Target::Evidence(start..start.checked_add(len)?)
            }
        };
        (point != 0 && fields.next().is_none()).then_some(Self {
            library,
            version,
            point,
            target,
        })
    }

    pub(super) fn pin(&self, catalog: &Catalog) -> Result<Arc<Version>, FetchError> {
        catalog
            .pin_version(self.library, self.version)
            .map_err(|_| FetchError::Store)?
            .ok_or(FetchError::StaleRef)
    }
}

pub(super) struct Fetched {
    version: Arc<Version>,
    path: String,
    source: String,
    range: Range<usize>,
    line_start: usize,
    line_end: usize,
    truncated: bool,
    next_ref: Option<String>,
    previous_ref: Option<String>,
}

#[derive(JsonSchema, Serialize)]
pub(super) struct FetchOutput<'a> {
    #[serde(flatten)]
    excerpt: Excerpt<'a>,
    next_ref: Option<&'a str>,
    previous_ref: Option<&'a str>,
}

impl Fetched {
    pub(super) fn excerpt(&self) -> FetchOutput<'_> {
        FetchOutput {
            excerpt: Excerpt {
                text: &self.source[self.range.clone()],
                library: self.version.library(),
                path: &self.path,
                line_start: self.line_start,
                line_end: self.line_end,
                truncated: self.truncated,
            },
            next_ref: self.next_ref.as_deref(),
            previous_ref: self.previous_ref.as_deref(),
        }
    }
}

pub(super) fn read(version: Arc<Version>, locator: SourceRef) -> Result<Fetched, FetchError> {
    let reader = Reader::new(version.open().map_err(|_| FetchError::Store)?);
    let center = reader
        .passage(locator.point)?
        .ok_or(FetchError::InvalidRef)?;
    let anchor = locator.target.in_passage(&center)?;
    let next_page = matches!(locator.target, Target::Next(_));
    let previous_page = matches!(locator.target, Target::Previous(_));
    let (before, from, prior_bytes) = if next_page {
        (Pieces::new(), anchor.start, 0)
    } else {
        preceding(&reader, locator.point, &center, anchor.start)?
    };
    let (first, earliest) = before
        .last()
        .map_or((&center, from), |(_, passage, range)| {
            (passage, range.start)
        });
    let first_line = first.line_at(earliest);
    let context_start = first.byte_start + earliest;
    let after_bytes = if previous_page {
        0
    } else {
        center.text.len() - anchor.end
    };
    let mut context = Context::new(prior_bytes + anchor.len() + after_bytes.min(BYTE_LIMIT));
    for (id, passage, range) in before.iter().rev() {
        context.push(*id, &passage.text[range.clone()]);
    }
    context.push(locator.point, &center.text[from..anchor.end]);
    let right_truncated = previous_page
        || append_following(&mut context, &reader, locator.point, &center, anchor.end)?;
    let range = if next_page {
        0..source_lines(&context.text)
            .take(LINE_LIMIT)
            .map(str::len)
            .sum()
    } else if previous_page {
        let skip = source_lines(&context.text)
            .count()
            .saturating_sub(LINE_LIMIT);
        source_lines(&context.text).take(skip).map(str::len).sum()..context.text.len()
    } else {
        let anchor = prior_bytes..prior_bytes + anchor.len();
        let mut range = window(&context.text, &anchor, BYTE_LIMIT);
        limit_lines(&context.text, &anchor, &mut range);
        range
    };
    if range.is_empty() {
        return Err(FetchError::InvalidRef);
    }
    let (line_start, line_end) = lines_for(&context.text, &range, first_line);
    // Point cursors at an included byte next to the edge. At a passage seam,
    // the adjacent page can then advance using the same overlap checks as context.
    let reference = |near, offset, mode| {
        SourceRef::encode_target(
            locator.library,
            locator.version,
            context.point_at(near),
            context_start + offset,
            mode,
        )
    };
    let next_ref = (right_truncated || range.end < context.text.len())
        .then(|| reference(range.end - 1, range.end, 'n'));
    let previous_ref =
        (context_start + range.start > 0).then(|| reference(range.start, range.start, 'p'));
    let truncated = next_ref.is_some() || previous_ref.is_some();
    Ok(Fetched {
        version,
        path: center.path,
        source: context.text,
        range,
        line_start,
        line_end,
        truncated,
        next_ref,
        previous_ref,
    })
}

impl Target {
    fn in_passage(&self, passage: &TextPassage) -> Result<Range<usize>, FetchError> {
        let local = |at: usize| at.checked_sub(passage.byte_start);
        let range = match self {
            Self::Evidence(absolute) => {
                let start = local(absolute.start).ok_or(FetchError::InvalidRef)?;
                let end = local(absolute.end).ok_or(FetchError::InvalidRef)?;
                let text = passage.text.get(start..end).ok_or(FetchError::InvalidRef)?;
                if source_lines(text).nth(LINE_LIMIT).is_some() {
                    return Err(FetchError::InvalidRef);
                }
                start..end
            }
            Self::Next(at) => {
                let at = local(*at)
                    .filter(|&at| at > 0)
                    .ok_or(FetchError::InvalidRef)?;
                at..at
            }
            Self::Previous(at) => {
                let at = local(*at)
                    .filter(|&at| at < passage.text.len())
                    .ok_or(FetchError::InvalidRef)?;
                at..at
            }
        };
        if !boundary(&passage.text, range.start) || !boundary(&passage.text, range.end) {
            return Err(FetchError::InvalidRef);
        }
        Ok(range)
    }
}

struct Context {
    text: String,
    // Offsets where source bytes begin contributing from a different point.
    points: Vec<(usize, u64)>,
}

impl Context {
    fn new(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            points: Vec::new(),
        }
    }

    fn push(&mut self, point: u64, text: &str) {
        if !text.is_empty() {
            if self
                .points
                .last()
                .is_none_or(|&(_, previous)| previous != point)
            {
                self.points.push((self.text.len(), point));
            }
            self.text.push_str(text);
        }
    }

    fn point_at(&self, offset: usize) -> u64 {
        self.points[self.points.partition_point(|&(start, _)| start <= offset) - 1].1
    }
}

type Pieces = Vec<(u64, TextPassage, Range<usize>)>;

// Collect only the suffixes that can contribute to the bounded response.
fn preceding(
    reader: &Reader<'_>,
    mut id: u64,
    center: &TextPassage,
    end: usize,
) -> Result<(Pieces, usize, usize), FetchError> {
    let from = suffix_start(&center.text, end, BYTE_LIMIT);
    let mut used = end - from;
    let mut pieces = Pieces::new();
    if from == 0 {
        loop {
            let current = pieces.last().map_or(center, |(_, passage, _)| passage);
            if used >= BYTE_LIMIT || current.byte_start == 0 {
                break;
            }
            id = id
                .checked_sub(1)
                .filter(|&id| id != 0)
                .ok_or(FetchError::Store)?;
            let previous = reader.passage(id)?.ok_or(FetchError::Store)?;
            previous.overlap(current)?;
            let end = current.byte_start - previous.byte_start;
            let start = suffix_start(&previous.text, end, BYTE_LIMIT - used);
            used += end - start;
            if start < end {
                pieces.push((id, previous, start..end));
            }
            if start != 0 {
                break;
            }
        }
    }
    Ok((pieces, from, used))
}

fn suffix_start(text: &str, end: usize, budget: usize) -> usize {
    let mut start = end.saturating_sub(budget);
    while !boundary(text, start) {
        start += 1;
    }
    start
}

fn append_following(
    context: &mut Context,
    reader: &Reader<'_>,
    mut id: u64,
    center: &TextPassage,
    mut start: usize,
) -> Result<bool, FetchError> {
    let mut current = None;
    let mut remaining = BYTE_LIMIT;
    loop {
        let passage = current.as_ref().unwrap_or(center);
        let mut end = (start + remaining).min(passage.text.len());
        while !boundary(&passage.text, end) {
            end -= 1;
        }
        context.push(id, &passage.text[start..end]);
        remaining -= end - start;
        if end != passage.text.len() {
            return Ok(true);
        }
        let Some(next_id) = id.checked_add(1) else {
            return Ok(false);
        };
        let Some(next) = reader.passage(next_id)? else {
            return Ok(false);
        };
        if next.path != center.path {
            return Ok(false);
        }
        start = passage.overlap(&next)?;
        if remaining == 0 {
            return Ok(true);
        }
        id = next_id;
        current = Some(next);
    }
}

pub(super) struct Reader<'a> {
    shard: &'a EdgeShard,
    fields: WithPayloadInterface,
}

impl<'a> Reader<'a> {
    pub(super) fn new(shard: &'a EdgeShard) -> Self {
        let fields = ["text", "path", "line_start", "line_end", "byte_start"];
        Self {
            shard,
            fields: WithPayloadInterface::Fields(
                fields
                    .into_iter()
                    .map(|s| s.parse::<JsonPath>().expect("constant passage field"))
                    .collect(),
            ),
        }
    }

    pub(super) fn passage(&self, id: u64) -> Result<Option<TextPassage>, FetchError> {
        let id = PointId::NumId(id);
        let mut request = RetrieveRequest::new(vec![id]);
        request.with_payload = Some(self.fields.clone());
        let mut records = self
            .shard
            .retrieve(request)
            .map_err(|_| FetchError::Store)?;
        let Some(record) = records.pop() else {
            return Ok(None);
        };
        if record.id != id || !records.is_empty() {
            return Err(FetchError::Store);
        }
        let payload = record.payload.ok_or(FetchError::Store)?;
        let passage: TextPassage =
            serde_json::from_value(Value::Object(payload.0)).map_err(|_| FetchError::Store)?;
        if passage.text.is_empty()
            || passage.line_start == 0
            || passage.line_end < passage.line_start
            || passage.byte_start.checked_add(passage.text.len()).is_none()
        {
            return Err(FetchError::Store);
        }
        Ok(Some(passage))
    }
}

#[derive(Deserialize)]
pub(super) struct TextPassage {
    pub(super) text: String,
    pub(super) path: String,
    pub(super) byte_start: usize,
    pub(super) line_start: usize,
    line_end: usize,
}

impl TextPassage {
    pub(super) fn spans_multiple_lines(&self) -> bool {
        self.line_start < self.line_end
    }

    fn line_at(&self, at: usize) -> usize {
        if at == 0 {
            self.line_start
        } else if at == self.text.len() {
            self.line_end + usize::from(self.text.ends_with(['\r', '\n']))
        } else if self.line_start == self.line_end {
            self.line_start
        } else {
            lines_for(&self.text, &(at..at + 1), self.line_start).0
        }
    }

    fn overlap(&self, next: &Self) -> Result<usize, FetchError> {
        let prefix =
            self.prefix_before(&next.path, &next.text, next.byte_start, next.line_start)?;
        Ok(self.text.len() - prefix.len())
    }

    pub(super) fn prefix_before(
        &self,
        path: &str,
        text: &str,
        byte_start: usize,
        line_start: usize,
    ) -> Result<&str, FetchError> {
        let advance = byte_start
            .checked_sub(self.byte_start)
            .ok_or(FetchError::Store)?;
        if self.path != path || advance == 0 {
            return Err(FetchError::Store);
        }
        let tail = self.text.get(advance..).ok_or(FetchError::Store)?;
        let expected_line = if tail.is_empty() {
            self.line_end.checked_add(1).ok_or(FetchError::Store)?
        } else {
            self.line_end
        };
        if tail.len() >= text.len()
            || !text.starts_with(tail)
            || expected_line != line_start
            || (tail.is_empty() && !self.text.ends_with(['\r', '\n']))
        {
            return Err(FetchError::Store);
        }
        Ok(&self.text[..advance])
    }
}
