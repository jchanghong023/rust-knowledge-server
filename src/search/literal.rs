use std::{borrow::Cow, ops::Range};

use caseless::Caseless;
use qdrant_edge::{EdgeShard, PointId, RetrieveRequest, WithPayloadInterface};
use serde::Deserialize;
use serde_json::Value;

use crate::{markdown, markdown::source_lines, technical};

use super::{MatchTier, PassageText, SearchError};

const MAX_TOPICS: usize = 8;
const TOPIC_RADIUS: usize = 96;
const STANDARD_RADIUS: usize = 512;

pub(super) struct LiteralQuery<'a> {
    pub(super) posting: Option<Cow<'a, str>>,
    full: Matcher,
    standard: Option<Standard>,
}

impl<'a> LiteralQuery<'a> {
    pub(super) fn new(query: &'a str, strict: bool) -> Self {
        let spelling = technical::query_spelling(query);
        let (posting, standard) = technical::posting_anchor(spelling).unzip();
        let standard = standard
            .flatten()
            .filter(|number| !strict && number.len() != spelling.len())
            .map(|number| Standard::new(spelling, number));
        Self {
            posting,
            full: Matcher::new(spelling),
            standard,
        }
    }

    pub(super) fn verify(
        &mut self,
        shard: &EdgeShard,
        id: u64,
        passage: &PassageText,
    ) -> Result<(Option<Range<usize>>, MatchTier), SearchError> {
        let mut context = Context {
            shard,
            id,
            passage,
            before: None,
            after: None,
        };
        let text = &passage.text;
        let mut best = None;
        let mut previous_line = None::<(Range<usize>, MatchTier)>;
        let mut contents = None;
        let mut headings = None;
        self.full.find(text, |span| -> Result<bool, SearchError> {
            if !context.complete(span.clone())? {
                return Ok(false);
            }
            let tier = if let Some((line, tier)) = &previous_line
                && line.start <= span.start
                && span.end <= line.end
            {
                *tier
            } else {
                let is_contents = *contents.get_or_insert_with(|| contents_heading(text));
                let (line, tier) = exact_context(text, &span, is_contents, |span| {
                    let headings =
                        headings.get_or_insert_with(|| markdown::heading_spans(text).peekable());
                    while headings
                        .peek()
                        .is_some_and(|heading| heading.end <= span.start)
                    {
                        headings.next();
                    }
                    headings.peek().is_some_and(|heading| {
                        heading.start <= span.start && span.end <= heading.end
                    })
                });
                previous_line = line.map(|line| (line, tier));
                tier
            };
            if best.as_ref().is_none_or(|(_, ranked)| tier > *ranked) {
                best = Some((span, tier));
            }
            Ok(tier == MatchTier::ExactDetail)
        })?;
        if let Some((span, tier)) = best {
            return Ok((Some(span), tier));
        }
        match &mut self.standard {
            Some(standard) => standard.verify(&mut context),
            None => Ok((None, MatchTier::Related)),
        }
    }
}

fn contents_heading(text: &str) -> bool {
    source_lines(text).next().is_some_and(|first| {
        let first = first.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
        let title = first
            .trim_start_matches('#')
            .trim()
            .trim_end_matches([':', '：']);
        title.eq_ignore_ascii_case("contents")
            || title.eq_ignore_ascii_case("table of contents")
            || title.eq_ignore_ascii_case("index")
            || matches!(title, "目录" | "索引")
    })
}

fn exact_context(
    text: &str,
    span: &Range<usize>,
    in_contents: bool,
    is_heading: impl FnOnce(&Range<usize>) -> bool,
) -> (Option<Range<usize>>, MatchTier) {
    let start = text[..span.start]
        .rfind(['\r', '\n'])
        .map_or(0, |at| at + 1);
    let end = text[span.start..]
        .find(['\r', '\n'])
        .map_or(text.len(), |at| span.start + at);
    if span.end > end {
        return (None, MatchTier::Exact);
    }
    let line = text[start..end].trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let plain_title = start == 0 && line.len() <= 120;
    let cacheable = (!plain_title).then_some(start..end);
    // A named contents/index section stays navigation regardless of its entry
    // format (page number, cross-file link, table row or plain text).
    if (start != 0 && in_contents) || navigation_line(line) {
        return (cacheable, MatchTier::ExactNavigation);
    }

    let mut following = source_lines(&text[end..])
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let next = following.next();
    let atx = line
        .strip_prefix('#')
        .is_some_and(|rest| rest.trim_start_matches('#').starts_with([' ', '\t']));
    let setext = next.is_some_and(|line| {
        line.bytes().all(|byte| byte == b'=') || line.bytes().all(|byte| byte == b'-')
    });
    let standalone = plain_title
        && text[span.end..end]
            .trim_matches([' ', '\t', ':', '：'])
            .is_empty();
    let title = if atx || setext {
        is_heading(span)
    } else {
        standalone || (line.starts_with("**") && line.ends_with("**"))
    };
    if title {
        let explanation = if setext { following.next() } else { next };
        if explanation
            .is_some_and(|line| !navigation_line(line) && line.chars().any(char::is_alphabetic))
        {
            return (cacheable, MatchTier::ExactDetail);
        }
    }
    (cacheable, MatchTier::Exact)
}

