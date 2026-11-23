//! reox against Oniguruma on random patterns: a pattern drawn from a small
//! grammar of the syntax, and a text from a small alphabet, searched from
//! every position; any difference in a match or a group's span is a crash.

#![no_main]

use arbitrary::{Result, Unstructured};
use reox::{Error, RegexBuilder};
use libfuzzer_sys::fuzz_target;
use onig::{MatchParam, RegexOptions, Region, SearchOptions, Syntax};

// ========================================================================
// Constants
// ========================================================================

/// The deepest groups nest.
const MAX_DEPTH: usize = 4;

/// The most items a sequence or alternation holds.
const MAX_ITEMS: usize = 4;

/// The longest text searched, in characters.
const MAX_TEXT: usize = 24;

/// The characters texts are made of: few, so patterns often match.
const TEXT_CHARS: &[char] = &['a', 'b', 'A', ' ', '\n', '-', 'ß', 's', 'S', '(', ')'];

/// Literals patterns may hold.
const LITERALS: &[&str] = &["a", "b", "A", " ", "s", "ß", "\\n", "\\(", "\\)", "-"];

/// Classes and other single-character items.
const ATOMS: &[&str] = &[
    ".",
    "\\w",
    "\\W",
    "\\s",
    "\\d",
    "[ab]",
    "[^a]",
    "[a-z]",
    "[[:alpha:]]",
    "\\h",
    "\\R",
    "\\X",
];

/// Zero-width items.
const ASSERTIONS: &[&str] = &["^", "$", "\\b", "\\B", "\\A", "\\z", "\\Z", "\\G", "\\K"];

/// Quantifiers, applied after an item.
const QUANTIFIERS: &[&str] = &[
    "*", "+", "?", "*?", "+?", "??", "*+", "++", "{2}", "{1,2}", "{,2}", "{2,}", "{0}",
];

/// Group openings; each is closed by `)`.
const GROUPS: &[&str] = &[
    "(", "(?:", "(?>", "(?=", "(?!", "(?<=", "(?<!", "(?<n>", "(?i:", "(?m:", "(?x:",
];

/// The most a backtracking search may backtrack before giving up.
const BACKTRACK_LIMIT: usize = 100_000;

// ========================================================================
// Data Structures
// ========================================================================

/// Where in a pattern an item goes.
#[derive(Clone, Copy, Default)]
struct Context {
    /// Whether inside a lookbehind, where calls are not generated: Oniguruma
    /// answers them inconsistently.
    behind: bool,
    /// How deep groups nest around it.
    depth: usize,
}

// ========================================================================
// Functions
// ========================================================================

fuzz_target!(|data: &[u8]| {
    let mut input = Unstructured::new(data);
    let (Ok(pattern), Ok(text)) = (pattern(&mut input), text(&mut input)) else {
        return;
    };
    compare(&pattern, &text);
});

/// Asserts reox and Oniguruma agree on `pattern` in `text`, from every
/// position, where both compile it and neither gives up.
fn compare(pattern: &str, text: &str) {
    let Ok(expected) = onig::Regex::with_options(
        pattern,
        RegexOptions::REGEX_OPTION_CAPTURE_GROUP,
        Syntax::default(),
    ) else {
        return;
    };
    let actual = match RegexBuilder::new(pattern)
        .backtrack_limit(BACKTRACK_LIMIT)
        .build()
    {
        Ok(actual) => actual,
        Err(Error::Unsupported(_) | Error::TooBig) => return,
        Err(error) => panic!("{pattern:?}: Oniguruma compiles it, reox says {error}"),
    };
    let mut cache = actual.create_cache();
    let mut locations = actual.capture_locations();
    for start in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
        let mut region = Region::new();
        let want = match expected.search_with_param(
            text,
            start,
            text.len(),
            SearchOptions::SEARCH_OPTION_NONE,
            Some(&mut region),
            MatchParam::default(),
        ) {
            Ok(found) => found.map(|_| (0..region.len()).map(|group| region.pos(group)).collect()),
            Err(_) => continue,
        };
        // Oniguruma sometimes reports a group ending before it starts, in a
        // repetition that matched nothing: its own slip, not followed.
        let inverted = |groups: &Vec<Option<(usize, usize)>>| {
            groups.iter().flatten().any(|(start, end)| start > end)
        };
        if want.as_ref().is_some_and(inverted) {
            continue;
        }
        let got: Option<Vec<Option<(usize, usize)>>> =
            match actual.search(&mut cache, text, start, &mut locations) {
                Ok(found) => found.then(|| {
                    (0..locations.len())
                        .map(|group| locations.get(group))
                        .collect()
                }),
                Err(_) => continue,
            };
        assert_eq!(want, got, "{pattern:?} on {text:?} from {start}");
    }
}

