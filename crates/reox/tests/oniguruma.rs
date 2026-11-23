//! reox against Oniguruma itself: every pattern searched in every text from
//! every position, each group's span compared, and every pattern that one
//! rejects the other rejects too.

use onig::{RegexOptions, Region, SearchOptions, Syntax};
use reox::{Error, Regex, RegexBuilder, RegexSet};

/// Patterns covering the syntax, as TextMate grammars use it.
const PATTERNS: &[&str] = &[
    // Literals, classes, and escapes.
    r"abc",
    r"a.c",
    r"(?m)a.c",
    r"[a-c]+",
    r"[^a-c\s]+",
    r"[\w-]+",
    r"[[:alpha:]]+",
    r"[[:^digit:]x]+",
    r"[a-z&&[^aeiou]]+",
    r"[]a]+",
    r"[a-]+",
    r"\d+\.\d*",
    r"\h+",
    r"\H+",
    r"\p{Lu}\p{Ll}*",
    r"\P{Alpha}+",
    r"\p{^Digit}+",
    r"\x41\x{42}C",
    r"[\x{41 42}]+",
    r"\t|\n",
    r"\<\>",
    r"\Verb",
    r"\012",
    // Anchors.
    r"^\w+",
    r"\w+$",
    r"^$",
    r"\Aab",
    r"ab\z",
    r"ab\Z",
    r"\Ga",
    r"(^|\G)\s*x",
    r"\bfn\b",
    r"\Bn\B",
    // Groups and alternation.
    r"(a)(b)?(c)",
    r"(?:ab|a)(c)",
    r"(a|ab)(c|bcd)(d*)",
    r"(?<word>\w+)\s(?<rest>.*)",
    r"a(?i)b|c",
    r"(?i:AB)c",
    r"(?x) a \s b  # a comment",
    r"(?#a comment)ab",
    // Quantifiers.
    r"a*?b",
    r"a+?",
    r"a??b",
    r"a{2}",
    r"a{2,}",
    r"a{,2}b",
    r"a{1,2}?",
    r"a{2}+",
    r"a{2}?",
    r"a{3,2}",
    r"a{3,2}a",
    r"a*+a",
    r"a++b",
    r"(a|b)*c",
    r"(a*)*b",
    r"(a?)+?b",
    r"(|a)+b",
    // Atomic groups and lookaround.
    r"(?>a+)b",
    r"(?>a|ab)c",
    r"a(?=b)",
    r"a(?!b)",
    r"(?<=a)b",
    r"(?<!a)b",
    r"(?<=ab|c)d",
    r"(?<=a+)b",
    r"(?<!a+)b",
    r"(?<=x.*)b",
    r"(?<=(a|bc)+)d",
    r"(?<![\w-])x(?![\w-])",
    r"(?=(\w+))\w",
    r"(?!(a))\w",
    // Back-references and `\K`.
    r"(a+)b\1",
    r"(?<q>['\x22]).*?\k<q>",
    r"(?i)(a)\1",
    r"(a)|\1b",
    r"ab\Kc",
    r"(?<=a)b\Kc",
    // Subroutine calls.
    r"(?<a>x|y)\g<a>",
    r"(?<p>\((?:[^()]|\g<p>)*\))",
    r"(?<a>a\g<a>?b)",
    r"(?<a>a\g<a>?)",
    r"(?<a>[ab]\g<a>?)",
    r"\g<a>(?<a>[a-c])",
    r"(a)\g<1>",
    r"(a)\g<-1>",
    r"\g<+1>(b)",
    r"(?i:(?<a>a))\g<a>",
    r"(?<a>a(?i)b)\g<a>",
    r"(?<a>(?<b>x)|\(\g<a>\))\k<b>",
    r"(?<a>.)\g<a>\k<a>",
    r"a|b\g<0>",
    r"(?<a>\g<b>\g<a>|c)(?<b>[ab])",
    r"(?<a>(?:a|\((?:\g<a>)\))+)",
    r"(?<=\g<a>)(?<a>b)",
    r"\g",
    r"(?<a>x(\.\g<a>)?)",
    r"(?<g>\((\w|\g<g>)+\))",
    // Case-insensitive.
    r"(?i)[a-c]+",
    r"(?i)[^a]+",
    r"(?i)k",
    // Linebreaks and dots.
    r"\R",
    r"\X",
    r"\X\X",
    r"a\X+?b",
    r"a\Nb",
    r"a\Ob",
    // Found by fuzzing: a repeated group holding an anchor, recursion only
    // in a group never run, a lookbehind's part reading past where it looks
    // from, and a repeated character folding to several.
    r"(?x:^)*|",
    r"(?<n>\g<n>|\B){0}|\B.*a*",
    r"(?<!.*+)",
    r"(?:[[:alpha:]]{2}(?i:(?m:ß{2})*){2}|){2}",
    // Invalid patterns.
    r"*a",
    r"(a",
    r"a)",
    r"[b-a]",
    r"\8",
    r"\k<n>(?<n>a)",
    r"(?<a>\g<a>)",
    r"(?<a>a|\g<a>b)",
    r"(?<a>x\g<a>)",
    r"(?<a>(?:\g<a>)?x)",
    r"(?<a>(?=\g<a>)x)",
    r"(?<a>.)(?<a>.)\g<a>",
    r"\g<b>(?<a>x)",
    r"(a)\g<2>",
    r"\g<>",
    r"\g<0>",
];

