//! Literal technical-name boundaries adapted from OMP's `crates/knowledge/src/query.rs` (MIT).

use std::{borrow::Cow, collections::HashMap, ops::Range};

use caseless::Caseless;
use unicode_ident::{is_xid_continue, is_xid_start};
use unicode_script::{Script, UnicodeScript};

/// Leading/trailing whitespace and one matching pair of quotes delimit a query.
pub(crate) fn query_spelling(query: &str) -> &str {
    let trimmed = query.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    match trimmed.as_bytes() {
        [quote @ (b'`' | b'\'' | b'"'), _, .., last] if quote == last => {
            &trimmed[1..trimmed.len() - 1]
        }
        _ => trimmed,
    }
}

#[cfg(test)]
fn query_name(query: &str) -> Option<String> {
    let spelling = query_spelling(query);
    (spelling.chars().all(name_character) && literal_spelling(spelling))
        .then(|| canonical(spelling).into_owned())
}

/// Select a required existing posting for a full match, preferring a numeric
/// standard identifier to an arbitrary word. The raw text is still authoritative.
pub(crate) fn posting_anchor(spelling: &str) -> Option<(Cow<'_, str>, Option<Range<usize>>)> {
    let (standard, _, range) = spellings(spelling, 0..spelling.len())
        .map(|range| {
            (
                standard_number(&spelling[range.clone()]),
                range.len(),
                range,
            )
        })
        .max_by_key(|&(standard, len, _)| (standard, len))?;
    Some((
        canonical(&spelling[range.clone()]),
        standard.then_some(range),
    ))
}

/// Prefer the terms following a standard number: the leading words usually
/// name the standards family. If the number trails the query, use the prefix.
pub(crate) fn standard_topics(spelling: &str, number: Range<usize>) -> impl Iterator<Item = &str> {
    let suffix = number.end..spelling.len();
    let topics = if spellings(spelling, suffix.clone()).next().is_some() {
        suffix
    } else {
        0..number.start
    };
    let mut previous = None;
    spellings(spelling, topics).filter_map(move |range| {
        if previous.replace(range.start) == Some(range.start) {
            None
        } else {
            Some(&spelling[range])
        }
    })
}

fn standard_number(spelling: &str) -> bool {
    let Some((first, rest)) = spelling.split_once('.') else {
        return false;
    };
    first.len() >= 3
        && first.bytes().all(|ch| ch.is_ascii_digit())
        && rest
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|ch| ch.is_ascii_digit()))
}

pub(crate) fn ends_inside_name(spelling: &str, next: Option<char>, after: Option<char>) -> bool {
    let separator = match next {
        Some(ch) if !name_character(ch) => return false,
        Some(ch @ ('.' | ':')) => ch as u8,
        Some(_) => return true,
        None => return false,
    };
    if !prose_separator(separator, spelling.as_bytes()[spelling.len() - 1], after) {
        return true;
    }
    // The historical tokenizer accepts the first prose period/colon as
    // well as a full token, but never uses a double colon to end a name.
    let token = spelling
        .rsplit(|ch| !name_character(ch))
        .next()
        .unwrap_or("");
    token.match_indices(['.', ':']).any(|(at, stop)| {
        at > 0
            && prose_separator(
                stop.as_bytes()[0],
                token.as_bytes()[at - 1],
                token[at + 1..].chars().next().or(next),
            )
    })
}

/// Returns complete names in first-occurrence order, deduplicated by folded spelling.
/// Scan only the section; check both ends of each candidate against the original document.
pub(crate) fn section_names(document: &str, span: Range<usize>) -> Vec<String> {
    let mut names = HashMap::new();
    for spelling in spellings(document, span) {
        let ordinal = names.len();
        names
            .entry(canonical(&document[spelling]))
            .or_insert(ordinal);
    }
    // Moving the keys into their slots preserves order without cloning any names.
    let mut ordered = vec![String::new(); names.len()];
    for (name, ordinal) in names {
        ordered[ordinal] = name.into_owned();
    }
    ordered
}

