use std::{ops::Range, sync::Arc};

use qdrant_edge::{EdgeShard, JsonPath, PointId, RetrieveRequest, WithPayloadInterface};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    catalog::{Catalog, Version},
    markdown::source_lines,
    search::Source,
};

use super::excerpt::{BYTE_LIMIT, Excerpt, boundary, lines_for, window};

const LINE_LIMIT: usize = 60;

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
    absolute: Range<usize>,
}

impl SourceRef {
    pub(super) fn encode(source: &Source, anchor: &Range<usize>) -> String {
        format!(
            "v1.{}.{}.{}.{}.{}",
            source.library.simple(),
            source.version.simple(),
            source.point,
            source.byte_start + anchor.start,
            anchor.len()
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
        let len: usize = fields.next()?.parse().ok()?;
        (point != 0 && (1..=BYTE_LIMIT).contains(&len) && fields.next().is_none()).then_some(Self {
            library,
            version,
            point,
            absolute: start..start.checked_add(len)?,
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
}

impl Fetched {
    pub(super) fn excerpt(&self) -> Excerpt<'_> {
        Excerpt {
            text: &self.source[self.range.clone()],
            library: self.version.library(),
            path: &self.path,
            line_start: self.line_start,
            line_end: self.line_end,
            truncated: self.truncated,
        }
    }
}

pub(super) fn read(version: Arc<Version>, locator: SourceRef) -> Result<Fetched, FetchError> {
    let reader = Reader::new(version.open().map_err(|_| FetchError::Store)?);
    let center = reader
        .passage(locator.point)?
        .ok_or(FetchError::InvalidRef)?;
    let start = locator
        .absolute
        .start
        .checked_sub(center.byte_start)
        .ok_or(FetchError::InvalidRef)?;
    let anchor = start..start + locator.absolute.len();
    let anchor_text = center
        .text
        .get(anchor.clone())
        .ok_or(FetchError::InvalidRef)?;
    // A reference points at a name or a single character, never across physical lines.
    if !boundary(&center.text, anchor.start)
        || !boundary(&center.text, anchor.end)
        || source_lines(anchor_text).count() != 1
    {
        return Err(FetchError::InvalidRef);
    }

    let (before, from, prior_bytes) = preceding(&reader, locator.point, &center, anchor.start)?;
    let (first, earliest) = before
        .last()
        .map_or((&center, from), |(passage, range)| (passage, range.start));
    let first_line = if earliest == 0 {
        first.line_start
    } else {
        lines_for(&first.text, &(earliest..earliest + 1), first.line_start).0
    };
    let left_truncated = first.byte_start + earliest != 0;
    let mut context = String::with_capacity(
        prior_bytes + anchor.len() + (center.text.len() - anchor.end).min(BYTE_LIMIT),
    );
    for (passage, range) in before.iter().rev() {
        context.push_str(&passage.text[range.clone()]);
    }
    context.push_str(&center.text[from..anchor.end]);
    let right_truncated =
        append_following(&mut context, &reader, locator.point, &center, anchor.end)?;
    let anchor = prior_bytes..prior_bytes + anchor.len();
    let mut range = window(&context, &anchor, BYTE_LIMIT);
    limit_lines(&context, &anchor, &mut range);
    let (line_start, line_end) = lines_for(&context, &range, first_line);
    let truncated = left_truncated || right_truncated || range.len() < context.len();
    Ok(Fetched {
        version,
        path: center.path,
        source: context,
        range,
        line_start,
        line_end,
        truncated,
    })
}

type Pieces = Vec<(TextPassage, Range<usize>)>;

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
            let current = pieces.last().map_or(center, |(passage, _)| passage);
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
                pieces.push((previous, start..end));
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
    context: &mut String,
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
        context.push_str(&passage.text[start..end]);
        remaining -= end - start;
        if end != passage.text.len() || remaining == 0 {
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
        id = next_id;
        current = Some(next);
    }
}

fn limit_lines(source: &str, anchor: &Range<usize>, range: &mut Range<usize>) {
    let text = &source[range.clone()];
    let count = source_lines(text).count();
    if count <= LINE_LIMIT {
        return;
    }
    let offset = range.start;
    let anchor = anchor.start - offset..anchor.end - offset;
    let (line, _) = lines_for(text, &anchor, 1);
    let skip = (line - 1)
        .saturating_sub(LINE_LIMIT / 3)
        .min(count - LINE_LIMIT);
    let mut lines = source_lines(text);
    range.start += lines.by_ref().take(skip).map(str::len).sum::<usize>();
    range.end = range.start + lines.take(LINE_LIMIT).map(str::len).sum::<usize>();
}

struct Reader<'a> {
    shard: &'a EdgeShard,
    fields: WithPayloadInterface,
}

impl<'a> Reader<'a> {
    fn new(shard: &'a EdgeShard) -> Self {
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

    fn passage(&self, id: u64) -> Result<Option<TextPassage>, FetchError> {
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
struct TextPassage {
    text: String,
    path: String,
    byte_start: usize,
    line_start: usize,
    line_end: usize,
}

impl TextPassage {
    fn overlap(&self, next: &Self) -> Result<usize, FetchError> {
        let advance = next
            .byte_start
            .checked_sub(self.byte_start)
            .ok_or(FetchError::Store)?;
        if self.path != next.path || advance == 0 {
            return Err(FetchError::Store);
        }
        let tail = self.text.get(advance..).ok_or(FetchError::Store)?;
        let expected_line = if tail.is_empty() {
            self.line_end.checked_add(1).ok_or(FetchError::Store)?
        } else {
            self.line_end
        };
        if tail.len() >= next.text.len()
            || !next.text.starts_with(tail)
            || expected_line != next.line_start
            || (tail.is_empty() && !self.text.ends_with(['\r', '\n']))
        {
            return Err(FetchError::Store);
        }
        Ok(tail.len())
    }
}
