//! Character classes: bracket classes, the escapes that name a class, and
//! Unicode properties, as Oniguruma reads them, case-folded where asked.

use std::sync::OnceLock;

use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, HirKind};

use crate::error::Error;

use super::{Parser, syntax, unsupported};

// ========================================================================
// Constants
// ========================================================================

/// How many POSIX classes there are, for their cache.
const POSIX_COUNT: usize = 14;

/// How many hex digits `\xHH` reads at most.
const HEX_BYTE_DIGITS: usize = 2;

/// The radix of hex escapes.
const HEX: u32 = 16;

/// The radix of octal escapes.
const OCTAL: u32 = 8;

/// The most digits one code point of a list takes.
const CODE_POINT_DIGITS: usize = 8;

/// The highest code point an escape may name outside a class: the highest
/// four UTF-8 bytes led by `F4` encode, past Unicode's last.
const MAX_CODE_POINT: u32 = 0x13_FFFF;

/// The highest code point an escape may name inside a class, but for a
/// range's end, which may name any.
const MAX_CLASS_CODE_POINT: u32 = 0x1F_FFFF;

/// The most bytes a character takes in UTF-8.
const UTF8_MAX_BYTES: usize = 4;

/// How many hex digits `\uHHHH` reads.
const HEX_UNICODE_DIGITS: usize = 4;

/// How many octal digits `\0oo` reads after its zero.
const OCTAL_DIGITS: usize = 2;

/// The bits a control escape (`\cX`) keeps of its letter.
const CONTROL_MASK: u32 = 0x1F;

// ========================================================================
// Data Structures
// ========================================================================

/// What an escape stands for.
pub(super) enum Escaped {
    /// One character.
    Char(char),
    /// Several characters, from a code point list (`\x{41 42}`).
    Chars(Vec<char>),
    /// A class (`\d`, `\p{Greek}`).
    Class(ClassUnicode),
}

/// Oniguruma's POSIX classes, which are Unicode, also the classes `\w`,
/// `\d`, `\s`, and `\h` name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Posix {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Word,
    Xdigit,
}

// ========================================================================
// Parser Implementations
// ========================================================================

impl Parser {
    /// The class whose `[` was just read, past its `]`.
    pub(super) fn parse_class(&mut self) -> Result<ClassUnicode, Error> {
        let start = self.pos - 1;
        let negated = self.eat('^');
        let mut class = self.parse_class_items(start, true)?;
        if self.flags.casei {
            class = fold(class, self.flags.ascii_casei);
        }
        if negated {
            class.negate();
        }
        Ok(class)
    }

