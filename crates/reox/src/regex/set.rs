//! Several patterns searched as one: the leftmost match of any, the first
//! pattern's where several match from the same place, as a TextMate
//! tokenizer tries the patterns of a rule.

use std::sync::Arc;

use crate::backtrack::Cache;
use crate::error::Error;
use crate::syntax::{Flags, parse_set};

use super::{Locations, Regex, RegexBuilder, SearchOptions};

// ========================================================================
// Data Structures
// ========================================================================

/// Several compiled patterns, in Oniguruma's syntax, searched as one.
#[derive(Clone, Debug)]
pub struct RegexSet {
    /// The group each pattern is held in, by pattern: its own groups
    /// follow it.
    bases: Arc<[usize]>,
    /// How many groups each pattern has, by pattern, its match not counted.
    groups: Arc<[usize]>,
    /// The patterns, as one alternation.
    regex: Regex,
}

// ========================================================================
// RegexSet Implementations
// ========================================================================

impl RegexSet {
    /// `patterns` compiled with the default options; an error when one does
    /// not compile.
    pub fn new(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, Error> {
        let patterns: Arc<[Box<str>]> = patterns
            .into_iter()
            .map(|pattern| pattern.as_ref().into())
            .collect();
        let (parsed, bases) = parse_set(&patterns, Flags::default())?;
        let ends = bases.iter().skip(1).copied().chain([parsed.groups + 1]);
        let groups = bases
            .iter()
            .zip(ends)
            .map(|(base, end)| end - base - 1)
            .collect();
        let regex = RegexBuilder::new("").compile(parsed, patterns)?;
        Ok(Self {
            bases: bases.into(),
            groups,
            regex,
        })
    }

    /// How many patterns it holds.
    pub fn len(&self) -> usize {
        self.bases.len()
    }

    /// Whether it holds no pattern, and so matches nothing.
    pub fn is_empty(&self) -> bool {
        self.bases.is_empty()
    }

    /// Fresh search state for [`search`](Self::search).
    pub fn create_cache(&self) -> Cache {
        self.regex.create_cache()
    }

    /// Fresh locations for [`search`](Self::search).
    pub fn capture_locations(&self) -> Locations {
        self.regex.capture_locations()
    }

    /// Searches `haystack` from byte `start` for the leftmost match of any
    /// pattern, the first pattern's where several start at the same place:
    /// which pattern matched, `locations` then holding its groups, numbered
    /// as in it.
    ///
    /// # Panics
    ///
    /// When `start` is past the text's end or inside a character.
    pub fn search(
        &self,
        cache: &mut Cache,
        haystack: &str,
        start: usize,
        locations: &mut Locations,
    ) -> Result<Option<usize>, Error> {
        self.search_with(cache, haystack, start, SearchOptions::new(), locations)
    }

    /// Searches as [`search`](Self::search) does, with `options` as
    /// [`Regex::search_with`] takes them.
    ///
    /// # Panics
    ///
    /// When `start` is past the text's end or inside a character.
    pub fn search_with(
        &self,
        cache: &mut Cache,
        haystack: &str,
        start: usize,
        options: SearchOptions,
        locations: &mut Locations,
    ) -> Result<Option<usize>, Error> {
        if !self
            .regex
            .search_with(cache, haystack, start, options, locations)?
        {
            return Ok(None);
        }
        // Only the pattern that matched holds its group.
        let slots = &mut locations.slots;
        let pattern = self
            .bases
            .iter()
            .position(|&base| slots[base * 2].is_some())
            .expect("the pattern that matched holds its group");
        let first = (self.bases[pattern] + 1) * 2;
        let groups = self.groups[pattern];
        slots.copy_within(first..first + groups * 2, 2);
        slots.truncate(2 + groups * 2);
        Ok(Some(pattern))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Which pattern matched, and its groups' spans.
    type Found = Option<(usize, Vec<Option<(usize, usize)>>)>;

    /// The pattern that matched in `text` from `start`, and its groups.
    fn search(set: &RegexSet, text: &str, start: usize) -> Found {
        let mut locations = set.capture_locations();
        let pattern = set
            .search(&mut set.create_cache(), text, start, &mut locations)
            .expect("finishes")?;
        Some((
            pattern,
            (0..locations.len())
                .map(|group| locations.get(group))
                .collect(),
        ))
    }

    #[test]
    fn test_the_leftmost_match_wins_then_the_first_pattern() {
        let set = RegexSet::new([r"b+", r"a(b)", r"(a)(b)"]).expect("compiles");
        assert_eq!(
            search(&set, "xabb", 0),
            Some((1, vec![Some((1, 3)), Some((2, 3))])),
            "the leftmost, of the first pattern there"
        );
        assert_eq!(search(&set, "xabb", 2), Some((0, vec![Some((2, 4))])));
        assert_eq!(search(&set, "xyz", 0), None);
        let empty = RegexSet::new([""; 0]).expect("compiles");
        assert_eq!(search(&empty, "xyz", 0), None, "no pattern matches nothing");
    }

    #[test]
    fn test_each_pattern_keeps_its_own_groups_flags_and_references() {
        let set = RegexSet::new([r"(?i)(x)\1", r"(?<n>a)\k<n>", r"(?<p>\((?:[^()]|\g<p>)*\))"])
            .expect("compiles");
        assert_eq!(
            search(&set, "xX", 0),
            Some((0, vec![Some((0, 2)), Some((0, 1))]))
        );
        assert_eq!(
            search(&set, "Aa aa", 0),
            Some((1, vec![Some((3, 5)), Some((3, 4))]))
        );
        assert_eq!(
            search(&set, "(a(b))", 0),
            Some((2, vec![Some((0, 6)), Some((0, 6))]))
        );
    }
}
