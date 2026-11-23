//! The engines measured, compiled and searched alike: fancy-regex reads
//! Perl's dialect, not Oniguruma's, so it gets each pattern with `(?m)`
//! first, for `^` and `$` at lines; close enough to measure, though a few
//! patterns mean something else to it.

use onig::{MatchParam, RegexOptions, Region, SearchOptions, Syntax};

// ========================================================================
// Constants
// ========================================================================

/// How many times a search backtracks before giving up, in every engine
/// that takes a limit.
const BACKTRACK_LIMIT: usize = 1_000_000;

// ========================================================================
// Data Structures
// ========================================================================

/// An engine measured.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Engine {
    /// fancy-regex.
    Fancy,
    /// Oniguruma, through the `onig` crate.
    Onig,
    /// reox.
    Reox,
}

/// Finds matches with one engine, reusing its search state.
pub(crate) trait Finder {
    /// The leftmost match in `text` from byte `start`, as a span; `Err` when
    /// the search gave up.
    fn find(&mut self, text: &str, start: usize) -> Result<Option<(usize, usize)>, GaveUp>;
}

/// A search that gave up at its backtrack limit.
#[derive(Debug)]
pub(crate) struct GaveUp;

/// reox, with its reusable search state.
struct ReoxFinder {
    /// Search state reused between searches.
    cache: reox::Cache,
    /// Where its groups matched.
    locations: reox::Locations,
    /// The compiled pattern.
    regex: reox::Regex,
}

/// fancy-regex.
struct FancyFinder {
    /// The compiled pattern.
    regex: fancy_regex::Regex,
}

/// Oniguruma, with its reusable region.
struct OnigFinder {
    /// The compiled pattern.
    regex: onig::Regex,
    /// Where its groups matched.
    region: Region,
}

// ========================================================================
// Engine Implementations
// ========================================================================

impl Engine {
    /// Every engine, in the order they are reported.
    pub(crate) const ALL: [Self; 3] = [Self::Reox, Self::Fancy, Self::Onig];

    /// The engine named `name`.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|engine| engine.name() == name)
    }

    /// Its name in reports and on the command line.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fancy => "fancy-regex",
            Self::Onig => "oniguruma",
            Self::Reox => "reox",
        }
    }

    /// Whether it compiles `pattern`; the compiled pattern is dropped.
    pub(crate) fn compiles(self, pattern: &str) -> bool {
        match self {
            Self::Fancy => fancy(pattern).is_some(),
            Self::Onig => oniguruma(pattern).is_some(),
            Self::Reox => reox(pattern).is_some(),
        }
    }

    /// A finder for `pattern`, if this engine compiles it.
    pub(crate) fn finder(self, pattern: &str) -> Option<Box<dyn Finder>> {
        Some(match self {
            Self::Fancy => Box::new(FancyFinder {
                regex: fancy(pattern)?,
            }),
            Self::Onig => Box::new(OnigFinder {
                regex: oniguruma(pattern)?,
                region: Region::new(),
            }),
            Self::Reox => {
                let regex = reox(pattern)?;
                Box::new(ReoxFinder {
                    cache: regex.create_cache(),
                    locations: regex.capture_locations(),
                    regex,
                })
            }
        })
    }
}

// ========================================================================
// Finder Implementations
// ========================================================================

impl Finder for ReoxFinder {
    fn find(&mut self, text: &str, start: usize) -> Result<Option<(usize, usize)>, GaveUp> {
        match self
            .regex
            .search(&mut self.cache, text, start, &mut self.locations)
        {
            Ok(true) => Ok(self.locations.get(0)),
            Ok(false) => Ok(None),
            Err(_) => Err(GaveUp),
        }
    }
}

impl Finder for FancyFinder {
    fn find(&mut self, text: &str, start: usize) -> Result<Option<(usize, usize)>, GaveUp> {
        match self.regex.find_from_pos(text, start) {
            Ok(found) => Ok(found.map(|m| (m.start(), m.end()))),
            Err(_) => Err(GaveUp),
        }
    }
}

impl Finder for OnigFinder {
    fn find(&mut self, text: &str, start: usize) -> Result<Option<(usize, usize)>, GaveUp> {
        let found = self.regex.search_with_param(
            text,
            start,
            text.len(),
            SearchOptions::SEARCH_OPTION_NONE,
            Some(&mut self.region),
            MatchParam::default(),
        );
        match found {
            Ok(Some(_)) => Ok(self.region.pos(0)),
            Ok(None) => Ok(None),
            Err(_) => Err(GaveUp),
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// `pattern` compiled by reox.
pub(crate) fn reox(pattern: &str) -> Option<reox::Regex> {
    reox::RegexBuilder::new(pattern)
        .backtrack_limit(BACKTRACK_LIMIT)
        .build()
        .ok()
}

/// `pattern` compiled by fancy-regex, its anchors made line anchors.
pub(crate) fn fancy(pattern: &str) -> Option<fancy_regex::Regex> {
    fancy_regex::RegexBuilder::new(&format!("(?m){pattern}"))
        .backtrack_limit(BACKTRACK_LIMIT)
        .build()
        .ok()
}

/// `pattern` compiled by Oniguruma, as VS Code compiles it.
pub(crate) fn oniguruma(pattern: &str) -> Option<onig::Regex> {
    onig::Regex::with_options(
        pattern,
        RegexOptions::REGEX_OPTION_CAPTURE_GROUP,
        Syntax::default(),
    )
    .ok()
}