    /// A class's members up to and past its `]`; an intersection (`&&`)
    /// takes in the members after it.
    fn parse_class_items(&mut self, start: usize, first: bool) -> Result<ClassUnicode, Error> {
        let mut class = ClassUnicode::empty();
        let mut has_members = false;
        let mut first = first;
        loop {
            let Some(c) = self.peek() else {
                return Err(syntax("premature end of char-class", self.offset(start)));
            };
            // A `]` first is a literal.
            if c == ']' && !first {
                self.pos += 1;
                return Ok(class);
            }
            first = false;
            if c == '&' && self.chars.get(self.pos + 1) == Some(&'&') {
                self.pos += 2;
                let rest = self.parse_class_items(start, false)?;
                match has_members {
                    true => class.intersect(&rest),
                    false => class = rest,
                }
                return Ok(class);
            }
            has_members = true;
            if c == '[' {
                if let Some(posix) = self.posix_class(start)? {
                    class.union(&posix);
                    continue;
                }
                self.pos += 1;
                let nested = self.parse_class()?;
                class.union(&nested);
                continue;
            }
            let member_start = self.pos;
            let member = self.class_member()?;
            // A list range ends no range; after one, the dash is a literal.
            if !self.is_list_range(member_start)
                && self.is_range_dash()
                && self.is_list_range(self.pos + 1)
            {
                return Err(syntax("invalid code point value", self.offset(start)));
            }
            // A code point list's last code point may start a range.
            let low = match &member {
                Escaped::Char(c) => Some(*c),
                Escaped::Chars(chars) => chars.last().copied(),
                Escaped::Class(_) => None,
            };
            let Some(low) = low.filter(|_| self.is_range_dash()) else {
                push_escaped(&mut class, member);
                continue;
            };
            if let Escaped::Chars(chars) = &member {
                for &c in &chars[..chars.len() - 1] {
                    class.push(ClassUnicodeRange::new(c, c));
                }
            }
            self.pos += 1;
            self.range_end = true;
            let high_member = self.class_member();
            self.range_end = false;
            let high_member = high_member?;
            // A code point list's first code point may end a range.
            let high = match &high_member {
                Escaped::Char(c) => Some(*c),
                Escaped::Chars(chars) => chars.first().copied(),
                Escaped::Class(_) => None,
            };
            match high {
                Some(high) if high < low => {
                    return Err(syntax("empty range in char class", self.offset(start)));
                }
                Some(high) => {
                    class.push(ClassUnicodeRange::new(low, high));
                    if let Escaped::Chars(chars) = &high_member {
                        for &c in &chars[1..] {
                            class.push(ClassUnicodeRange::new(c, c));
                        }
                    }
                }
                // `[a-\d]`: the dash is a literal.
                None => {
                    class.push(ClassUnicodeRange::new(low, low));
                    class.push(ClassUnicodeRange::new('-', '-'));
                    push_escaped(&mut class, high_member);
                }
            }
        }
    }

    /// One member of a class at the parser, read past: a character or an
    /// escape.
    fn class_member(&mut self) -> Result<Escaped, Error> {
        let start = self.pos;
        match self.next() {
            Some('\\') => match self.next() {
                Some(c) => self.parse_escaped(c, true),
                None => Err(syntax("end pattern at escape", self.offset(start))),
            },
            Some(c) => Ok(Escaped::Char(c)),
            None => Err(syntax("premature end of char-class", self.offset(start))),
        }
    }

    /// Whether a dash at the parser makes a range: one followed by the
    /// class's end or by an intersection (`&&`) is a literal.
    fn is_range_dash(&self) -> bool {
        self.peek() == Some('-')
            && !matches!(&self.chars[self.pos + 1..], [] | [']', ..] | ['&', '&', ..])
    }

    /// Whether a code point list holding a range starts at `pos`
    /// (`\x{30-39}`), which may not end a range.
    fn is_list_range(&self, pos: usize) -> bool {
        matches!(self.chars.get(pos..pos + 3), Some(['\\', 'x' | 'o', '{']))
            && self.chars[pos + 3..]
                .iter()
                .take_while(|&&c| c != '}')
                .any(|&c| c == '-')
    }