fn navigation_line(line: &str) -> bool {
    let link = line.trim_start_matches(['-', '+', '*', ' ']);
    if link.starts_with('[') && link.ends_with(')') && link.contains("](#") {
        return true;
    }
    let title = line.trim_end_matches(|ch: char| ch.is_ascii_digit());
    if title.len() == line.len() {
        return false;
    }
    let label = title.trim_end_matches(char::is_whitespace);
    let spaces = &title[label.len()..];
    if label.ends_with("..") || label.ends_with('…') || label.ends_with("··") {
        return true;
    }
    !label.is_empty() && spaces.contains('\t')
}

struct Standard {
    number: Matcher,
    topics: Vec<Matcher>,
}

impl Standard {
    fn new(spelling: &str, number: Range<usize>) -> Self {
        let matcher = Matcher::new(&spelling[number.clone()]);
        let mut topics: Vec<Matcher> = Vec::new();
        for topic in technical::standard_topics(spelling, number) {
            let topic = Matcher::new(topic);
            if topics.iter().any(|other| other.pattern == topic.pattern) {
                continue;
            }
            if topics.len() == MAX_TOPICS {
                // A partial set would promote evidence missing requested terms.
                topics.clear();
                break;
            }
            topics.push(topic);
        }
        Self {
            number: matcher,
            topics,
        }
    }

    fn verify(
        &mut self,
        context: &mut Context<'_>,
    ) -> Result<(Option<Range<usize>>, MatchTier), SearchError> {
        let passage = context.passage;
        let text = &passage.text;
        let number = self.number.find(text, |span| context.complete(span))?;
        let mut topic_anchor = None;
        let mut target_anchor = None;
        let mut direct = false;
        if let Some((pivot, rest)) = self.topics.split_first_mut() {
            let hit = pivot.find(text, |pivot| -> Result<bool, SearchError> {
                if pivot.len() > TOPIC_RADIUS || !context.complete(pivot.clone())? {
                    return Ok(false);
                }
                let mut topic = pivot;
                for term in rest.iter_mut() {
                    let nearby = text.ceil_char_boundary(topic.end.saturating_sub(TOPIC_RADIUS))
                        ..text.floor_char_boundary((topic.start + TOPIC_RADIUS).min(text.len()));
                    let Some(span) = context.find_in(term, nearby, |_| true)? else {
                        return Ok(false);
                    };
                    topic.start = topic.start.min(span.start);
                    topic.end = topic.end.max(span.end);
                }
                if number.is_some() {
                    let target = around(text, &topic, STANDARD_RADIUS);
                    let direct_number = context.find_in(&mut self.number, target, |number| {
                        let combined = topic.start.min(number.start)..topic.end.max(number.end);
                        if same_sentence(&text[combined.clone()]) {
                            return true;
                        }
                        // Preserve the answer when the provenance is farther away.
                        target_anchor.get_or_insert(topic.clone());
                        false
                    })?;
                    if let Some(number) = direct_number {
                        let combined = topic.start.min(number.start)..topic.end.max(number.end);
                        target_anchor = Some(if combined.len() <= 2 * TOPIC_RADIUS {
                            combined
                        } else {
                            topic
                        });
                        direct = true;
                        return Ok(true);
                    }
                }
                topic_anchor.get_or_insert(topic);
                // With no target, the first complete topic is enough.
                Ok(number.is_none())
            })?;
            if hit.is_some() && direct {
                return Ok((target_anchor, MatchTier::DirectTopic));
            }
            if target_anchor.is_some() {
                return Ok((target_anchor, MatchTier::TargetTopic));
            }
        }
        if topic_anchor.is_some() {
            Ok((topic_anchor, MatchTier::Topic))
        } else if number.is_some() {
            Ok((number, MatchTier::Identifier))
        } else {
            Ok((None, MatchTier::Related))
        }
    }
}

