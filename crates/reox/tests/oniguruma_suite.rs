//! Oniguruma's own test suite, `test_utf8.c` from Oniguruma 6.9.10, run on
//! reox: each `x2`, `x3`, `n`, and `e` case read from the C file as it is
//! vendored, so updating the suite means copying a newer file in.
//!
//! The suite compiles without `ONIG_OPTION_CAPTURE_GROUP`, so where a
//! pattern has named groups its other groups capture nothing; reox
//! numbers every group, as VS Code's Oniguruma does, so group checks are
//! skipped for such patterns.

use reox::{Error, Regex};

// ========================================================================
// Constants
// ========================================================================

/// The suite, as Oniguruma ships it.
const SUITE: &str = include_str!("fixtures/oniguruma/test_utf8.c");

/// The lines of cases reox is known to fail, with why: each a gap to
/// close, kept here so the test catches every new failure.
const KNOWN_FAILURES: &[(usize, &str)] = &[];

// ========================================================================
// Data Structures
// ========================================================================

/// One case of the suite.
#[derive(Debug)]
struct Case {
    /// What the case expects.
    expect: Expect,
    /// Its line in the C file.
    line: usize,
    /// The pattern.
    pattern: String,
    /// The text searched.
    text: String,
}

/// What a case expects.
#[derive(Debug)]
enum Expect {
    /// The pattern does not compile.
    Error,
    /// A group matches at a span: the whole match for group 0.
    Match(Span),
    /// Nothing matches.
    NoMatch,
}

/// Where a group is expected to match.
#[derive(Debug)]
struct Span {
    /// Where it ends, in bytes.
    end: usize,
    /// The group.
    group: usize,
    /// Where it starts, in bytes.
    start: usize,
}

/// How a case went.
enum Outcome {
    /// As expected.
    Pass,
    /// Not as expected, and why.
    Fail(String),
    /// Not run: reox does not support the pattern, or the case cannot be
    /// read as UTF-8 text.
    Skip,
}

// ========================================================================
// Functions
// ========================================================================

