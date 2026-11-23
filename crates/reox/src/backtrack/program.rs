//! Compiled patterns: the instructions the backtracking machine runs.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use regex_syntax::hir::ClassUnicode;

use crate::accel::StartBytes;
use crate::syntax::{Assertion, BackRef};

// ========================================================================
// Constants
// ========================================================================

/// The characters a class's ASCII bitmap covers.
const ASCII_LIMIT: u32 = 128;

/// How far the hash of a class's ranges turns before taking in the next.
const INTERN_HASH_ROTATE: u32 = 5;

/// What the hash of a class's ranges is multiplied by at each range: odd,
/// with its bits well mixed.
const INTERN_HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// How many shared classes there are before ones no pattern holds are let
/// go, then again at each doubling.
const INTERN_SWEEP: usize = 1 << 10;

// ========================================================================
// Data Structures
// ========================================================================

/// A compiled pattern.
#[derive(Debug)]
pub(crate) struct Program {
    /// Where every match must start, if one place is known.
    pub(crate) anchor: Anchor,
    /// The instructions, the first run first.
    pub(crate) insts: Vec<Inst>,
    /// The groups its back-references refer to, by number: a memo point it
    /// failed from fails again when reached with them holding the same.
    pub(crate) referenced: Box<[usize]>,
    /// How many position registers its loops use.
    pub(crate) registers: usize,
    /// How many capture slots it fills: two per group, the match first.
    pub(crate) slots: usize,
    /// How many places it may memoize: its splits and character loops.
    pub(crate) memo_points: usize,
    /// The bytes every match starts with, if known.
    pub(crate) start_bytes: Option<StartBytes>,
}

/// Where every match of a pattern must start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Anchor {
    /// Anywhere.
    Floating,
    /// Where a line starts (`^...`).
    LineStart,
    /// Where the search starts (`\G...`).
    SearchStart,
    /// At the text's start (`\A...`).
    TextStart,
}

/// One instruction.
#[derive(Debug)]
pub(crate) enum Inst {
    /// Matches any character.
    Any,
    /// Matches any character but a newline.
    AnyButNewline,
    /// Tries alternatives in order, skipping those that cannot start with
    /// the character at the position.
    Alt(Box<AltInst>),
    /// Checks the position.
    Assert(Assertion),
    /// Runs the part after it to its [`Inst::End`] and keeps the first way it
    /// matches, then goes on at the instruction given.
    Atomic(usize),
    /// Matches a group's text again.
    BackRef(BackRef),
    /// Runs the called group starting at the instruction given, then goes
    /// on after this one, at its [`Inst::Return`].
    Call(usize),
    /// Matches one character.
    Char(char),
    /// Matches characters of one kind as many times as a loop allows.
    CharLoop(Box<CharLoop>),
    /// Matches one character of a class.
    Class(Arc<CharClass>),
    /// Ends the part an [`Inst::Atomic`] or [`Inst::Look`] runs.
    End,
    /// Goes on at the instruction given.
    Jump(usize),
    /// Moves where the match is reported to start to here.
    Keep,
    /// Runs the part after it as a lookaround.
    Look(Box<LookInst>),
    /// Stores the position in a register, for [`Inst::Progress`].
    Mark(usize),
    /// Ends the match.
    Match,
    /// Ends a loop's iteration: loops again if it consumed anything.
    Progress(Box<Progress>),
    /// Ends a called group: goes on after the call that ran it.
    Return,
    /// Stores the position in a capture slot.
    Save(usize),
    /// Ends a called group's match, which started where a register holds.
    SaveGroup(GroupEnd),
    /// Tries one instruction, and the other if that fails.
    Split(Split),
}

/// An alternation of several alternatives.
#[derive(Debug)]
pub(crate) struct AltInst {
    /// Its memo point, outside loops that may match nothing.
    pub(crate) id: Option<usize>,
    /// The alternatives, in order.
    pub(crate) targets: Box<[AltTarget]>,
}

/// One alternative of an [`AltInst`].
#[derive(Debug)]
pub(crate) struct AltTarget {
    /// The characters its matches start with; `None` when it may start
    /// with any, or match nothing.
    pub(crate) first: Option<Arc<CharClass>>,
    /// Its first instruction.
    pub(crate) pc: usize,
}

/// A greedy loop over single characters (`\s*`, `[^"]+`), run in one step:
/// it takes as many as it can and gives them back one at a time.
#[derive(Debug)]
pub(crate) struct CharLoop {
    /// Its memo point, outside loops that may match nothing.
    pub(crate) id: Option<usize>,
    /// The characters it takes.
    pub(crate) matcher: CharMatcher,
    /// The most it takes; `None` for no limit.
    pub(crate) max: Option<u32>,
    /// The fewest it takes.
    pub(crate) min: u32,
    /// Whether it never gives any back: written possessive, or followed
    /// by a part that never starts with what it takes.
    pub(crate) possessive: bool,
}

/// One character of a kind.
#[derive(Debug)]
pub(crate) enum CharMatcher {
    /// Any character.
    Any,
    /// Any character but a newline.
    AnyButNewline,
    /// One character.
    Char(char),
    /// One character of a class.
    Class(Arc<CharClass>),
}

/// A character class, with a bitmap for ASCII.
#[derive(Debug)]
pub(crate) struct CharClass {
    /// Which ASCII characters it holds, one bit each.
    ascii: u128,
    /// Its ranges, sorted.
    ranges: Box<[(char, char)]>,
}