/// Texts the patterns are searched in.
const TEXTS: &[&str] = &[
    "",
    "abc",
    "aabcc ab",
    "xyz a-b c_d",
    "fn main() {}",
    "Hello World",
    "abcd abcbcd",
    "aaab aab ab b",
    "a\nb\n",
    "ab\n",
    "\"quoted\" and 'single'",
    "x = 12.5 + 0x1F",
    "élan Straße ǅ K",
    "aaab x  b abcd",
    "\t\r\n\u{85}\u{2028}",
    "a<b>c",
    "AB ab aB Ab",
    "f(a(b)c)d ((x))x (a",
    "aaaaaaaaaaaaaaaaaaaaaaaaab",
    "x.x.x (a(b)) bb",
    "aaA)ß\nASSSSS",
    "e\u{301}a \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{1F1EF}\u{1F1F5}\u{1F1FA} \u{1100}\u{1161}\u{11A8}\u{AC00}\r\nx\u{600}y\u{903}",
];

/// The groups of the leftmost match from `start`, as Oniguruma finds them.
fn oniguruma(regex: &onig::Regex, text: &str, start: usize) -> Option<Vec<Option<(usize, usize)>>> {
    let mut region = Region::new();
    regex.search_with_options(
        text,
        start,
        text.len(),
        SearchOptions::SEARCH_OPTION_NONE,
        Some(&mut region),
    )?;
    Some((0..region.len()).map(|group| region.pos(group)).collect())
}

/// The groups of the leftmost match from `start`, as reox finds them.
fn reox(regex: &Regex, text: &str, start: usize) -> Option<Vec<Option<(usize, usize)>>> {
    let mut locations = regex.capture_locations();
    let found = regex
        .search(&mut regex.create_cache(), text, start, &mut locations)
        .expect("within the backtrack limit");
    found.then(|| {
        (0..locations.len())
            .map(|group| locations.get(group))
            .collect()
    })
}

