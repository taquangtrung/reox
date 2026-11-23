//! Options: for compiling a pattern, and for one search with it.

use std::sync::Arc;

use crate::accel::Accelerator;
use crate::backtrack::compile;
use crate::error::Error;
use crate::syntax::{Flags, Parsed, parse};

use super::Regex;

// ========================================================================
// Constants
// ========================================================================

/// How many times one match attempt backtracks before the search gives up,
/// by default.
pub const DEFAULT_BACKTRACK_LIMIT: usize = 1_000_000;

/// The most instructions a compiled pattern holds, by default.
pub const DEFAULT_SIZE_LIMIT: usize = 1 << 20;

// ========================================================================
// Data Structures
// ========================================================================

/// How one search treats where it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    /// Whether a match must start where the search does.
    pub(crate) anchored: bool,
    /// Whether `\G` matches where the search starts.
    pub(crate) search_start: bool,
    /// Whether `\A` matches at the text's start.
    pub(crate) text_start: bool,
}

/// Options for compiling a [`Regex`].
#[derive(Clone, Debug)]
pub struct RegexBuilder {
    /// How many times one match attempt backtracks before the search gives up.
    backtrack_limit: usize,
    /// Whether automata are built at once and run on every search.
    eager_automata: bool,
    /// The flags in effect from the pattern's start.
    flags: Flags,
    /// The pattern as written.
    pattern: String,
    /// The most instructions the compiled pattern holds.
    size_limit: usize,
}

// ========================================================================
// RegexBuilder Implementations
// ========================================================================

impl RegexBuilder {
    /// Options for compiling `pattern`, the defaults to start with.
    pub fn new(pattern: &str) -> Self {
        Self {
            backtrack_limit: DEFAULT_BACKTRACK_LIMIT,
            eager_automata: false,
            flags: Flags::default(),
            pattern: pattern.to_owned(),
            size_limit: DEFAULT_SIZE_LIMIT,
        }
    }

    /// How many times one match attempt, at one place in the text, may
    /// backtrack before the search gives up with [`Error::BacktrackLimit`].
    pub fn backtrack_limit(&mut self, limit: usize) -> &mut Self {
        self.backtrack_limit = limit;
        self
    }

    /// The most instructions the compiled pattern may hold; past it,
    /// compiling fails with [`Error::TooBig`].
    pub fn size_limit(&mut self, limit: usize) -> &mut Self {
        self.size_limit = limit;
        self
    }

    /// Whether letters match either case from the start, as `(?i)` does.
    pub fn case_insensitive(&mut self, yes: bool) -> &mut Self {
        self.flags.casei = yes;
        self
    }

    /// Whether `.` matches a newline from the start, as Oniguruma's `(?m)`
    /// does.
    pub fn dot_matches_new_line(&mut self, yes: bool) -> &mut Self {
        self.flags.dotall = yes;
        self
    }

    /// Whether automata, where the pattern allows them, are built at once
    /// and run on every search however short, rather than once it has
    /// scanned enough long text: for testing them against the backtracking
    /// machine. Only with the `testing` feature.
    #[cfg(feature = "testing")]
    pub fn eager_automata(&mut self, yes: bool) -> &mut Self {
        self.eager_automata = yes;
        self
    }

    /// Whether whitespace and `#` comments are ignored from the start, as
    /// `(?x)` does.
    pub fn ignore_whitespace(&mut self, yes: bool) -> &mut Self {
        self.flags.extended = yes;
        self
    }

    /// The pattern compiled with these options; an error when it does not
    /// parse or passes the size limit.
    pub fn build(&self) -> Result<Regex, Error> {
        let parsed = parse(&self.pattern, self.flags)?;
        self.compile(parsed, Arc::new([self.pattern.as_str().into()]))
    }

    /// `parsed`, read from `patterns`, compiled with these options.
    pub(crate) fn compile(
        &self,
        parsed: Parsed,
        patterns: Arc<[Box<str>]>,
    ) -> Result<Regex, Error> {
        let program = compile(&parsed, self.size_limit)?;
        let accelerator = Accelerator::new(&parsed.node, self.eager_automata);
        let mut names = vec![None; parsed.groups + 1];
        for (name, index) in parsed.names {
            names[index] = Some(name.into_boxed_str());
        }
        Ok(Regex {
            accelerator: Arc::new(accelerator),
            backtrack_limit: self.backtrack_limit,
            flags: self.flags,
            names: names.into(),
            patterns,
            program: Arc::new(program),
        })
    }
}

// ========================================================================
// SearchOptions Implementations
// ========================================================================

impl SearchOptions {
    /// A search from anywhere on, with `\A` and `\G` matching.
    pub fn new() -> Self {
        Self {
            anchored: false,
            search_start: true,
            text_start: true,
        }
    }

    /// Whether a match must start where the search does.
    pub fn anchored(mut self, yes: bool) -> Self {
        self.anchored = yes;
        self
    }

    /// Whether `\G` matches where the search starts.
    pub fn search_start_matches(mut self, yes: bool) -> Self {
        self.search_start = yes;
        self
    }

    /// Whether `\A` matches at the text's start.
    pub fn text_start_matches(mut self, yes: bool) -> Self {
        self.text_start = yes;
        self
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self::new()
    }
}