fn spellings(document: &str, span: Range<usize>) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut chars = document
        .get(span.clone())
        .unwrap_or("")
        .char_indices()
        .peekable();
    std::iter::from_fn(move || {
        while let Some((offset, ch)) = chars.next() {
            if !name_character(ch) {
                continue;
            }
            while chars.next_if(|&(_, next)| name_character(next)).is_some() {}
            let start = span.start + offset;
            let end = chars
                .peek()
                .map_or(span.end, |&(offset, _)| span.start + offset);
            if document[..start]
                .chars()
                .next_back()
                .is_some_and(name_character)
            {
                continue;
            }
            let complete =
                end < span.end || !document[end..].chars().next().is_some_and(name_character);
            return Some(
                [prose_end(document, start, end), complete.then_some(end)]
                    .into_iter()
                    .flatten()
                    .filter_map(move |end| {
                        literal_spelling(&document[start..end]).then_some(start..end)
                    }),
            );
        }
        None
    })
    .flatten()
}

fn canonical(spelling: &str) -> Cow<'_, str> {
    if !spelling.is_ascii() {
        Cow::Owned(spelling.chars().default_case_fold().collect())
    } else if spelling.bytes().any(|ch| ch.is_ascii_uppercase()) {
        Cow::Owned(spelling.to_ascii_lowercase())
    } else {
        Cow::Borrowed(spelling)
    }
}

fn is_han(ch: char) -> bool {
    !ch.is_ascii() && ch.script() == Script::Han
}

fn identifier_start(ch: char) -> bool {
    if ch.is_ascii() {
        ch.is_ascii_alphabetic() || ch == '_'
    } else {
        is_xid_start(ch)
    }
}

fn identifier_continue(ch: char) -> bool {
    if ch.is_ascii() {
        ch.is_ascii_alphanumeric() || ch == '_'
    } else {
        is_xid_continue(ch) || ch.is_alphanumeric()
    }
}

pub(crate) fn name_character(ch: char) -> bool {
    matches!(ch, '+' | '#' | '/' | '\\' | '.' | ':' | '-')
        || (identifier_continue(ch) && !is_han(ch))
}

fn literal_spelling(spelling: &str) -> bool {
    let mut chars = spelling.chars();
    if chars.next().is_some_and(identifier_start) && chars.all(identifier_continue) {
        return true;
    }
    if !spelling.chars().any(char::is_alphanumeric) {
        return false;
    }
    if spelling.contains(['_', '+', '#', '/', '\\']) || spelling.contains("::") {
        return true;
    }
    let unprefixed = spelling.trim_start_matches(['.', '-']);
    if (1..=2).contains(&(spelling.len() - unprefixed.len()))
        && unprefixed
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic())
    {
        return true;
    }
    let mut chars = spelling.chars();
    let (mut left, mut middle) = (chars.next(), chars.next());
    for right in chars {
        if middle.is_some_and(|ch| matches!(ch, '.' | '-'))
            && left.is_some_and(char::is_alphanumeric)
            && right.is_alphanumeric()
        {
            return true;
        }
        (left, middle) = (middle, Some(right));
    }
    false
}

// Keep the first possible prose boundary as well as the full spelling. Inspect
// one extra byte so an original period/colon just outside a section still works.
// A `::` is structural punctuation, not a prose delimiter.
fn prose_end(document: &str, start: usize, end: usize) -> Option<usize> {
    let end_with_boundary =
        end + usize::from(matches!(document.as_bytes().get(end), Some(b'.' | b':')));
    let candidate = &document[start..end_with_boundary];
    candidate
        .match_indices(['.', ':'])
        .find_map(|(offset, stop)| {
            let following = start + offset + 1;
            (offset > 0
                && prose_separator(
                    stop.as_bytes()[0],
                    candidate.as_bytes()[offset - 1],
                    document[following..].chars().next(),
                ))
            .then_some(start + offset)
        })
}

fn prose_separator(separator: u8, preceding: u8, following: Option<char>) -> bool {
    !(separator == b':' && preceding == b':')
        && !following.is_some_and(|ch| ch == ':' || identifier_continue(ch))
}

#[cfg(test)]
mod tests {
    use super::{query_name, section_names};

