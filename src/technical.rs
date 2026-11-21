//! Literal technical-name boundaries adapted from OMP's `crates/knowledge/src/query.rs` (MIT).

use std::{borrow::Cow, collections::HashMap, ops::Range};

use caseless::Caseless;
use unicode_ident::{is_xid_continue, is_xid_start};
use unicode_script::{Script, UnicodeScript};

/// Recognizes a single literal spelling, optionally surrounded by matching quotes.
/// Multi-word queries are left to ordinary retrieval; no operator syntax is parsed.
pub(crate) fn query_name(query: &str) -> Option<String> {
    let trimmed = query.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let spelling = match trimmed.as_bytes() {
        [quote @ (b'`' | b'\'' | b'"'), .., last] if quote == last => {
            &trimmed[1..trimmed.len() - 1]
        }
        _ => trimmed,
    };
    (spelling.chars().all(name_character) && literal_spelling(spelling))
        .then(|| canonical(spelling).into_owned())
}

/// Returns complete names in first-occurrence order, deduplicated by folded spelling.
/// Scan only the section; check both ends of each candidate against the original document.
pub(crate) fn section_names(document: &str, span: Range<usize>) -> Vec<String> {
    let Some(section) = document.get(span.clone()) else {
        return Vec::new();
    };
    let mut names = HashMap::new();
    let mut chars = section.char_indices().peekable();
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
        for candidate_end in [prose_end(document, start, end), complete.then_some(end)]
            .into_iter()
            .flatten()
        {
            let candidate = &document[start..candidate_end];
            if literal_spelling(candidate) {
                let ordinal = names.len();
                names.entry(canonical(candidate)).or_insert(ordinal);
            }
        }
    }
    // Moving the keys into their slots preserves order without cloning any names.
    let mut ordered = vec![String::new(); names.len()];
    for (name, ordinal) in names {
        ordered[ordinal] = name.into_owned();
    }
    ordered
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

fn name_character(ch: char) -> bool {
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
            if offset == 0 || (stop == ":" && candidate.as_bytes()[offset - 1] == b':') {
                return None;
            }
            let following = start + offset + 1;
            (!document[following..]
                .chars()
                .next()
                .is_some_and(|ch| ch == ':' || identifier_continue(ch)))
            .then_some(start + offset)
        })
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