fn same_sentence(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '。' | '！' | '？')
            || (matches!(ch, '.' | '!' | '?') && chars.peek().is_some_and(|ch| ch.is_whitespace()))
        {
            return false;
        }
    }
    true
}

fn around(text: &str, span: &Range<usize>, radius: usize) -> Range<usize> {
    text.ceil_char_boundary(span.start.saturating_sub(radius))
        ..text.floor_char_boundary((span.end + radius).min(text.len()))
}

/// Streaming KMP over case-folded scalars. Only raw character boundaries may
/// begin/end a match (e.g. `s` must not match one half of the fold of `ß`).
/// Reuse the bounded origin ring between passages, without making folded copies.
struct Matcher {
    pattern: Vec<char>,
    prefix: Vec<usize>,
    origins: Vec<Option<usize>>,
}

impl Matcher {
    fn new(spelling: &str) -> Self {
        let pattern: Vec<_> = spelling.chars().default_case_fold().collect();
        let mut prefix = vec![0; pattern.len()];
        for next in 1..pattern.len() {
            prefix[next] = Self::advance(&pattern, &prefix, prefix[next - 1], pattern[next]);
        }
        Self {
            pattern,
            prefix,
            origins: Vec::new(),
        }
    }

    fn advance(pattern: &[char], prefix: &[usize], mut count: usize, ch: char) -> usize {
        while count > 0 && pattern[count] != ch {
            count = prefix[count - 1];
        }
        count + usize::from(pattern[count] == ch)
    }

    fn find<E>(
        &mut self,
        text: &str,
        mut complete: impl FnMut(Range<usize>) -> Result<bool, E>,
    ) -> Result<Option<Range<usize>>, E> {
        let len = self.pattern.len();
        let mut count = 0;
        let mut cursor = 0;
        if len == 0 {
            return Ok(None);
        }
        for (start, ch) in text.char_indices() {
            if ch.is_ascii() {
                // In a purely ASCII passage the byte offset is the origin.
                // In a mixed passage enter the origin in the same ring as folds.
                if !self.origins.is_empty() {
                    self.origins[cursor] = Some(start);
                    cursor = if cursor + 1 == len { 0 } else { cursor + 1 };
                }
                count = Self::advance(&self.pattern, &self.prefix, count, ch.to_ascii_lowercase());
            } else {
                if self.origins.is_empty() {
                    self.origins.resize(len, None);
                    // At most len-1 ASCII characters can be part of a pending prefix.
                    let prefix_start = start.saturating_sub(len - 1);
                    for position in prefix_start..start {
                        self.origins[cursor] = Some(position);
                        cursor += 1;
                    }
                }
                let mut fold = std::iter::once(ch).default_case_fold().peekable();
                let mut origin = Some(start);
                while let Some(folded) = fold.next() {
                    self.origins[cursor] = origin.take();
                    cursor = if cursor + 1 == len { 0 } else { cursor + 1 };
                    count = Self::advance(&self.pattern, &self.prefix, count, folded);
                    if count == len && fold.peek().is_some() {
                        count = self.prefix[len - 1];
                    }
                }
            }
            if count == len {
                let end = start + ch.len_utf8();
                let from = if self.origins.is_empty() {
                    Some(end - len)
                } else {
                    self.origins[cursor]
                };
                if let Some(from) = from {
                    let span = from..end;
                    if complete(span.clone())? {
                        return Ok(Some(span));
                    }
                }
                count = self.prefix[len - 1];
            }
        }
        Ok(None)
    }
}

struct Context<'a> {
    shard: &'a EdgeShard,
    id: u64,
    passage: &'a PassageText,
    before: Option<Option<char>>,
    after: Option<[Option<char>; 2]>,
}

