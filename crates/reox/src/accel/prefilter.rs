//! Where a match may start: the characters every match of part of a pattern
//! starts with, found from its tree. Their first bytes let a search skip
//! the places no match starts at, and a loop followed by a part that never
//! starts with what the loop takes need never give any of it back.

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

use crate::syntax::{Node, Repeat};

// ========================================================================
// Constants
// ========================================================================

/// How many distinct start bytes `memchr` looks for; past it, a table.
const MEMCHR_NEEDLES: usize = 3;

/// The code points where each length of UTF-8 encoding starts and ends:
/// lead bytes rise with the code point within each.
const UTF8_LENGTHS: [(u32, u32); 4] = [
    (0, 0x7F),
    (0x80, 0x7FF),
    (0x800, 0xFFFF),
    (0x1_0000, 0x10_FFFF),
];

// ========================================================================
// Data Structures
// ========================================================================

/// The bytes every match of a pattern starts with.
#[derive(Debug)]
pub(crate) struct StartBytes {
    /// The bytes, when few enough for `memchr`.
    needles: Option<Box<[u8]>>,
    /// Whether each byte is one.
    table: [bool; 256],
}

/// An end of a match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edge {
    /// Where it starts.
    First,
    /// Where it ends.
    Last,
}

/// Which characters the matches of part of a pattern may start (or end)
/// with.
#[derive(Clone, Debug)]
enum Start {
    /// A match may start with any character.
    Any,
    /// Every match starts with one of these characters.
    Definite(ClassUnicode),
    /// Every match is empty or starts with one of these characters.
    MaybeEmpty(ClassUnicode),
}

// ========================================================================
// Start Implementations
// ========================================================================

impl Start {
    /// The start of `parts` matched one after another: the first that must
    /// match decides, with the characters of those before it that may not.
    fn concat(parts: impl Iterator<Item = Self>) -> Self {
        let mut chars = ClassUnicode::empty();
        for part in parts {
            match part {
                Self::Definite(more) => {
                    chars.union(&more);
                    return Self::Definite(chars);
                }
                Self::MaybeEmpty(more) => chars.union(&more),
                Self::Any => return Self::Any,
            }
        }
        Self::MaybeEmpty(chars)
    }

    /// The start of any of `alternatives`.
    fn alt(alternatives: impl Iterator<Item = Self>) -> Self {
        let mut chars = ClassUnicode::empty();
        let mut definite = true;
        for alternative in alternatives {
            match alternative {
                Self::Definite(more) => chars.union(&more),
                Self::MaybeEmpty(more) => {
                    chars.union(&more);
                    definite = false;
                }
                Self::Any => return Self::Any,
            }
        }
        match definite {
            true => Self::Definite(chars),
            false => Self::MaybeEmpty(chars),
        }
    }

    /// The start of what matches this or nothing.
    fn optional(self) -> Self {
        match self {
            Self::Definite(chars) | Self::MaybeEmpty(chars) => Self::MaybeEmpty(chars),
            Self::Any => Self::Any,
        }
    }
}

// ========================================================================
// StartBytes Implementations
// ========================================================================

impl StartBytes {
    /// The bytes every match of `node` starts with; `None` when a match may
    /// start with any byte, or be empty.
    pub(crate) fn of(node: &Node) -> Option<Self> {
        let chars = first_chars(node)?;
        let mut table = [false; 256];
        for range in chars.ranges() {
            let (start, end) = (u32::from(range.start()), u32::from(range.end()));
            for (low, high) in UTF8_LENGTHS {
                if start <= high && end >= low {
                    for byte in lead_byte(start.max(low))..=lead_byte(end.min(high)) {
                        table[usize::from(byte)] = true;
                    }
                }
            }
        }
        let needles: Vec<u8> = (0..=u8::MAX)
            .filter(|&byte| table[usize::from(byte)])
            .collect();
        if needles.is_empty() {
            return None;
        }
        Some(Self {
            needles: (needles.len() <= MEMCHR_NEEDLES).then(|| needles.into()),
            table,
        })
    }

