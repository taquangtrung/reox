//! Compiled patterns and their searches: the crate's public face.

mod builder;
mod captures;
mod set;

use std::sync::Arc;

use crate::accel::{Accelerator, Filter, Plan, regular_input};
use crate::backtrack::{self, Bounds, Cache, Program};
use crate::error::Error;
use crate::syntax::{Flags, Parsed, parse, parse_set};

pub use self::builder::{DEFAULT_BACKTRACK_LIMIT, DEFAULT_SIZE_LIMIT, RegexBuilder, SearchOptions};
pub use self::captures::{Captures, Locations, Match};
pub use self::set::RegexSet;

// ========================================================================
// Data Structures
// ========================================================================

/// A compiled pattern, in Oniguruma's syntax.
#[derive(Clone, Debug)]
pub struct Regex {
    /// The automata it may run on once hot.
    accelerator: Arc<Accelerator>,
    /// How many times one match attempt backtracks before the search gives up.
    backtrack_limit: usize,
    /// The flags in effect from its start, to read it again with.
    flags: Flags,
    /// Each group's name, by number; the match itself first.
    names: Arc<[Option<Box<str>>]>,
    /// The pattern as written; a set's patterns, read as one.
    patterns: Arc<[Box<str>]>,
    /// The program the backtracking machine runs.
    program: Arc<Program>,
}

// ========================================================================
// Regex Implementations
// ========================================================================

impl Regex {
    /// `pattern` compiled with the default options.
    pub fn new(pattern: &str) -> Result<Self, Error> {
        RegexBuilder::new(pattern).build()
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.patterns[0]
    }

    /// How many groups it has, the whole match included.
    pub fn captures_len(&self) -> usize {
        self.names.len()
    }

    /// Each group's name, by number, the whole match first.
    pub fn capture_names(&self) -> impl Iterator<Item = Option<&str>> {
        self.names.iter().map(Option::as_deref)
    }

    /// Fresh search state for [`search`](Self::search).
    pub fn create_cache(&self) -> Cache {
        Cache::default()
    }

    /// Fresh locations for [`search`](Self::search).
    pub fn capture_locations(&self) -> Locations {
        Locations::new(self.program.slots)
    }