#[test]
fn test_oniguruma_s_own_suite() {
    let cases = cases();
    assert!(cases.len() > 1_400, "read {} cases", cases.len());
    let (mut passed, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    for case in &cases {
        match run(case) {
            Outcome::Pass => passed += 1,
            Outcome::Skip => skipped += 1,
            Outcome::Fail(why) => failures.push((case.line, why)),
        }
    }
    let unexpected: Vec<String> = failures
        .iter()
        .filter(|(line, _)| !KNOWN_FAILURES.iter().any(|(known, _)| known == line))
        .map(|(line, why)| format!("line {line}: {why}"))
        .collect();
    let fixed: Vec<usize> = KNOWN_FAILURES
        .iter()
        .map(|(line, _)| *line)
        .filter(|line| !failures.iter().any(|(failed, _)| failed == line))
        .collect();
    println!(
        "{} cases: {passed} passed, {} failed ({} known), {skipped} skipped",
        cases.len(),
        failures.len(),
        KNOWN_FAILURES.len()
    );
    assert!(
        unexpected.is_empty(),
        "{} unexpected failures:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
    assert!(fixed.is_empty(), "known failures now pass: {fixed:?}");
}

/// How `case` goes on reox.
fn run(case: &Case) -> Outcome {
    let regex = match (Regex::new(&case.pattern), &case.expect) {
        (Err(Error::Unsupported(_)), _) => return Outcome::Skip,
        (Err(_), Expect::Error) => return Outcome::Pass,
        (Err(error), _) => {
            return Outcome::Fail(format!("{:?} does not compile: {error}", case.pattern));
        }
        (Ok(_), Expect::Error) => return Outcome::Fail(format!("{:?} compiles", case.pattern)),
        (Ok(regex), _) => regex,
    };
    let found = match regex.captures(&case.text) {
        Ok(found) => found,
        Err(error) => {
            return Outcome::Fail(format!("{:?} on {:?}: {error}", case.pattern, case.text));
        }
    };
    match (&case.expect, found) {
        (Expect::NoMatch, None) => Outcome::Pass,
        (Expect::NoMatch, Some(captures)) => Outcome::Fail(format!(
            "{:?} on {:?} matched at {:?}",
            case.pattern,
            case.text,
            captures.get(0).map(|m| m.range())
        )),
        (Expect::Match(_), None) => Outcome::Fail(format!(
            "{:?} on {:?} found nothing",
            case.pattern, case.text
        )),
        (Expect::Match(span), Some(_)) if span.group > 0 && has_named_group(&case.pattern) => {
            Outcome::Skip
        }
        (Expect::Match(span), Some(captures)) => {
            let got = captures.get(span.group).map(|m| m.range());
            match got == Some(span.start..span.end) {
                true => Outcome::Pass,
                false => Outcome::Fail(format!(
                    "{:?} on {:?}: group {} at {:?}, expected {:?}",
                    case.pattern,
                    case.text,
                    span.group,
                    got,
                    span.start..span.end
                )),
            }
        }
        (Expect::Error, _) => unreachable!("handled above"),
    }
}

/// Whether `pattern` has a named group, which makes Oniguruma's other
/// groups capture nothing in the suite.
fn has_named_group(pattern: &str) -> bool {
    pattern.contains("(?<") && !pattern.contains("(?<=") && !pattern.contains("(?<!")
        || pattern.contains("(?'")
}

/// Every case of the suite that reads as UTF-8.
fn cases() -> Vec<Case> {
    SUITE
        .lines()
        .enumerate()
        .filter_map(|(index, line)| case(line.trim_start(), index + 1))
        .collect()
}

/// The case on `line`, if it holds one.
fn case(line: &str, number: usize) -> Option<Case> {
    let (kind, rest) = line.split_once('(')?;
    if !matches!(kind, "x2" | "x3" | "n" | "e") {
        return None;
    }
    let mut reader = Reader {
        chars: rest.chars().collect(),
        pos: 0,
    };
    let pattern = String::from_utf8(reader.string()?).ok()?;
    reader.comma()?;
    let text = String::from_utf8(reader.string()?).ok()?;
    let expect = match kind {
        "x2" | "x3" => {
            reader.comma()?;
            let start = reader.number()?;
            reader.comma()?;
            let end = reader.number()?;
            let group = match kind {
                "x3" => {
                    reader.comma()?;
                    reader.number()?
                }
                _ => 0,
            };
            Expect::Match(Span { end, group, start })
        }
        "n" => Expect::NoMatch,
        _ => Expect::Error,
    };
    Some(Case {
        expect,
        line: number,
        pattern,
        text,
    })
}

/// Reads the arguments of a C call.
struct Reader {
    /// The text after the call's `(`.
    chars: Vec<char>,
    /// Where the reader is.
    pos: usize,
}

impl Reader {
    /// A C string, adjacent literals joined, as bytes.
    fn string(&mut self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        self.skip_space();
        if self.chars.get(self.pos) != Some(&'"') {
            return None;
        }
        while self.chars.get(self.pos) == Some(&'"') {
            self.pos += 1;
            loop {
                let c = *self.chars.get(self.pos)?;
                self.pos += 1;
                match c {
                    '"' => break,
                    '\\' => self.escape(&mut bytes)?,
                    c => {
                        let mut buffer = [0; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
            self.skip_space();
        }
        Some(bytes)
    }

    /// The C escape after a backslash, added to `bytes`.
    fn escape(&mut self, bytes: &mut Vec<u8>) -> Option<()> {
        let c = *self.chars.get(self.pos)?;
        self.pos += 1;
        let byte = match c {
            'n' => b'\n',
            't' => b'\t',
            'r' => b'\r',
            'f' => 0x0C,
            'v' => 0x0B,
            'a' => 0x07,
            'b' => 0x08,
            'e' => 0x1B,
            // A universal character name, `\u2126`.
            'u' => {
                let digits = self.take_while(|c| c.is_ascii_hexdigit(), 4);
                let c = char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?;
                let mut buffer = [0; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                return Some(());
            }
            'x' => {
                let digits = self.take_while(|c| c.is_ascii_hexdigit(), usize::MAX);
                u8::try_from(u32::from_str_radix(&digits, 16).ok()?).ok()?
            }
            '0'..='7' => {
                self.pos -= 1;
                let digits = self.take_while(|c| matches!(c, '0'..='7'), 3);
                u8::try_from(u32::from_str_radix(&digits, 8).ok()?).ok()?
            }
            c if c.is_ascii() => c as u8,
            _ => return None,
        };
        bytes.push(byte);
        Some(())
    }

    /// A decimal number.
    fn number(&mut self) -> Option<usize> {
        self.skip_space();
        self.take_while(|c| c.is_ascii_digit(), usize::MAX)
            .parse()
            .ok()
    }

    /// A comma between arguments.
    fn comma(&mut self) -> Option<()> {
        self.skip_space();
        (self.chars.get(self.pos) == Some(&',')).then(|| self.pos += 1)
    }

    /// Skips whitespace.
    fn skip_space(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// At most `max` characters `accepts` takes, read past.
    fn take_while(&mut self, accepts: impl Fn(char) -> bool, max: usize) -> String {
        let start = self.pos;
        while self.pos - start < max && self.chars.get(self.pos).is_some_and(|&c| accepts(c)) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }
}