impl Context<'_> {
    fn find_in(
        &mut self,
        matcher: &mut Matcher,
        window: Range<usize>,
        mut accept: impl FnMut(&Range<usize>) -> bool,
    ) -> Result<Option<Range<usize>>, SearchError> {
        let passage = self.passage;
        let start = window.start;
        let to_source = |span: Range<usize>| start + span.start..start + span.end;
        matcher
            .find(&passage.text[window], |span| {
                let span = to_source(span);
                Ok(self.complete(span.clone())? && accept(&span))
            })
            .map(|span| span.map(to_source))
    }

    fn complete(&mut self, span: Range<usize>) -> Result<bool, SearchError> {
        let passage = self.passage;
        let text = &passage.text;
        // Excerpts cannot start or end in the middle of a source line ending.
        let splits_crlf = |at| at != 0 && text.as_bytes().get(at - 1..=at) == Some(b"\r\n");
        if splits_crlf(span.start) || splits_crlf(span.end) {
            return Ok(false);
        }
        let spelling = &text[span.clone()];
        let first = spelling.chars().next().ok_or(SearchError::Store)?;
        if technical::name_character(first) {
            let before = if span.start > 0 {
                text[..span.start].chars().next_back()
            } else {
                self.previous_char()?
            };
            if before.is_some_and(technical::name_character) {
                return Ok(false);
            }
        }
        let last = spelling.chars().next_back().ok_or(SearchError::Store)?;
        if !technical::name_character(last) {
            return Ok(true);
        }
        let mut chars = text[span.end..].chars();
        let next = match chars.next() {
            Some(ch) => Some(ch),
            None => self.next_chars()?[0],
        };
        let after = if matches!(next, Some('.' | ':')) {
            match chars.next() {
                Some(ch) => Some(ch),
                None => self.next_chars()?[usize::from(span.end == text.len())],
            }
        } else {
            None
        };
        Ok(!technical::ends_inside_name(spelling, next, after))
    }

    fn previous_char(&mut self) -> Result<Option<char>, SearchError> {
        if let Some(ch) = self.before {
            return Ok(ch);
        }
        let start = self.passage.byte_start;
        let ch = if start == 0 {
            None
        } else {
            let id = self.id.checked_sub(1).ok_or(SearchError::Store)?;
            let previous = self.neighbor(id)?.ok_or(SearchError::Store)?;
            let end = start
                .checked_sub(previous.byte_start)
                .ok_or(SearchError::Store)?;
            let ch = previous
                .text
                .get(..end)
                .and_then(|s| s.chars().next_back())
                .ok_or(SearchError::Store)?;
            Some(ch)
        };
        self.before = Some(ch);
        Ok(ch)
    }

    fn next_chars(&mut self) -> Result<[Option<char>; 2], SearchError> {
        if let Some(chars) = self.after {
            return Ok(chars);
        }
        let mut chars = [None; 2];
        if let Some(id) = self.id.checked_add(1)
            && let Some(next) = self.neighbor(id)?
        {
            let end = self.passage.byte_start + self.passage.text.len();
            let overlap = end.checked_sub(next.byte_start).ok_or(SearchError::Store)?;
            let rest = next.text.get(overlap..).ok_or(SearchError::Store)?;
            for (slot, ch) in chars.iter_mut().zip(rest.chars()) {
                *slot = Some(ch);
            }
        }
        self.after = Some(chars);
        Ok(chars)
    }

    fn neighbor(&self, id: u64) -> Result<Option<Neighbor>, SearchError> {
        let id = PointId::NumId(id);
        let mut request = RetrieveRequest::new(vec![id]);
        request.with_payload = Some(WithPayloadInterface::Fields(
            ["text", "path", "byte_start"]
                .into_iter()
                .map(|field| field.parse().expect("constant Edge payload field"))
                .collect(),
        ));
        let Some(record) = self
            .shard
            .retrieve(request)
            .map_err(|_| SearchError::Store)?
            .pop()
        else {
            return Ok(None);
        };
        if record.id != id {
            return Err(SearchError::Store);
        }
        let payload = record.payload.ok_or(SearchError::Store)?;
        if payload.0.get("path").and_then(Value::as_str) != Some(self.passage.path.as_str()) {
            return Ok(None);
        }
        serde_json::from_value(Value::Object(payload.0))
            .map(Some)
            .map_err(|_| SearchError::Store)
    }
}

#[derive(Deserialize)]
struct Neighbor {
    text: String,
    byte_start: usize,
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    #[test]
    fn folded_stream_only_accepts_whole_source_characters() {
        let accept = |_| Ok::<_, Infallible>(true);
        let mut single = Matcher::new("s");
        assert_eq!(single.find("ß", accept).unwrap(), None);
        let mut word = Matcher::new("STRASSE");
        for (text, expected) in [
            ("Straße", Some(0..7)),
            ("中STRASSE", Some(3..10)),
            ("STRASSE", Some(0..7)),
            ("Straß", None),
        ] {
            assert_eq!(word.find(text, accept).unwrap(), expected);
        }
    }
}