/// A random pattern.
fn pattern(input: &mut Unstructured<'_>) -> Result<String> {
    let mut out = String::new();
    alternation(input, Context::default(), &mut out)?;
    Ok(out)
}

/// Appends alternatives, in `context`.
fn alternation(input: &mut Unstructured<'_>, context: Context, out: &mut String) -> Result<()> {
    let count = input.int_in_range(1..=2)?;
    for i in 0..count {
        if i > 0 {
            out.push('|');
        }
        sequence(input, context, out)?;
    }
    Ok(())
}

/// Appends items one after another, in `context`.
fn sequence(input: &mut Unstructured<'_>, context: Context, out: &mut String) -> Result<()> {
    let count = input.int_in_range(0..=MAX_ITEMS)?;
    for _ in 0..count {
        item(input, context, out)?;
    }
    Ok(())
}

/// Appends one item, quantified or not, in `context`.
fn item(input: &mut Unstructured<'_>, context: Context, out: &mut String) -> Result<()> {
    let depth = context.depth;
    let choices = match depth < MAX_DEPTH {
        true => 7,
        false => 4,
    };
    let quantifiable = match input.int_in_range(0..=choices - 1)? {
        0 => {
            out.push_str(input.choose(LITERALS)?);
            true
        }
        1 => {
            // Not `\\R` in a lookbehind, which Oniguruma never matches there.
            let atom = input.choose(ATOMS)?;
            match context.behind && *atom == "\\R" {
                true => out.push_str("\\n"),
                false => out.push_str(atom),
            }
            true
        }
        2 => {
            // Not word boundaries in a lookbehind, which Oniguruma answers
            // differently at the text's start for patterns alike.
            let assertion = input.choose(ASSERTIONS)?;
            match context.behind && (*assertion == "\\b" || *assertion == "\\B") {
                true => out.push('^'),
                false => out.push_str(assertion),
            }
            false
        }
        3 => {
            // A back-reference or call: to a group that may not exist, which
            // both engines then reject. Back-references only outside groups,
            // so never inside the group they refer to, where the engines'
            // treatment of an open group differs in ways no pattern relies on.
            let group = input.int_in_range(1..=2)?;
            let kinds: &[&str] = match (depth, context.behind) {
                (0, _) => &["\\", "\\g<", "\\k<"],
                (_, false) => &["\\g<"],
                (_, true) => &["a"],
            };
            let kind = input.choose(kinds)?;
            out.push_str(kind);
            out.push_str(&group.to_string());
            if *kind != "\\" {
                out.push('>');
            }
            true
        }
        4 => {
            out.push_str(match context.behind {
                true => "b",
                false => "\\g<n>",
            });
            true
        }
        _ => {
            // Not captures in a lookbehind, which Oniguruma sets by its own
            // order of alternatives there.
            let group = match *input.choose(GROUPS)? {
                "(" | "(?<n>" if context.behind => "(?:",
                group => group,
            };
            out.push_str(group);
            let inner = Context {
                behind: context.behind || group.starts_with("(?<=") || group.starts_with("(?<!"),
                depth: depth + 1,
            };
            alternation(input, inner, out)?;
            out.push(')');
            true
        }
    };
    if quantifiable && input.ratio(1, 3)? {
        out.push_str(input.choose(QUANTIFIERS)?);
    }
    Ok(())
}

/// A random text.
fn text(input: &mut Unstructured<'_>) -> Result<String> {
    let len = input.int_in_range(0..=MAX_TEXT)?;
    (0..len)
        .map(|_| input.choose(TEXT_CHARS).copied())
        .collect()
}
