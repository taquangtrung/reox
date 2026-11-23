//! What a search found: the match, and where its groups matched.

use std::ops::Range;
use std::sync::Arc;

// ========================================================================
// Data Structures
// ========================================================================

/// Where a search's groups matched, reusable between searches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Locations {
    /// Two slots per group, start then end, the match itself first.
    pub(crate) slots: Vec<Option<usize>>,
}

/// A match: where it is in the text searched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Match<'h> {
    /// Where it ends, in bytes.
    end: usize,
    /// The text searched.
    haystack: &'h str,
    /// Where it starts, in bytes.
    start: usize,
}

/// A match with its groups.
#[derive(Clone, Debug)]
pub struct Captures<'h> {
    /// The text searched.
    haystack: &'h str,
    /// Where the groups matched.
    locations: Locations,
    /// Each group's name, by number.
    names: Arc<[Option<Box<str>>]>,
}

// ========================================================================
// Captures Implementations
// ========================================================================

impl<'h> Captures<'h> {
    /// The groups of a match in `haystack`, found at `locations`.
    pub(crate) fn new(
        haystack: &'h str,
        locations: Locations,
        names: Arc<[Option<Box<str>>]>,
    ) -> Self {
        Self {
            haystack,
            locations,
            names,
        }
    }

    /// Group `index`'s match, the whole match being group 0; `None` for a
    /// group that took no part.
    pub fn get(&self, index: usize) -> Option<Match<'h>> {
        let (start, end) = self.locations.get(index)?;
        Some(Match {
            end,
            haystack: self.haystack,
            start,
        })
    }

    /// The match of the group named `name`: of the groups sharing it, the
    /// last that took part.
    pub fn name(&self, name: &str) -> Option<Match<'h>> {
        self.names
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, group)| group.as_deref() == Some(name))
            .find_map(|(index, _)| self.get(index))
    }

    /// How many groups there are, the whole match included.
    pub fn len(&self) -> usize {
        self.locations.len()
    }

    /// Whether there are no groups; never, as the match is one.
    pub fn is_empty(&self) -> bool {
        self.locations.len() == 0
    }
}

// ========================================================================
// Locations Implementations
// ========================================================================

impl Locations {
    /// Locations for `slots` slots, two per group, none filled.
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            slots: vec![None; slots],
        }
    }

    /// Where group `index` matched, start and end in bytes; `None` for a
    /// group that took no part.
    pub fn get(&self, index: usize) -> Option<(usize, usize)> {
        Some((
            *self.slots.get(index * 2)?.as_ref()?,
            (*self.slots.get(index * 2 + 1)?)?,
        ))
    }

    /// How many groups there are, the whole match included.
    pub fn len(&self) -> usize {
        self.slots.len() / 2
    }

    /// Whether there are no groups; never, as the match is one.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

// ========================================================================
// Match Implementations
// ========================================================================

impl<'h> Match<'h> {
    /// Where it starts, in bytes.
    pub fn start(&self) -> usize {
        self.start
    }

    /// Where it ends, in bytes.
    pub fn end(&self) -> usize {
        self.end
    }

    /// Where it is, in bytes.
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// The text it matched.
    pub fn as_str(&self) -> &'h str {
        &self.haystack[self.start..self.end]
    }
}