#[test]
fn test_every_match_is_oniguruma_s() {
    let mut failures = Vec::new();
    for &pattern in PATTERNS {
        let expected = onig::Regex::with_options(
            pattern,
            RegexOptions::REGEX_OPTION_CAPTURE_GROUP,
            Syntax::default(),
        );
        let actual = Regex::new(pattern);
        let (expected, actual) = match (expected, actual) {
            (Ok(expected), Ok(actual)) => (expected, actual),
            (Err(_), Err(Error::Syntax(_))) => continue,
            (_, Err(Error::Unsupported(_))) => continue,
            (expected, actual) => {
                failures.push(format!(
                    "{pattern}: Oniguruma {:?}, reox {:?}",
                    expected.err().map(|error| error.to_string()),
                    actual.err()
                ));
                continue;
            }
        };
        // The automata too, run on every text, where the pattern allows.
        let eager = RegexBuilder::new(pattern)
            .eager_automata(true)
            .build()
            .expect("compiles as it did");
        for &text in TEXTS {
            for start in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
                let want = oniguruma(&expected, text, start);
                let got = reox(&actual, text, start);
                if want != got {
                    failures.push(format!(
                        "{pattern} on {text:?} from {start}: {want:?}, got {got:?}"
                    ));
                }
                let got = reox(&eager, text, start);
                if want != got {
                    failures.push(format!(
                        "{pattern} on {text:?} from {start}, on automata: {want:?}, got {got:?}"
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// How many patterns each set the set test searches holds.
const SET_SIZE: usize = 4;

#[test]
fn test_sets_match_as_oniguruma_s_patterns_alone() {
    let mut failures = Vec::new();
    let compiled: Vec<(&str, onig::Regex)> = PATTERNS
        .iter()
        .filter(|pattern| Regex::new(pattern).is_ok())
        .filter_map(|&pattern| {
            onig::Regex::with_options(
                pattern,
                RegexOptions::REGEX_OPTION_CAPTURE_GROUP,
                Syntax::default(),
            )
            .ok()
            .map(|regex| (pattern, regex))
        })
        .collect();
    for chunk in compiled.chunks(SET_SIZE) {
        let patterns: Vec<&str> = chunk.iter().map(|(pattern, _)| *pattern).collect();
        let set = RegexSet::new(&patterns).expect("each pattern compiles");
        let mut cache = set.create_cache();
        let mut locations = set.capture_locations();
        for &text in TEXTS {
            for start in (0..=text.len()).filter(|&at| text.is_char_boundary(at)) {
                // The leftmost match of any pattern alone, the first's on a tie.
                let want = chunk
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (_, regex))| {
                        oniguruma(regex, text, start).map(|groups| (index, groups))
                    })
                    .min_by_key(|(index, groups)| (groups[0].map(|(at, _)| at), *index));
                let got = set
                    .search(&mut cache, text, start, &mut locations)
                    .expect("within the backtrack limit")
                    .map(|index| {
                        let groups = (0..locations.len()).map(|group| locations.get(group));
                        (index, groups.collect())
                    });
                if want != got {
                    failures.push(format!(
                        "{patterns:?} on {text:?} from {start}: {want:?}, got {got:?}"
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Multi-character case folds, which Oniguruma applies both ways: `ß` and
/// `ss` match each other ignoring case.
#[test]
fn test_multi_character_case_folds_match_both_ways() {
    for (pattern, text) in [
        ("(?i)straße", "STRASSE"),
        ("(?i)ss", "ß"),
        ("(?i)[ß]", "ss"),
    ] {
        let regex = Regex::new(pattern).expect("compiles");
        assert!(
            regex.is_match(text).expect("finishes"),
            "{pattern} on {text:?}"
        );
    }
}

/// How long a text the long-text test searches: past what makes a pattern
/// run on automata.
const LONG_TEXT: usize = 80 << 10;

#[test]
fn test_long_texts_match_as_oniguruma_s() {
    let sample = TEXTS.concat();
    let text = sample.repeat(LONG_TEXT.div_ceil(sample.len()));
    let mut failures = Vec::new();
    for &pattern in PATTERNS {
        let (Ok(expected), Ok(actual)) = (
            onig::Regex::with_options(
                pattern,
                RegexOptions::REGEX_OPTION_CAPTURE_GROUP,
                Syntax::default(),
            ),
            Regex::new(pattern),
        ) else {
            continue;
        };
        let starts = [0, 1, text.len() / 2];
        // Twice: once building the automata, once running on them.
        for _ in 0..2 {
            for start in starts.into_iter().filter(|&at| text.is_char_boundary(at)) {
                let want = oniguruma(&expected, &text, start);
                let got = reox(&actual, &text, start);
                if want != got {
                    failures.push(format!("{pattern} from {start}: {want:?}, got {got:?}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
