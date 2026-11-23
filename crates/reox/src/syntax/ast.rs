//! Parsed patterns: the tree the parser builds and the compiler reads.

use regex_syntax::hir::ClassUnicode;

// ========================================================================
// Data Structures
// ========================================================================

/// A parsed pattern, or part of one.
#[derive(Clone, Debug)]
pub(crate) enum Node {
    /// Alternatives, tried in order: the first that leads to a match wins.
    Alt(Vec<Node>),
    /// Any character (`\O`, or `.` with Oniguruma's `m`).
    Any,
    /// Any character but a newline (`.`, `\N`).
    AnyButNewline,
    /// A position the match must be at (`^`, `\b`, `\G`).
    Assert(Assertion),
    /// A part that, once matched, is never matched another way (`(?>...)`).
    Atomic(Box<Node>),
    /// The text a group matched, matched again (`\1`, `\k<name>`).
    BackRef(BackRef),
    /// A subroutine call: the group numbered, matched here as it is
    /// defined (`\g<name>`), group 0 being the whole pattern.
    Call(usize),
    /// A capturing group.
    Capture(Capture),
    /// One character.
    Char(char),
    /// One character of a set, already case-folded where the flags say.
    Class(ClassUnicode),
    /// Parts matched one after another.
    Concat(Vec<Node>),
    /// Nothing.
    Empty,
    /// Where the match is reported to start, moved here (`\K`).
    Keep,
    /// A lookahead or lookbehind.
    Look(Look),
    /// A repetition.
    Repeat(Repeat),
}

/// A position a match must be at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Assertion {
    /// Not an ASCII word boundary (`\B` under `(?W)`).
    AsciiNotWordBoundary,
    /// An ASCII word boundary (`\b` under `(?W)`).
    AsciiWordBoundary,
    /// The end of a line (`$`): before a newline or at the text's end.
    LineEnd,
    /// The start of a line (`^`): the text's start, or after a newline that
    /// does not end the text.
    LineStart,
    /// Not a word boundary (`\B`).
    NotWordBoundary,
    /// Where the search started (`\G`).
    SearchStart,
    /// The text's end (`\z`).
    TextEnd,
    /// The text's end, or before a newline ending it (`\Z`).
    TextEndNewline,
    /// The text's start (`\A`).
    TextStart,
    /// A word boundary (`\b`).
    WordBoundary,
}

/// A back-reference.
#[derive(Clone, Debug)]
pub(crate) struct BackRef {
    /// Whether the text is matched again ignoring case.
    pub(crate) casei: bool,
    /// The groups it may refer to, the last defined tried first: several
    /// for a name several groups share.
    pub(crate) groups: Box<[usize]>,
}

/// A capturing group.
#[derive(Clone, Debug)]
pub(crate) struct Capture {
    /// Its number, counted by opening parenthesis from 1.
    pub(crate) index: usize,
    /// What it matches.
    pub(crate) node: Box<Node>,
}

/// A lookaround.
#[derive(Clone, Debug)]
pub(crate) struct Look {
    /// Whether it looks behind (`(?<=...)`) rather than ahead.
    pub(crate) behind: bool,
    /// Whether it asserts its part does not match (`(?!...)`).
    pub(crate) negated: bool,
    /// The part it looks for.
    pub(crate) node: Box<Node>,
}

/// A repetition.
#[derive(Clone, Debug)]
pub(crate) struct Repeat {
    /// Whether it takes as many as it can first (`*`), or as few (`*?`).
    pub(crate) greedy: bool,
    /// The most times it repeats; `None` for no limit.
    pub(crate) max: Option<u32>,
    /// The fewest times it repeats.
    pub(crate) min: u32,
    /// What it repeats.
    pub(crate) node: Box<Node>,
}

// ========================================================================
// Node Implementations
// ========================================================================

impl Node {
    /// The fewest characters a match of it takes, a call counting as none.
    pub(crate) fn min_len(&self) -> usize {
        self.min_len_with(&[])
    }

    /// The fewest characters a match of it takes, a call taking what
    /// `groups` says its group takes, or none when it does not say.
    pub(crate) fn min_len_with(&self, groups: &[usize]) -> usize {
        let min_len = |node: &Self| node.min_len_with(groups);
        match self {
            Self::Alt(alternatives) => alternatives.iter().map(min_len).min().unwrap_or(0),
            Self::Any | Self::AnyButNewline | Self::Char(_) | Self::Class(_) => 1,
            Self::Assert(_) | Self::BackRef(_) | Self::Empty | Self::Keep | Self::Look(_) => 0,
            Self::Atomic(node) => min_len(node),
            Self::Call(group) => groups.get(*group).copied().unwrap_or(0),
            Self::Capture(capture) => min_len(&capture.node),
            Self::Concat(parts) => parts.iter().map(min_len).fold(0, usize::saturating_add),
            Self::Repeat(repeat) => min_len(&repeat.node).saturating_mul(repeat.min as usize),
        }
    }

    /// The most characters a match of it takes; `None` when unbounded. A
    /// call takes what `groups` says its group takes, unbounded when it
    /// does not say.
    pub(crate) fn max_len_with(&self, groups: &[Option<usize>]) -> Option<usize> {
        let max_len = |node: &Self| node.max_len_with(groups);
        match self {
            Self::Alt(alternatives) => alternatives
                .iter()
                .try_fold(0, |most, alternative| Some(most.max(max_len(alternative)?))),
            Self::Any | Self::AnyButNewline | Self::Char(_) | Self::Class(_) => Some(1),
            Self::Assert(_) | Self::Empty | Self::Keep | Self::Look(_) => Some(0),
            Self::BackRef(_) => None,
            Self::Atomic(node) => max_len(node),
            Self::Call(group) => groups.get(*group).copied().flatten(),
            Self::Capture(capture) => max_len(&capture.node),
            Self::Concat(parts) => parts
                .iter()
                .try_fold(0usize, |total, part| total.checked_add(max_len(part)?)),
            Self::Repeat(repeat) => {
                let inner = max_len(&repeat.node)?;
                match repeat.max {
                    Some(max) => inner.checked_mul(max as usize),
                    None if inner == 0 => Some(0),
                    None => None,
                }
            }
        }
    }
}