    /// Searches `haystack` from byte `start` for the leftmost match, filling
    /// `locations`; whether it found one. `\G` holds at `start`, and
    /// lookbehinds see the text before it. Allocates nothing once `cache`
    /// is warm.
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
    ) -> Result<bool, Error> {
        self.search_with(cache, haystack, start, SearchOptions::new(), locations)
    }

    /// Searches as [`search`](Self::search) does, with `options` saying
    /// whether the match must start at `start` and whether `\A` and `\G`
    /// may match: a TextMate tokenizer allows `\G` only where the last
    /// match ended.
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
    ) -> Result<bool, Error> {
        assert!(
            haystack.is_char_boundary(start),
            "start is not a char boundary"
        );
        locations.slots.resize(self.program.slots, None);
        let slots = &mut locations.slots;
        let plan = self.accelerator.plan();
        let remaining = haystack.len() - start;
        let mut filter = None;
        if let Some(automata) = self.accelerator.ready(|| self.reparse(), remaining) {
            match plan {
                Plan::Regular(regular) => {
                    let Some(input) = regular_input(
                        haystack,
                        start,
                        regular.leading,
                        options.anchored,
                        options.text_start,
                        options.search_start,
                    ) else {
                        return Ok(false);
                    };
                    let Some((match_start, match_end)) =
                        self.accelerator.find(&mut cache.automata, automata, &input)
                    else {
                        return Ok(false);
                    };
                    // A match ending after the final newline may lean on
                    // `^` there, which Oniguruma's does not match: the
                    // backtracking machine decides it, searching anew.
                    let after_final_newline = regular.line_start
                        && haystack.ends_with('\n')
                        && match_end == haystack.len();
                    if !after_final_newline {
                        if slots.len() == 2 {
                            slots[0] = Some(match_start);
                            slots[1] = Some(match_end);
                            return Ok(true);
                        }
                        // The groups, found where the match starts.
                        return backtrack::search(
                            &self.program,
                            cache,
                            haystack,
                            Bounds {
                                backtrack_limit: self.backtrack_limit,
                                filter: None,
                                from: match_start,
                                options: options.anchored(true),
                                start,
                            },
                            slots,
                        );
                    }
                }
                Plan::Filter => {
                    filter = Some(Filter {
                        accelerator: &self.accelerator,
                        automata,
                    });
                }
                Plan::Backtrack => {}
            }
        }
        backtrack::search(
            &self.program,
            cache,
            haystack,
            Bounds {
                backtrack_limit: self.backtrack_limit,
                filter,
                from: start,
                options,
                start,
            },
            slots,
        )
    }

    /// The pattern parsed again, as it was compiled: for its automata.
    fn reparse(&self) -> Option<Parsed> {
        match &*self.patterns {
            [pattern] => parse(pattern, self.flags).ok(),
            patterns => parse_set(patterns, self.flags)
                .ok()
                .map(|(parsed, _)| parsed),
        }
    }

    /// Whether `haystack` holds a match.
    pub fn is_match(&self, haystack: &str) -> Result<bool, Error> {
        Ok(self.find(haystack)?.is_some())
    }

    /// The leftmost match in `haystack`.
    pub fn find<'h>(&self, haystack: &'h str) -> Result<Option<Match<'h>>, Error> {
        self.find_at(haystack, 0)
    }

    /// The leftmost match in `haystack` from byte `start`, as
    /// [`search`](Self::search) finds it.
    pub fn find_at<'h>(&self, haystack: &'h str, start: usize) -> Result<Option<Match<'h>>, Error> {
        Ok(self
            .captures_at(haystack, start)?
            .and_then(|captures| captures.get(0)))
    }

    /// The leftmost match in `haystack`, with its groups.
    pub fn captures<'h>(&self, haystack: &'h str) -> Result<Option<Captures<'h>>, Error> {
        self.captures_at(haystack, 0)
    }

    /// The leftmost match in `haystack` from byte `start`, with its groups,
    /// as [`search`](Self::search) finds it.
    pub fn captures_at<'h>(
        &self,
        haystack: &'h str,
        start: usize,
    ) -> Result<Option<Captures<'h>>, Error> {
        let mut locations = self.capture_locations();
        let found = self.search(&mut self.create_cache(), haystack, start, &mut locations)?;
        Ok(found.then(|| Captures::new(haystack, locations, self.names.clone())))
    }
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_runaway_search_gives_up_at_its_limit() {
        // Places inside a call are not memoized.
        let regex = RegexBuilder::new(r"\g<x>(?<x>(?:a|a)*b){0}")
            .backtrack_limit(10_000)
            .build()
            .expect("compiles");
        let text = "a".repeat(40);
        assert_eq!(regex.is_match(&text), Err(Error::BacktrackLimit));
        assert_eq!(regex.is_match("aab"), Ok(true), "a short one finishes");
    }

    #[test]
    fn test_memoizing_bounds_a_search_with_back_references() {
        // Unmemoized, it backtracks some 2^30 times, past the default limit.
        let regex = Regex::new(r"^(a+)+\1$").expect("compiles");
        let text = format!("{}!", "a".repeat(30));
        assert_eq!(regex.is_match(&text), Ok(false));
        assert_eq!(regex.is_match("aaaa"), Ok(true));
    }

    #[test]
    fn test_memoizing_bounds_an_exponential_search() {
        let regex = RegexBuilder::new(r"(a|a)*b")
            .backtrack_limit(10_000)
            .build()
            .expect("compiles");
        let text = "a".repeat(40);
        assert_eq!(regex.is_match(&text), Ok(false));
        assert_eq!(
            regex.find(&format!("{text}b")).map(|m| m.map(|m| m.end())),
            Ok(Some(41))
        );
    }

    #[test]
    fn test_search_options_govern_anchors_and_where_a_match_starts() {
        let search = |pattern: &str, text: &str, start: usize, options: SearchOptions| {
            let regex = Regex::new(pattern).expect("compiles");
            let mut locations = regex.capture_locations();
            regex
                .search_with(
                    &mut regex.create_cache(),
                    text,
                    start,
                    options,
                    &mut locations,
                )
                .expect("finishes")
                .then(|| locations.get(0).expect("a match"))
        };
        let options = SearchOptions::new();
        assert_eq!(search(r"\Ga|b", "ab", 0, options), Some((0, 1)));
        assert_eq!(
            search(r"\Ga|b", "ab", 0, options.search_start_matches(false)),
            Some((1, 2)),
            "\\G never matches"
        );
        assert_eq!(
            search(r"\Aa", "a", 0, options.text_start_matches(false)),
            None
        );
        assert_eq!(
            search(r"b", "ab", 0, options.anchored(true)),
            None,
            "only at the start"
        );
        assert_eq!(search(r"b", "ab", 1, options.anchored(true)), Some((1, 2)));
    }

    #[test]
    fn test_a_reused_search_starts_clean() {
        let regex = Regex::new(r"(a)?b").expect("compiles");
        let mut cache = regex.create_cache();
        let mut locations = regex.capture_locations();
        assert_eq!(regex.search(&mut cache, "ab", 0, &mut locations), Ok(true));
        assert_eq!(locations.get(1), Some((0, 1)));
        assert_eq!(regex.search(&mut cache, "b", 0, &mut locations), Ok(true));
        assert_eq!(locations.get(1), None, "no group left from the last search");
    }
}