/// The end of a called group's match.
#[derive(Debug)]
pub(crate) struct GroupEnd {
    /// The group's number.
    pub(crate) index: usize,
    /// The register holding where the match started, in this call.
    pub(crate) register: usize,
}

/// A lookaround instruction.
#[derive(Debug)]
pub(crate) struct LookInst {
    /// Whether it looks behind.
    pub(crate) behind: bool,
    /// For a lookbehind, the characters its part ends with, checked against
    /// the character before the position first; `None` when any.
    pub(crate) last: Option<Arc<CharClass>>,
    /// For a lookbehind whose alternatives each take a fixed number of
    /// characters, those numbers in the alternatives' order, tried in it,
    /// as Oniguruma tries them; `None` to try every length nearest first.
    pub(crate) lengths: Option<Box<[usize]>>,
    /// For a lookbehind, the most characters its part takes; `None` when
    /// unbounded, every earlier start then tried.
    pub(crate) max_len: Option<usize>,
    /// For a lookbehind, the fewest characters its part takes.
    pub(crate) min_len: usize,
    /// Whether it asserts its part does not match.
    pub(crate) negated: bool,
    /// The instruction after its part.
    pub(crate) next: usize,
}

/// The end of a loop's iteration that may have matched nothing.
#[derive(Debug)]
pub(crate) struct Progress {
    /// Where the loop goes when the iteration matched nothing.
    pub(crate) exit: usize,
    /// The register holding where the iteration started.
    pub(crate) register: usize,
    /// Where the next iteration starts.
    pub(crate) start: usize,
    /// The groups whose change makes an iteration that matched nothing
    /// count, by number.
    pub(crate) watched: Box<[usize]>,
}

/// A choice between two instructions.
#[derive(Debug)]
pub(crate) struct Split {
    /// The instruction tried first.
    pub(crate) first: usize,
    /// Its memo point, outside loops that may match nothing.
    pub(crate) id: Option<usize>,
    /// The instruction tried when the first fails.
    pub(crate) second: usize,
}

// ========================================================================
// CharMatcher Implementations
// ========================================================================

impl CharMatcher {
    /// Whether it takes `c`.
    pub(crate) fn matches(&self, c: char) -> bool {
        match self {
            Self::Any => true,
            Self::AnyButNewline => c != '\n',
            Self::Char(expected) => c == *expected,
            Self::Class(class) => class.contains(c),
        }
    }
}

// ========================================================================
// CharClass Implementations
// ========================================================================

impl CharClass {
    /// `class`, ready to match, shared with every pattern that holds the
    /// same class while any does: `\w` and its kin take kilobytes each.
    pub(crate) fn shared(class: &ClassUnicode) -> Arc<Self> {
        // Classes by a hash of their ranges, each compared to find the one:
        // looking one up builds nothing.
        type Interned = HashMap<u64, Vec<Weak<CharClass>>>;
        static INTERNED: OnceLock<Mutex<Interned>> = OnceLock::new();
        let key = class.ranges().iter().fold(0u64, |hash, range| {
            let pair =
                (u64::from(u32::from(range.start())) << 32) | u64::from(u32::from(range.end()));
            (hash.rotate_left(INTERN_HASH_ROTATE) ^ pair).wrapping_mul(INTERN_HASH_MULTIPLIER)
        });
        let is_same = |shared: &CharClass| {
            shared.ranges.len() == class.ranges().len()
                && shared
                    .ranges
                    .iter()
                    .zip(class.ranges())
                    .all(|(&(start, end), range)| start == range.start() && end == range.end())
        };
        let mut interned = INTERNED
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(shared) = interned
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(Weak::upgrade)
            .find(|shared| is_same(shared))
        {
            return shared;
        }
        let shared = Arc::new(Self::new(class));
        // Classes no pattern holds any more are let go as new ones arrive.
        if interned.len() >= INTERN_SWEEP && interned.len().is_power_of_two() {
            interned.retain(|_, classes| {
                classes.retain(|class| class.strong_count() > 0);
                !classes.is_empty()
            });
        }
        // The bucket's classes no pattern holds any more go too, or a class
        // built and dropped again and again would fill it.
        let bucket = interned.entry(key).or_default();
        bucket.retain(|class| class.strong_count() > 0);
        bucket.push(Arc::downgrade(&shared));
        shared
    }

    /// `class`, ready to match.
    pub(crate) fn new(class: &ClassUnicode) -> Self {
        let mut ascii = 0u128;
        for range in class.ranges() {
            let (start, end) = (u32::from(range.start()), u32::from(range.end()));
            for code in start..=end.min(ASCII_LIMIT - 1) {
                ascii |= 1 << code;
            }
        }
        Self {
            ascii,
            ranges: class
                .ranges()
                .iter()
                .map(|range| (range.start(), range.end()))
                .collect(),
        }
    }

    /// Whether it holds `c`.
    pub(crate) fn contains(&self, c: char) -> bool {
        let code = u32::from(c);
        if code < ASCII_LIMIT {
            return self.ascii >> code & 1 == 1;
        }
        self.ranges
            .binary_search_by(|&(start, end)| {
                if end < c {
                    Ordering::Less
                } else if start > c {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            })
            .is_ok()
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The most bytes an instruction may take: a program holds one per
    /// part of its pattern, so a variant grown past it, rather than boxed,
    /// grows every program.
    const INST_MAX_BYTES: usize = 40;

    #[test]
    fn test_instructions_stay_small() {
        assert!(size_of::<Inst>() <= INST_MAX_BYTES, "{}", size_of::<Inst>());
    }
}