    /// The first place from `from` in `haystack` holding one of the bytes:
    /// a character's start, as no lead byte continues a character.
    pub(crate) fn find(&self, haystack: &[u8], from: usize) -> Option<usize> {
        let rest = haystack.get(from..)?;
        let found = match self.needles.as_deref() {
            Some(&[a]) => memchr::memchr(a, rest),
            Some(&[a, b]) => memchr::memchr2(a, b, rest),
            Some(&[a, b, c]) => memchr::memchr3(a, b, c, rest),
            Some(_) | None => rest.iter().position(|&byte| self.table[usize::from(byte)]),
        };
        found.map(|at| from + at)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The characters every match of `node` starts with; `None` when a match
/// may start with any, or be empty.
pub(crate) fn first_chars(node: &Node) -> Option<ClassUnicode> {
    definite(edge(node, Edge::First))
}

/// The characters every match of `node` ends with; `None` when a match may
/// end with any, or be empty.
pub(crate) fn last_chars(node: &Node) -> Option<ClassUnicode> {
    definite(edge(node, Edge::Last))
}

/// The characters of `start`, if every match has one.
fn definite(start: Start) -> Option<ClassUnicode> {
    match start {
        Start::Definite(chars) if !chars.ranges().is_empty() => Some(chars),
        Start::Any | Start::Definite(_) | Start::MaybeEmpty(_) => None,
    }
}

/// Which characters the matches of `node` may start or end with.
fn edge(node: &Node, side: Edge) -> Start {
    match node {
        Node::Alt(alternatives) => Start::alt(alternatives.iter().map(|alt| edge(alt, side))),
        Node::Any | Node::AnyButNewline | Node::BackRef(_) | Node::Call(_) => Start::Any,
        Node::Assert(_) | Node::Empty | Node::Keep => Start::MaybeEmpty(ClassUnicode::empty()),
        Node::Atomic(inner) => edge(inner, side),
        Node::Capture(capture) => edge(&capture.node, side),
        Node::Conditional(cond) => {
            Start::alt([edge(&cond.yes, side), edge(&cond.no, side)].into_iter())
        }
        Node::Char(c) => Start::Definite(ClassUnicode::new([ClassUnicodeRange::new(*c, *c)])),
        Node::Class(class) => Start::Definite(class.clone()),
        Node::Concat(parts) => match side {
            Edge::First => Start::concat(parts.iter().map(|part| edge(part, side))),
            Edge::Last => Start::concat(parts.iter().rev().map(|part| edge(part, side))),
        },
        // A lookahead at the start says what the match starts with; any
        // other lookaround says nothing about either end.
        Node::Look(look) if side == Edge::First && !look.behind && !look.negated => {
            match edge(&look.node, side) {
                Start::Definite(chars) => Start::Definite(chars),
                Start::Any | Start::MaybeEmpty(_) => Start::MaybeEmpty(ClassUnicode::empty()),
            }
        }
        Node::Look(_) => Start::MaybeEmpty(ClassUnicode::empty()),
        Node::Repeat(Repeat { min: 0, node, .. }) => edge(node, side).optional(),
        Node::Repeat(repeat) => edge(&repeat.node, side),
    }
}

/// The first byte of the code point `code` in UTF-8.
fn lead_byte(code: u32) -> u8 {
    let lead = match code {
        0..0x80 => code,
        0x80..0x800 => 0xC0 | code >> 6,
        0x800..0x1_0000 => 0xE0 | code >> 12,
        _ => 0xF0 | code >> 18,
    };
    lead as u8
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{Flags, parse};

    fn starts(pattern: &str) -> Option<Vec<u8>> {
        let parsed = parse(pattern, Flags::default()).expect("parses");
        StartBytes::of(&parsed.node).map(|start| {
            (0..=u8::MAX)
                .filter(|&byte| start.table[usize::from(byte)])
                .collect()
        })
    }

    #[test]
    fn test_start_bytes_cover_every_way_a_match_starts() {
        assert_eq!(
            starts(r"\s*(?:fn|let)\b"),
            Some(b"\t\n\x0b\x0c\r fl\xc2\xe1\xe2\xe3".to_vec())
        );
        assert_eq!(starts(r"a?b"), Some(b"ab".to_vec()));
        assert_eq!(
            starts(r"(?i)k"),
            Some(b"Kk\xe2".to_vec()),
            "the Kelvin sign too"
        );
        assert_eq!(
            starts(r"(?=x)\w"),
            Some(b"x".to_vec()),
            "a leading lookahead"
        );
        assert_eq!(starts(r"é|z"), Some(b"z\xc3".to_vec()));
    }

    #[test]
    fn test_patterns_that_may_start_anywhere_have_no_start_bytes() {
        assert_eq!(starts(r"a*"), None, "may be empty");
        assert_eq!(
            starts(r"(?!x)a"),
            Some(b"a".to_vec()),
            "a negative lookahead says nothing"
        );
        assert_eq!(starts(r".b"), None);
        assert_eq!(starts(r"(a)\1"), Some(b"a".to_vec()));
        assert_eq!(starts(r"\1(a)"), None, "a back-reference may hold anything");
    }

    #[test]
    fn test_finding_a_start_skips_to_the_next_lead_byte() {
        let parsed = parse("é|z", Flags::default()).expect("parses");
        let start = StartBytes::of(&parsed.node).expect("start bytes");
        assert_eq!(start.find("aaéz".as_bytes(), 0), Some(2));
        assert_eq!(
            start.find("aaéz".as_bytes(), 3),
            Some(4),
            "never a continuation byte"
        );
        assert_eq!(start.find(b"aaa", 0), None);
    }

    #[test]
    fn test_last_characters_read_a_match_from_its_end() {
        let last = |pattern: &str| {
            last_chars(&parse(pattern, Flags::default()).expect("parses").node)
                .map(|class| format!("{class:?}"))
        };
        assert_eq!(last(r">\s*\("), last(r"\("));
        assert_eq!(last(r"ab?"), last(r"[ab]"), "an optional end");
        assert_eq!(last(r"a.*"), None, "may end with anything");
    }
}