    /// The POSIX class at the parser (`[:alpha:]`, `[:^digit:]`), read past;
    /// `None` when none is there, the `[` then opening a nested class. A
    /// name of letters that names no class is an error.
    fn posix_class(&mut self, start: usize) -> Result<Option<ClassUnicode>, Error> {
        if self.chars.get(self.pos + 1) != Some(&':') {
            return Ok(None);
        }
        let from = self.pos + 2;
        let negated = self.chars.get(from) == Some(&'^');
        let name_start = if negated { from + 1 } else { from };
        let Some(end) = (name_start..self.chars.len().saturating_sub(1))
            .find(|&i| self.chars[i] == ':' && self.chars[i + 1] == ']')
        else {
            return Ok(None);
        };
        let name: String = self.chars[name_start..end].iter().collect();
        let Some(posix) = Posix::from_name(&name) else {
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(syntax("invalid POSIX bracket type", self.offset(start)));
            }
            return Ok(None);
        };
        let mut class = self.posix(posix, self.is_ascii(posix));
        if negated {
            class.negate();
        }
        self.pos = end + 2;
        Ok(Some(class))
    }

    /// Whether the flags hold `posix` to ASCII where a POSIX bracket or a
    /// property names it: `P` for any, `W`, `D`, and `S` for their own.
    fn is_ascii(&self, posix: Posix) -> bool {
        self.flags.ascii_posix
            || match posix {
                Posix::Word => self.flags.ascii_word,
                Posix::Digit => self.flags.ascii_digit,
                Posix::Space => self.flags.ascii_space,
                Posix::Alnum
                | Posix::Alpha
                | Posix::Ascii
                | Posix::Blank
                | Posix::Cntrl
                | Posix::Graph
                | Posix::Lower
                | Posix::Print
                | Posix::Punct
                | Posix::Upper
                | Posix::Xdigit => false,
            }
    }

    /// The members of `posix`, only its ASCII ones when `ascii` says.
    fn posix(&self, posix: Posix, ascii: bool) -> ClassUnicode {
        let mut class = posix.class().clone();
        if ascii {
            class.intersect(Posix::Ascii.class());
        }
        class
    }

    /// The class a POSIX escape names, ASCII only when `ascii` says, and
    /// negated for the capital letter (`\D`).
    fn posix_escape(&self, posix: Posix, ascii: bool, negated: bool) -> Escaped {
        let mut class = self.posix(posix, ascii);
        if negated {
            class.negate();
        }
        Escaped::Class(class)
    }

    /// The escape `\c` whose `c` was just read, inside a class or out.
    pub(super) fn parse_escaped(&mut self, c: char, in_class: bool) -> Result<Escaped, Error> {
        let start = self.pos - 2;
        Ok(match c {
            'd' | 'D' => self.posix_escape(Posix::Digit, self.flags.ascii_digit, c == 'D'),
            'w' | 'W' => self.posix_escape(Posix::Word, self.flags.ascii_word, c == 'W'),
            's' | 'S' => self.posix_escape(Posix::Space, self.flags.ascii_space, c == 'S'),
            'h' | 'H' => self.posix_escape(Posix::Xdigit, false, c == 'H'),
            'p' | 'P' if self.peek() == Some('{') => self.property(c == 'P', start)?,
            't' => Escaped::Char('\t'),
            'n' => Escaped::Char('\n'),
            'r' => Escaped::Char('\r'),
            'f' => Escaped::Char('\u{C}'),
            'v' => Escaped::Char('\u{B}'),
            'a' => Escaped::Char('\u{7}'),
            'e' => Escaped::Char('\u{1B}'),
            'b' if in_class => Escaped::Char('\u{8}'),
            'x' if self.eat('{') => self.code_point_list(HEX, in_class, start)?,
            'x' => Escaped::Char(self.hex_byte(start)?),
            'u' => match self.hex_digits(HEX_UNICODE_DIGITS) {
                HEX_UNICODE_DIGITS => Escaped::Char(self.code_point(
                    HEX,
                    HEX_UNICODE_DIGITS,
                    MAX_CODE_POINT,
                    start,
                )?),
                _ => return Err(syntax("too short digits", self.offset(start))),
            },
            'o' if self.eat('{') => self.code_point_list(OCTAL, in_class, start)?,
            'c' => Escaped::Char(self.control(start)?),
            'C' if self.eat('-') => Escaped::Char(self.control(start)?),
            '0' => {
                let c = self.octal(OCTAL_DIGITS).unwrap_or('\0');
                Escaped::Char(c)
            }
            '1'..='7' if in_class => {
                self.pos -= 1;
                Escaped::Char(self.octal(3).expect("an octal digit"))
            }
            // Any other escaped character is itself (`\<`, `\V`).
            c => Escaped::Char(c),
        })
    }

    /// The control character of the letter after `\c` or `\C-` (`\ca`,
    /// `\c\\`), read past.
    fn control(&mut self, start: usize) -> Result<char, Error> {
        let Some(mut letter) = self.next() else {
            return Err(syntax("end pattern at control", self.offset(start)));
        };
        if letter == '\\' {
            letter = self
                .next()
                .ok_or_else(|| syntax("end pattern at control", self.offset(start)))?;
        }
        Ok(char::from_u32(u32::from(letter) & CONTROL_MASK).expect("a control character"))
    }

    /// The `\xHH` escape whose `x` was just read. A byte past ASCII starts
    /// the UTF-8 of a character when the escapes after it complete one
    /// (`\xca\xb8` is `ʸ`), and is itself as a code point otherwise.
    fn hex_byte(&mut self, start: usize) -> Result<char, Error> {
        let digits = self.hex_digits(HEX_BYTE_DIGITS);
        if digits == 0 {
            return Err(syntax("invalid code point value", self.offset(start)));
        }
        let first = self.code_point(HEX, digits, MAX_CODE_POINT, start)?;
        if first.is_ascii() || digits < HEX_BYTE_DIGITS {
            return Ok(first);
        }
        let mut bytes = vec![u32::from(first) as u8];
        let resume = self.pos;
        while bytes.len() < UTF8_MAX_BYTES
            && self.chars.get(self.pos) == Some(&'\\')
            && self.chars.get(self.pos + 1) == Some(&'x')
            && self.chars[self.pos + 2..]
                .iter()
                .take(HEX_BYTE_DIGITS)
                .filter(|c| c.is_ascii_hexdigit())
                .count()
                == HEX_BYTE_DIGITS
        {
            let text: String = self.chars[self.pos + 2..self.pos + 4].iter().collect();
            bytes.push(u8::from_str_radix(&text, 16).expect("two hex digits"));
            self.pos += 4;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                return Ok(text.chars().next().expect("one character"));
            }
        }
        self.pos = resume;
        Ok(first)
    }

    /// The code point list whose `{` was just read, in `radix`, up to and
    /// past its `}`: code points apart by whitespace (`\x{41 42}`), and
    /// inside a class, ranges (`[\x{30-39 41}]`).
    fn code_point_list(
        &mut self,
        radix: u32,
        in_class: bool,
        start: usize,
    ) -> Result<Escaped, Error> {
        let invalid = |parser: &Self| syntax("invalid code point value", parser.offset(start));
        let mut chars = Vec::new();
        let mut class = ClassUnicode::empty();
        // Whether the last item was a range, and whether any was.
        let mut ranges = false;
        let mut has_ranges = false;
        // Whether a space follows a single code point last: in a class, the
        // list may not end so.
        let mut spaced = false;
        loop {
            if self.skip_list_space() && !ranges {
                spaced = true;
            }
            if self.eat('}') {
                if in_class && spaced && !chars.is_empty() {
                    return Err(invalid(self));
                }
                break;
            }
            let digits = self.digits(radix);
            if digits == 0 || digits > CODE_POINT_DIGITS {
                return Err(invalid(self));
            }
            // A range's end may name any code point, past the limit.
            let max = match (in_class, self.range_end && chars.is_empty() && !has_ranges) {
                (false, _) => MAX_CODE_POINT,
                (true, false) => MAX_CLASS_CODE_POINT,
                (true, true) => u32::MAX,
            };
            let low = self.code_point(radix, digits, max, start)?;
            spaced = self.skip_list_space();
            ranges = false;
            if !(in_class && self.eat('-')) {
                chars.push(low);
                continue;
            }
            self.skip_list_space();
            let digits = self.digits(radix);
            if digits == 0 || digits > CODE_POINT_DIGITS {
                return Err(invalid(self));
            }
            let high = self.code_point(radix, digits, u32::MAX, start)?;
            if high < low {
                return Err(invalid(self));
            }
            class.push(ClassUnicodeRange::new(low, high));
            ranges = true;
            spaced = false;
            has_ranges = true;
        }
        if has_ranges {
            for c in chars {
                class.push(ClassUnicodeRange::new(c, c));
            }
            return Ok(Escaped::Class(class));
        }
        Ok(match chars.len() {
            0 => return Err(invalid(self)),
            1 => Escaped::Char(chars[0]),
            _ => Escaped::Chars(chars),
        })
    }

    /// Skips the whitespace between a list's code points; whether any.
    fn skip_list_space(&mut self) -> bool {
        let from = self.pos;
        while self.peek().is_some_and(|c| c == ' ' || c == '\n') {
            self.pos += 1;
        }
        self.pos > from
    }

    /// How many digits of `radix` are at the parser.
    fn digits(&self, radix: u32) -> usize {
        self.chars[self.pos..]
            .iter()
            .take_while(|c| c.is_digit(radix))
            .count()
    }

    /// How many hex digits, at most `max`, are at the parser.
    fn hex_digits(&self, max: usize) -> usize {
        self.chars[self.pos..]
            .iter()
            .take(max)
            .take_while(|c| c.is_ascii_hexdigit())
            .count()
    }

    /// The code point the `digits` digits of `radix` at the parser make,
    /// read past; an error past `max`. One past Unicode's last is its last,
    /// as a class range's end may reach past it (`[\x{7f}-\x{7fffffff}]`).
    fn code_point(
        &mut self,
        radix: u32,
        digits: usize,
        max: u32,
        start: usize,
    ) -> Result<char, Error> {
        let text: String = self.chars[self.pos..self.pos + digits].iter().collect();
        self.pos += digits;
        u32::from_str_radix(&text, radix)
            .ok()
            .filter(|&code| code <= max)
            .map(|code| code.min(u32::from(char::MAX)))
            .and_then(char::from_u32)
            .ok_or_else(|| syntax("invalid code point value", self.offset(start)))
    }

    /// The property escape whose `p` or `P` was just read, at its `{`.
    fn property(&mut self, negated: bool, start: usize) -> Result<Escaped, Error> {
        self.pos += 1;
        let from = self.pos;
        while self.peek().is_some_and(|c| c != '}') {
            self.pos += 1;
        }
        let name: String = self.chars[from..self.pos].iter().collect();
        if !self.eat('}') {
            return Err(syntax(
                "invalid character property name",
                self.offset(start),
            ));
        }
        let (name, negated) = match name.strip_prefix('^') {
            Some(name) => (name.to_owned(), !negated),
            None => (name, negated),
        };
        let posix_name = normalize(&name);
        let posix_name = posix_name.strip_prefix("posix").unwrap_or(&posix_name);
        let named =
            Posix::from_name(posix_name).map(|posix| self.posix(posix, self.is_ascii(posix)));
        let Some(mut class) = named.or_else(|| property(&name)) else {
            if normalize(&name).starts_with("in") {
                let message = format!("Unicode block property {{{name}}}");
                return Err(unsupported(message, self.offset(start)));
            }
            let message = format!("invalid character property name {{{name}}}");
            return Err(syntax(message, self.offset(start)));
        };
        if negated {
            class.negate();
        }
        Ok(Escaped::Class(class))
    }
}