    #[test]
    fn keeps_complete_spellings_distinct_and_deduplicates_in_order() {
        let source = "C++17 c++ c++ STD::VECTOR::Iterator std::vector Foo.bar Foo.barn";
        let names = section_names(source, 0..source.len());
        assert_eq!(
            names,
            [
                "c++17",
                "c++",
                "std::vector::iterator",
                "std::vector",
                "foo.bar",
                "foo.barn"
            ],
        );
        let near = "C++17 std::vector::iterator Foo.bar.bak";
        let names = section_names(near, 0..near.len());
        for query in ["C++", "std::vector", "Foo.bar"] {
            assert!(!names.contains(&query_name(query).unwrap()), "{query}");
        }
    }

    #[test]
    fn treats_han_as_context_and_folds_non_ascii_identifiers() {
        let source = "中文C++说明𠀀STD::Vector文 Straße ÉTAGE ΛΌΓΟΣ e\u{0301}lan";
        let names = section_names(source, 0..source.len());
        assert_eq!(
            names,
            [
                "c++",
                "std::vector",
                "strasse",
                "étage",
                "λόγοσ",
                "e\u{0301}lan"
            ]
        );
        for query in [
            "c++",
            "std::VECTOR",
            "STRASSE",
            "étage",
            "λόγος",
            "E\u{0301}LAN",
        ] {
            assert!(names.contains(&query_name(query).unwrap()), "{query}");
        }
        assert_eq!(query_name("中文"), None);
        assert_eq!(query_name("𠀀"), None);
    }

    #[test]
    fn quotes_delimit_one_literal_not_an_operator_expression() {
        let source = "`c++17` (C++), 'Std::Vector' \"Foo.bar.\" scanner: _Thing";
        let names = section_names(source, 0..source.len());
        for query in [
            "`C++17`",
            "\"c++\"",
            "'std::vector'",
            "`Foo.bar.`",
            "Foo.bar",
            "SCANNER",
            "_thing",
        ] {
            assert!(names.contains(&query_name(query).unwrap()), "{query}");
        }
        for query in [
            "C++ OR C++17",
            "\"Foo.bar OR scanner\"",
            "scan*",
            "scan.*",
            "regular expression",
        ] {
            assert_eq!(query_name(query), None, "{query}");
        }
    }

    #[test]
    fn rejects_artificial_ends_starts_and_unseen_namespace_context() {
        let source = "before C++17 std::vector::iterator prefixed.name (C++) after";
        for (suffix, len) in [
            ("C++17", 3),
            ("std::vector::", 11),
            ("fixed.name", 10),
            ("vector::iterator", 16),
        ] {
            let start = source.find(suffix).unwrap();
            assert!(
                section_names(source, start..start + len).is_empty(),
                "{suffix}"
            );
        }
        let start = source.rfind("C++").unwrap();
        assert_eq!(section_names(source, start..start + 3), ["c++"]);
        assert!(section_names("C++19", 0..4).is_empty());
        assert!(section_names("std::vector", 0..4).is_empty());
        assert!(section_names("Foo.bar", 0..4).is_empty());
    }

    #[test]
    fn punctuation_runs_do_not_turn_clipped_spellings_into_exact_names() {
        let source = format!("Foo.bar{} Finish", ".".repeat(30_000));
        let clipped = section_names(&source, 0.."Foo.bar.".len());
        assert_eq!(clipped, ["foo.bar"]);
        assert!(!clipped.contains(&query_name("`Foo.bar.`").unwrap()));
        assert_eq!(section_names(&source, 0.."Foo.bar".len()), ["foo.bar"]);
        assert!(section_names(&source, 10_000..20_000).is_empty());
        let continued = "Foo.bar...continuation";
        assert_eq!(
            section_names(continued, 0..continued.len()),
            ["foo.bar", "foo.bar...continuation"]
        );
        assert_eq!(section_names(continued, 0.."Foo.bar.".len()), ["foo.bar"]);
        let continued = "std::vector::iterator";
        assert!(section_names(continued, 0.."std::vector::".len()).is_empty());
    }
}
