//! reox, regex oxidized: a regex engine with lookaround and back-references,
//! in Oniguruma's syntax, the dialect TextMate grammars and Ruby write
//! patterns in.
//!
//! - [`Regex`] is a compiled pattern. Compiling is cheap: a pattern becomes
//!   a short program for a backtracking machine, with no automata built,
//!   so loading thousands of patterns costs little until they are used.
//! - [`Regex::search`] finds the leftmost match from a position, `\G`
//!   holding there and lookbehinds seeing the text before it, and with a
//!   reused [`Cache`] and [`Locations`] allocates nothing.
//! - [`RegexSet`] searches several patterns as one: the leftmost match of
//!   any, the first pattern's where several start at the same place.
//! - Backtracking is bounded: a search that backtracks more than its limit
//!   allows ends with [`Error::BacktrackLimit`] instead of stalling.
//!
//! The syntax is Oniguruma's: `^` and `$` match at lines, `(?m)` lets `.`
//! match a newline, POSIX classes and `\w` are Unicode, `(?i)` alone
//! applies to the rest of its group, alternatives included, ignoring case
//! folds `ß` to `ss`, and lookaround, atomic groups, possessive quantifiers,
//! back-references, subroutine calls, and `\K` work as Oniguruma's do.
//!
//! ```
//! use reox::Regex;
//!
//! let regex = Regex::new(r"(?<!\w)(?<name>\w+)(?=\()").unwrap();
//! let captures = regex.captures("x = call(1)").unwrap().unwrap();
//! assert_eq!(captures.name("name").unwrap().as_str(), "call");
//! ```

mod accel;
mod backtrack;
mod error;
mod regex;
mod syntax;

pub use backtrack::Cache;
pub use error::{Error, PatternError};
pub use regex::{
    Captures, DEFAULT_BACKTRACK_LIMIT, DEFAULT_SIZE_LIMIT, Locations, Match, Regex, RegexBuilder,
    RegexSet, SearchOptions,
};