// ========================================================================
// Posix Implementations
// ========================================================================

impl Posix {
    /// The class named `name`, as a POSIX bracket or a property names it,
    /// ignoring case, spaces, underscores, and dashes.
    fn from_name(name: &str) -> Option<Self> {
        Some(match normalize(name).as_str() {
            "alnum" => Self::Alnum,
            "alpha" => Self::Alpha,
            "ascii" => Self::Ascii,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "digit" => Self::Digit,
            "graph" => Self::Graph,
            "lower" => Self::Lower,
            "print" => Self::Print,
            "punct" => Self::Punct,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "word" => Self::Word,
            "xdigit" => Self::Xdigit,
            _ => return None,
        })
    }

    /// Its members, built the first time.
    pub(crate) fn class(self) -> &'static ClassUnicode {
        static CACHE: [OnceLock<ClassUnicode>; POSIX_COUNT] =
            [const { OnceLock::new() }; POSIX_COUNT];
        CACHE[self as usize].get_or_init(|| unicode_class(self.definition()))
    }

    /// Its members, as Oniguruma defines them for Unicode, in the syntax of
    /// `regex-syntax`.
    fn definition(self) -> &'static str {
        match self {
            Self::Alnum => r"[\p{Alphabetic}\p{Nd}]",
            Self::Alpha => r"\p{Alphabetic}",
            Self::Ascii => r"[\x00-\x7F]",
            Self::Blank => r"[\p{Zs}\t]",
            Self::Cntrl => r"[\p{Cc}\p{Cf}\p{Cn}\p{Co}]",
            Self::Digit => r"\p{Nd}",
            Self::Graph => r"[^\p{White_Space}\p{Cc}\p{Cn}]",
            Self::Lower => r"\p{Ll}",
            Self::Print => r"[[^\p{White_Space}\p{Cc}\p{Cn}]\p{Zs}]",
            Self::Punct => r"[\p{P}\p{S}]",
            Self::Space => r"\p{White_Space}",
            Self::Upper => r"\p{Lu}",
            Self::Word => r"[\p{L}\p{M}\p{Nd}\p{Pc}]",
            Self::Xdigit => r"[0-9A-Fa-f]",
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// `class` with every member's other cases added, only ASCII ones when
/// `ascii` says (Oniguruma's `I`). Only characters with a
/// case mapping fold to others, so only the class's part among them is
/// folded: folding visits every character it covers, and a wide class
/// (`[\x{7f}-\x{10ffff}]`) would cost milliseconds.
pub(super) fn fold(mut class: ClassUnicode, ascii: bool) -> ClassUnicode {
    static CASED: OnceLock<ClassUnicode> = OnceLock::new();
    let cased = CASED
        .get_or_init(|| unicode_class(r"[\p{Changes_When_Casemapped}\p{Changes_When_Casefolded}]"));
    let mut folded = class.clone();
    folded.intersect(cased);
    if ascii {
        folded.intersect(Posix::Ascii.class());
    }
    // Folding fails only without Unicode case data, which is built in.
    let _ = folded.try_case_fold_simple();
    if ascii {
        folded.intersect(Posix::Ascii.class());
    }
    class.union(&folded);
    class
}

/// Adds what `escaped` stands for to `class`.
fn push_escaped(class: &mut ClassUnicode, escaped: Escaped) {
    match escaped {
        Escaped::Char(c) => class.push(ClassUnicodeRange::new(c, c)),
        Escaped::Chars(chars) => {
            for c in chars {
                class.push(ClassUnicodeRange::new(c, c));
            }
        }
        Escaped::Class(other) => class.union(&other),
    }
}

/// The class of the property `name` (`Greek`, `Lu`, `Alpha`), as Oniguruma
/// names properties; `None` for a name it does not know.
fn property(name: &str) -> Option<ClassUnicode> {
    let posix_name = normalize(name);
    let posix_name = posix_name.strip_prefix("posix").unwrap_or(&posix_name);
    if let Some(posix) = Posix::from_name(posix_name) {
        return Some(posix.class().clone());
    }
    match normalize(name).as_str() {
        "any" => Some(ClassUnicode::new([ClassUnicodeRange::new('\0', char::MAX)])),
        // Surrogates never occur in UTF-8 text.
        "surrogate" | "cs" => Some(ClassUnicode::empty()),
        "assigned" => {
            let mut class = parse_class_source(r"\p{Cn}")?;
            class.negate();
            Some(class)
        }
        _ => parse_class_source(&format!(r"\p{{{name}}}")),
    }
}

/// `name` lowercased, without spaces, underscores, or dashes, as property
/// names compare.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The class `source`, in the syntax of `regex-syntax`, which must parse.
pub(super) fn unicode_class(source: &str) -> ClassUnicode {
    parse_class_source(source).expect("a valid class definition")
}

/// The class `source`, in the syntax of `regex-syntax`, if it is one.
fn parse_class_source(source: &str) -> Option<ClassUnicode> {
    let hir = regex_syntax::parse(source).ok()?;
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => Some(class.clone()),
        HirKind::Literal(literal) => {
            let c = std::str::from_utf8(&literal.0).ok()?.chars().next()?;
            Some(ClassUnicode::new([ClassUnicodeRange::new(c, c)]))
        }
        _ => None,
    }
}
