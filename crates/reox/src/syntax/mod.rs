//! Patterns in Oniguruma's syntax, as TextMate grammars write them, parsed
//! into the tree the compiler reads.

mod ast;
mod calls;
mod class;
mod fold;

use std::iter;

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

use crate::error::{Error, PatternError};

use self::calls::check_calls;
use self::class::{Escaped, fold, unicode_class};
use self::fold::{class_folds, fold_literals};

pub(crate) use self::ast::{Assertion, BackRef, Capture, Conditional, Look, Node, Repeat};
pub(crate) use self::calls::Calls;
pub(crate) use self::class::Posix;

// ========================================================================
// Constants
// ========================================================================

/// The largest count a repetition may name, as in Oniguruma.
const MAX_REPEAT: u32 = 100_000;

/// The highest group a single-digit back-reference (`\1` to `\9`) names
/// even before the group is defined; past it, a number is a back-reference
/// only to a group already defined, and otherwise octal.
const SINGLE_DIGIT_GROUPS: usize = 9;

// ========================================================================
// Data Structures
// ========================================================================

/// The inline flags in effect at a point of a pattern.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
    /// Whether only ASCII letters match either case (Oniguruma's `I`).
    pub(crate) ascii_casei: bool,
    /// Whether `\d` is ASCII digits only (`D`).
    pub(crate) ascii_digit: bool,
    /// Whether POSIX brackets hold ASCII characters only (`P`).
    pub(crate) ascii_posix: bool,
    /// Whether `\s` is ASCII whitespace only (`S`).
    pub(crate) ascii_space: bool,
    /// Whether `\w` is ASCII word characters only (`W`).
    pub(crate) ascii_word: bool,
    /// Whether letters match either case (`i`).
    pub(crate) casei: bool,
    /// Whether `.` matches a newline (Oniguruma's `m`).
    pub(crate) dotall: bool,
    /// Whether whitespace and `#` comments are ignored (`x`).
    pub(crate) extended: bool,
}

/// A parsed pattern.
#[derive(Debug)]
pub(crate) struct Parsed {
    /// What compiling its calls needs to know of its groups.
    pub(crate) calls: Calls,
    /// How many capturing groups it has.
    pub(crate) groups: usize,
    /// The names of its named groups, with their numbers.
    pub(crate) names: Vec<(String, usize)>,
    /// Its tree.
    pub(crate) node: Node,
}

/// The state of parsing one pattern.
struct Parser {
    /// The pattern, as characters so a position indexes it directly.
    chars: Vec<char>,
    /// The flags in effect.
    flags: Flags,
    /// How many capturing groups have opened.
    groups: usize,
    /// The highest group a call names by number, and where it is.
    highest_call: Option<(usize, usize)>,
    /// The highest group a back-reference names, and where it is.
    highest_reference: Option<(usize, usize)>,
    /// Every named group of the pattern, from parsing it before, which
    /// calls by name refer to; `None` on the first parse.
    known_names: Option<Vec<(String, usize)>>,
    /// The named groups so far, with their numbers.
    names: Vec<(String, usize)>,
    /// Whether a call names its group, so the pattern is parsed again once
    /// every name is known: a call may name a group defined after it.
    named_calls: bool,
    /// Where the parser is, in characters.
    pos: usize,
    /// Whether the class member being read ends a range, so its first code
    /// point may be any (`[a-\x{7fffffff}]`).
    range_end: bool,
}

/// One item of a pattern, before any quantifier.
enum Atom {
    /// Literal characters, as written: a run of them is matched as one
    /// ignoring case, as a character may fold to several (`ß` to `ss`).
    Literals(Vec<char>),
    /// Anything else.
    Node(Node),
}

/// An interval quantifier, as written.
struct Interval {
    /// Where it ends, just past its `}`.
    end: usize,
    /// Its upper bound; `None` for none (`{2,}`).
    max: Option<u32>,
    /// Its lower bound.
    min: u32,
}

// ========================================================================
// Parser Implementations
// ========================================================================

impl Parser {
    // ----------------------------------------------------------------
    // Structure
    // ----------------------------------------------------------------

    /// Alternatives up to the end of the pattern or the group's `)`.
    fn parse_alt(&mut self) -> Result<Node, Error> {
        let mut alternatives = vec![self.parse_concat()?];
        while self.eat('|') {
            alternatives.push(self.parse_concat()?);
        }
        Ok(match alternatives.len() {
            1 => alternatives.pop().expect("one alternative"),
            _ => Node::Alt(alternatives),
        })
    }

    /// Items one after another up to a `|`, a `)`, or the pattern's end.
    fn parse_concat(&mut self) -> Result<Node, Error> {
        let mut items = Vec::new();
        let mut literals = Vec::new();
        loop {
            self.skip_extended();
            match self.peek() {
                None | Some('|' | ')') => break,
                Some('(') if self.isolated_flags().is_some() => {
                    self.push_literals(&mut items, &mut literals);
                    // Flags alone apply to the rest of the group, its later
                    // alternatives included: `a(?i)b|c` is `a(?i:b|c)`.
                    let (flags, end) = self.isolated_flags().expect("isolated flags");
                    let saved = self.flags;
                    self.flags = flags;
                    self.pos = end;
                    items.push(self.parse_alt()?);
                    self.flags = saved;
                    break;
                }
                Some(_) => match self.parse_atom()? {
                    Atom::Literals(chars) => {
                        let Some((&last, init)) = chars.split_last() else {
                            continue;
                        };
                        literals.extend(init);
                        // A quantifier takes the last literal only.
                        match self.parse_quantifiers(self.folded_literal(last))? {
                            quantified @ (Node::Repeat(_) | Node::Atomic(_)) => {
                                self.push_literals(&mut items, &mut literals);
                                items.push(quantified);
                            }
                            _ => literals.push(last),
                        }
                    }
                    Atom::Node(node) => {
                        let node = self.parse_quantifiers(node)?;
                        self.push_literals(&mut items, &mut literals);
                        items.push(node);
                    }
                },
            }
        }
        self.push_literals(&mut items, &mut literals);
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().expect("one item"),
            _ => Node::Concat(items),
        })
    }

    /// Appends the run `literals` to `items`, emptying it: matched as one
    /// ignoring case, where a character may fold to several.
    fn push_literals(&self, items: &mut Vec<Node>, literals: &mut Vec<char>) {
        if literals.is_empty() {
            return;
        }
        let folded = match self.flags.casei && !self.flags.ascii_casei {
            true => fold_literals(literals),
            false => None,
        };
        match folded {
            Some(parts) => items.extend(parts),
            None => items.extend(literals.iter().map(|&c| self.literal(c))),
        }
        literals.clear();
    }

    /// The literal `c` alone, matching either case where the flags say, a
    /// character that folds to several included.
    fn folded_literal(&self, c: char) -> Node {
        let folded = match self.flags.casei && !self.flags.ascii_casei {
            true => fold_literals(&[c]),
            false => None,
        };
        match folded {
            Some(parts) => Node::Concat(parts),
            None => self.literal(c),
        }
    }

    /// One item, before any quantifier.
    fn parse_atom(&mut self) -> Result<Atom, Error> {
        let start = self.pos;
        let c = self.next().expect("an item");
        Ok(Atom::Node(match c {
            // A group holding only an anchor may be repeated, as the anchor
            // alone may not: kept apart from a bare one.
            '(' => match self.parse_group()? {
                assertion @ Node::Assert(_) => Node::Concat(vec![assertion]),
                group => group,
            },
            '[' => self.parse_bracket()?,
            '.' if self.flags.dotall => Node::Any,
            '.' => Node::AnyButNewline,
            '^' => Node::Assert(Assertion::LineStart),
            '$' => Node::Assert(Assertion::LineEnd),
            '\\' => return self.parse_escape(),
            '*' | '+' | '?' => {
                return Err(syntax(
                    "target of repeat operator is not specified",
                    self.offset(start),
                ));
            }
            '{' if self.interval(start)?.is_some() => {
                return Err(syntax(
                    "target of repeat operator is not specified",
                    self.offset(start),
                ));
            }
            c => return Ok(Atom::Literals(vec![c])),
        }))
    }

    /// The bracket class whose `[` was just read, past its `]`: ignoring
    /// case, also what its few members that fold to several characters
    /// fold to (`[ß]` matching `ss`).
    fn parse_bracket(&mut self) -> Result<Node, Error> {
        let negated = self.peek() == Some('^');
        let class = self.parse_class()?;
        if !self.flags.casei || self.flags.ascii_casei || negated {
            return Ok(single_or_class(class));
        }
        let folds = class_folds(&class);
        let node = single_or_class(class);
        Ok(match folds.is_empty() {
            true => node,
            false => Node::Alt(iter::once(node).chain(folds).collect()),
        })
    }

    /// `node` with the quantifiers after it applied, innermost first.
    fn parse_quantifiers(&mut self, mut node: Node) -> Result<Node, Error> {
        loop {
            self.skip_extended();
            let start = self.pos;
            let (min, max, reversed) = match self.peek() {
                Some('*') => (0, None, false),
                Some('+') => (1, None, false),
                Some('?') => (0, Some(1), false),
                Some('{') => match self.interval(start)? {
                    // A reversed interval, `{3,2}`, is possessive `{2,3}`.
                    Some(Interval {
                        end,
                        max: Some(max),
                        min,
                    }) if max < min => {
                        self.pos = end - 1;
                        (max, Some(min), true)
                    }
                    Some(Interval { end, max, min }) => {
                        self.pos = end - 1;
                        (min, max, false)
                    }
                    None => return Ok(node),
                },
                _ => return Ok(node),
            };
            self.pos += 1;
            if matches!(node, Node::Assert(_)) {
                return Err(syntax(
                    "target of repeat operator is invalid",
                    self.offset(start),
                ));
            }
            if let Node::Repeat(inner) = &node
                && inner.max == Some(inner.min)
                && max == Some(min)
                && u64::from(inner.min) * u64::from(min) > u64::from(MAX_REPEAT)
            {
                return Err(syntax(
                    "too big number for repeat range",
                    self.offset(start),
                ));
            }
            let interval = self.chars[start] == '{';
            // After an interval, `+` is another repetition, and so is `?`
            // after an exact count written without a comma (`a{2}?` is
            // `(?:a{2})?`, `a{2,2}?` lazy).
            let exact = interval && !self.chars[start..self.pos].contains(&',');
            let (greedy, possessive) = match self.peek() {
                _ if reversed => (true, true),
                Some('?') if !exact => (false, false),
                Some('+') if !interval => (true, true),
                _ => (true, false),
            };
            if !reversed && (!greedy || possessive) {
                self.pos += 1;
            }
            let repeat = Node::Repeat(Repeat {
                greedy,
                max,
                min,
                node: Box::new(node),
            });
            node = match possessive {
                true => Node::Atomic(Box::new(repeat)),
                false => repeat,
            };
        }
    }

    /// The group whose `(` was just read.
    fn parse_group(&mut self) -> Result<Node, Error> {
        let open = self.pos - 1;
        if self.peek() == Some('*') {
            return Err(unsupported("callout", self.offset(open)));
        }
        if !self.eat('?') {
            return self.parse_capture(None);
        }
        match self.next() {
            Some(':') => self.parse_body(),
            Some('=') => self.parse_look(false, false),
            Some('!') => self.parse_look(false, true),
            Some('>') => Ok(Node::Atomic(Box::new(self.parse_body()?))),
            Some('#') => {
                while self.next().is_some_and(|c| c != ')') {}
                Ok(Node::Empty)
            }
            Some('<') => match self.peek() {
                Some('=') => {
                    self.pos += 1;
                    self.parse_look(true, false)
                }
                Some('!') => {
                    self.pos += 1;
                    self.parse_look(true, true)
                }
                _ => {
                    let name = self.parse_name('>')?;
                    self.parse_capture(Some(name))
                }
            },
            Some('\'') => {
                let name = self.parse_name('\'')?;
                self.parse_capture(Some(name))
            }
            Some('~') => self.parse_absent(open),
            Some('(') => self.parse_conditional(open),
            _ => {
                self.pos = open;
                let Some((flags, end)) = self.scoped_flags() else {
                    // Longest match (`L`) and text segments (`y{g}`).
                    let options: String = self.chars[open + 2..]
                        .iter()
                        .take_while(|&&c| c != ':' && c != ')')
                        .collect();
                    if options.contains('L') || options.contains("y{") {
                        return Err(unsupported("option", self.offset(open)));
                    }
                    return Err(syntax("undefined group option", self.offset(open)));
                };
                let saved = self.flags;
                self.flags = flags;
                self.pos = end;
                let body = self.parse_body();
                // ASCII-only case with a body applies to the whole pattern,
                // so nothing but the groups around it may close after it.
                if body.is_ok()
                    && flags.ascii_casei
                    && !saved.ascii_casei
                    && self.chars[self.pos..].iter().any(|&c| c != ')')
                {
                    return Err(syntax("invalid group option", self.offset(open)));
                }
                self.flags = saved;
                body
            }
        }
    }

    /// A capturing group's body, numbered as its `(` is counted.
    fn parse_capture(&mut self, name: Option<String>) -> Result<Node, Error> {
        self.groups += 1;
        let index = self.groups;
        if let Some(name) = name {
            self.names.push((name, index));
        }
        let node = self.parse_body()?;
        Ok(Node::Capture(Capture {
            index,
            node: Box::new(node),
        }))
    }

    /// A lookaround's body.
    fn parse_look(&mut self, behind: bool, negated: bool) -> Result<Node, Error> {
        let node = self.parse_body()?;
        Ok(Node::Look(Look {
            behind,
            negated,
            node: Box::new(node),
        }))
    }

    /// An absent operator (`(?~absent)`), matching zero or more characters
    /// where `absent` does not match at any position.
    fn parse_absent(&mut self, open: usize) -> Result<Node, Error> {
        if self.peek() == Some('|') {
            return Err(unsupported("absent operator", self.offset(open)));
        }
        let absent = self.parse_body()?;
        let step = Node::Concat(vec![
            Node::Look(Look {
                behind: false,
                negated: true,
                node: Box::new(absent),
            }),
            Node::Any,
        ]);
        Ok(Node::Repeat(Repeat {
            greedy: true,
            max: None,
            min: 0,
            node: Box::new(step),
        }))
    }

    /// A conditional group (`(?(condition)yes|no)` or `(?(condition)yes)`).
    fn parse_conditional(&mut self, open: usize) -> Result<Node, Error> {
        let start = self.pos;
        let groups: Box<[usize]> = match self.peek() {
            Some('<') => {
                self.pos += 1;
                self.parse_conditional_named('>', open, start)?
            }
            Some('\'') => {
                self.pos += 1;
                self.parse_conditional_named('\'', open, start)?
            }
            Some(c) if c.is_ascii_digit() => {
                let n = self.parse_group_number(start)?;
                if matches!(self.peek(), Some('+' | '-')) {
                    let sign = self.next();
                    let level = self.parse_group_number_allow_zero(start)?;
                    if level != 0 || sign.is_none() {
                        return Err(unsupported("conditional group", self.offset(open)));
                    }
                }
                Box::new([n])
            }
            _ => return Err(unsupported("conditional group", self.offset(open))),
        };
        if !self.eat(')') {
            return Err(syntax("invalid conditional pattern", self.offset(open)));
        }
        let saved = self.flags;
        let yes = self.parse_concat()?;
        self.flags = saved;
        let no = match self.eat('|') {
            true => self.parse_alt()?,
            false => Node::Alt(Vec::new()),
        };
        self.flags = saved;
        if !self.eat(')') {
            return Err(syntax(
                "end pattern with unmatched parenthesis",
                self.offset(start),
            ));
        }
        Ok(Node::Conditional(Conditional {
            groups,
            no: Box::new(no),
            yes: Box::new(yes),
        }))
    }

    /// A group name or number inside `<...>` or `'...'` of a conditional.
    fn parse_conditional_named(
        &mut self,
        close: char,
        open: usize,
        start: usize,
    ) -> Result<Box<[usize]>, Error> {
        let mut text = String::new();
        loop {
            match self.next() {
                Some(c) if c == close => break,
                Some(c) if c.is_alphanumeric() || c == '_' || c == '-' => text.push(c),
                Some(_) | None => return Err(syntax("invalid group name", self.offset(start))),
            }
        }
        if text.is_empty() {
            return Err(syntax("invalid group name", self.offset(start)));
        }
        if text.chars().all(|c| c.is_ascii_digit()) {
            let n: usize = text
                .parse()
                .map_err(|_| syntax("too big number for reference", self.offset(start)))?;
            if n == 0 {
                return Err(syntax("invalid conditional pattern", self.offset(open)));
            }
            if self.highest_reference.is_none_or(|(known, _)| n > known) {
                self.highest_reference = Some((n, start));
            }
            return Ok(Box::new([n]));
        }
        if text.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(syntax("invalid group name", self.offset(start)));
        }
        self.named_groups(&text, start)
    }

    /// A positive group number at the parser, recording it for bounds checks.
    fn parse_group_number(&mut self, start: usize) -> Result<usize, Error> {
        let n = self.parse_group_number_allow_zero(start)?;
        if n == 0 {
            return Err(syntax("invalid conditional pattern", self.offset(start)));
        }
        if self.highest_reference.is_none_or(|(known, _)| n > known) {
            self.highest_reference = Some((n, start));
        }
        Ok(n)
    }

    /// A non-negative decimal integer at the parser.
    fn parse_group_number_allow_zero(&mut self, start: usize) -> Result<usize, Error> {
        let mut n = 0usize;
        let mut any = false;
        while let Some(d) = self.peek().filter(char::is_ascii_digit) {
            self.pos += 1;
            any = true;
            n = n
                .checked_mul(10)
                .and_then(|v| v.checked_add((d as u8 - b'0') as usize))
                .ok_or_else(|| syntax("too big number for reference", self.offset(start)))?;
        }
        if !any {
            return Err(syntax("invalid conditional pattern", self.offset(start)));
        }
        Ok(n)
    }

    /// All group numbers named `name`, most recently opened first.
    fn named_groups(&self, name: &str, start: usize) -> Result<Box<[usize]>, Error> {
        let groups: Vec<usize> = self
            .names
            .iter()
            .rev()
            .filter(|(known, _)| known == name)
            .map(|(_, group)| *group)
            .collect();
        if groups.is_empty() {
            let message = format!("undefined name <{name}> reference");
            return Err(syntax(message, self.offset(start)));
        }
        Ok(groups.into())
    }

    /// A group's body up to and past its `)`, its flags kept to it.
    fn parse_body(&mut self) -> Result<Node, Error> {
        let start = self.pos;
        let saved = self.flags;
        let node = self.parse_alt()?;
        self.flags = saved;
        if !self.eat(')') {
            return Err(syntax(
                "end pattern with unmatched parenthesis",
                self.offset(start),
            ));
        }
        Ok(node)
    }

    /// A group name up to `close`, past it.
    fn parse_name(&mut self, close: char) -> Result<String, Error> {
        let start = self.pos;
        let mut name = String::new();
        loop {
            match self.next() {
                Some(c) if c == close => break,
                Some(c) if c.is_alphanumeric() || c == '_' || c == '-' => name.push(c),
                Some(_) | None => return Err(syntax("invalid group name", self.offset(start))),
            }
        }
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(syntax("invalid group name", self.offset(start)));
        }
        Ok(name)
    }

    // ----------------------------------------------------------------
    // Escapes
    // ----------------------------------------------------------------

    /// The escape whose backslash was just read, outside a class.
    fn parse_escape(&mut self) -> Result<Atom, Error> {
        let start = self.pos - 1;
        let Some(c) = self.next() else {
            return Err(syntax("end pattern at escape", self.offset(start)));
        };
        Ok(Atom::Node(match c {
            'A' => Node::Assert(Assertion::TextStart),
            'z' => Node::Assert(Assertion::TextEnd),
            'Z' => Node::Assert(Assertion::TextEndNewline),
            'G' => Node::Assert(Assertion::SearchStart),
            'b' if self.flags.ascii_word || self.flags.ascii_posix => {
                Node::Assert(Assertion::AsciiWordBoundary)
            }
            'B' if self.flags.ascii_word || self.flags.ascii_posix => {
                Node::Assert(Assertion::AsciiNotWordBoundary)
            }
            'b' => Node::Assert(Assertion::WordBoundary),
            'B' => Node::Assert(Assertion::NotWordBoundary),
            'K' => Node::Keep,
            'N' => Node::AnyButNewline,
            'O' => Node::Any,
            'R' => linebreak(),
            'X' => grapheme_cluster(),
            'y' | 'Y' => {
                return Err(unsupported("text segments", self.offset(start)));
            }
            'g' if matches!(self.peek(), Some('<' | '\'')) => self.parse_call(start)?,
            'k' => self.parse_named_reference(start)?,
            '1'..='9' => self.parse_numbered_reference(start)?,
            c => match self.parse_escaped(c, false)? {
                Escaped::Char(c) => return Ok(Atom::Literals(vec![c])),
                Escaped::Chars(chars) => return Ok(Atom::Literals(chars)),
                Escaped::Class(class) => single_or_class(class),
            },
        }))
    }

    /// The back-reference or octal escape whose first digit was just read:
    /// `\1` to `\9` always refer to a group; a longer number does if that
    /// many groups have opened, and is octal otherwise.
    fn parse_numbered_reference(&mut self, start: usize) -> Result<Node, Error> {
        let first = self.pos - 1;
        let digits = self.chars[first..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let number: String = self.chars[first..first + digits].iter().collect();
        let number = number.parse::<usize>().unwrap_or(usize::MAX);
        if number <= SINGLE_DIGIT_GROUPS || number <= self.groups {
            let group = match number <= SINGLE_DIGIT_GROUPS {
                true => self.chars[first].to_digit(10).expect("a digit") as usize,
                false => number,
            };
            self.pos = match number <= SINGLE_DIGIT_GROUPS {
                true => first + 1,
                false => first + digits,
            };
            return Ok(self.reference(vec![group], start));
        }
        self.pos = first;
        match self.octal(3) {
            Some(c) => Ok(self.literal(c)),
            None => Err(syntax("invalid backref number/name", self.offset(start))),
        }
    }

    /// The back-reference `\k<name>`, `\k'name'`, `\k<n>`, or `\k<-n>`
    /// whose `k` was just read.
    fn parse_named_reference(&mut self, start: usize) -> Result<Node, Error> {
        let close = match self.next() {
            Some('<') => '>',
            Some('\'') => '\'',
            _ => return Err(syntax("invalid backref number/name", self.offset(start))),
        };
        let from = self.pos;
        while self.peek().is_some_and(|c| c != close) {
            self.pos += 1;
        }
        let reference: String = self.chars[from..self.pos].iter().collect();
        if !self.eat(close) {
            return Err(syntax("invalid backref number/name", self.offset(start)));
        }
        // A level of recursion (`\k<name+1>`) picks a group's match in a
        // call nested in this one.
        let level = reference.rfind(['+', '-']).filter(|&at| at > 0);
        if level.is_some_and(|at| {
            at + 1 < reference.len() && reference[at + 1..].chars().all(|c| c.is_ascii_digit())
        }) {
            return Err(unsupported(
                "back-reference with recursion level",
                self.offset(start),
            ));
        }
        let groups = match reference.parse::<isize>() {
            Ok(number) if number > 0 => vec![number as usize],
            Ok(number) if number < 0 && number.unsigned_abs() <= self.groups => {
                vec![self.groups + 1 - number.unsigned_abs()]
            }
            Ok(_) => return Err(syntax("invalid backref number/name", self.offset(start))),
            Err(_) => self
                .names
                .iter()
                .filter(|(name, _)| *name == reference)
                .map(|(_, index)| *index)
                .collect(),
        };
        if groups.is_empty() {
            let message = format!("undefined name <{reference}> reference");
            return Err(syntax(message, self.offset(start)));
        }
        Ok(self.reference(groups, start))
    }

    /// The subroutine call `\g<name>`, `\g'name'`, `\g<n>`, `\g<-n>`, or
    /// `\g<+n>` whose `g` was just read.
    fn parse_call(&mut self, start: usize) -> Result<Node, Error> {
        let close = match self.next() {
            Some('<') => '>',
            _ => '\'',
        };
        let from = self.pos;
        while self.peek().is_some_and(|c| c != close) {
            self.pos += 1;
        }
        let target: String = self.chars[from..self.pos].iter().collect();
        if !self.eat(close) {
            return Err(syntax("invalid group name", self.offset(start)));
        }
        if target.is_empty() {
            return Err(syntax("group name is empty", self.offset(start)));
        }
        let relative = target.starts_with(['+', '-']);
        let group = match target.parse::<isize>() {
            Ok(number) if relative && number < 0 && number.unsigned_abs() <= self.groups => {
                self.groups + 1 - number.unsigned_abs()
            }
            Ok(number) if relative && number > 0 => self.groups + number.unsigned_abs(),
            Ok(number) if !relative && number >= 0 => number.unsigned_abs(),
            Ok(_) => {
                let message = format!("undefined group <{target}> reference");
                return Err(syntax(message, self.offset(start)));
            }
            Err(_) => return self.named_call(&target, start),
        };
        if self.highest_call.is_none_or(|(known, _)| group > known) {
            self.highest_call = Some((group, start));
        }
        Ok(Node::Call(group))
    }

    /// The call to the group named `name`, which may be defined after it:
    /// on the first parse, a stand-in until every name is known.
    fn named_call(&mut self, name: &str, start: usize) -> Result<Node, Error> {
        let Some(known) = &self.known_names else {
            self.named_calls = true;
            return Ok(Node::Call(0));
        };
        let mut groups = known.iter().filter(|(known, _)| known == name);
        match (groups.next(), groups.next()) {
            (Some((_, group)), None) => Ok(Node::Call(*group)),
            (Some(_), Some(_)) => {
                let message = format!("multiplex definition name <{name}> call");
                Err(syntax(message, self.offset(start)))
            }
            (None, _) => {
                let message = format!("undefined name <{name}> reference");
                Err(syntax(message, self.offset(start)))
            }
        }
    }

    /// A back-reference to any of `groups`, noted for the check that every
    /// group referred to exists.
    fn reference(&mut self, groups: Vec<usize>, start: usize) -> Node {
        let highest = groups.iter().copied().max().unwrap_or(0);
        if self
            .highest_reference
            .is_none_or(|(known, _)| highest > known)
        {
            self.highest_reference = Some((highest, start));
        }
        Node::BackRef(BackRef {
            casei: self.flags.casei,
            groups: groups.into(),
        })
    }

    // ----------------------------------------------------------------
    // Reading
    // ----------------------------------------------------------------

    /// The flags of an isolated option group at the parser (`(?i)`), and
    /// where the group ends.
    fn isolated_flags(&self) -> Option<(Flags, usize)> {
        self.flags_at(')')
    }

    /// The flags of an option group with a body at the parser (`(?i:`), and
    /// where its body starts.
    fn scoped_flags(&self) -> Option<(Flags, usize)> {
        self.flags_at(':')
    }

    /// The flags of an option group at the parser ending in `close`, with
    /// the flags in effect changed as it says, and where it ends.
    fn flags_at(&self, close: char) -> Option<(Flags, usize)> {
        if self.chars.get(self.pos + 1) != Some(&'?') {
            return None;
        }
        let mut flags = self.flags;
        let mut on = true;
        for (i, &c) in self.chars.iter().enumerate().skip(self.pos + 2) {
            match c {
                'i' => flags.casei = on,
                'm' => flags.dotall = on,
                'x' => flags.extended = on,
                // ASCII-only case applies to the whole pattern, so only
                // where nothing but groups have opened before it.
                'I' if on
                    && self.chars[..self.pos]
                        .chunks(3)
                        .all(|open| open == ['(', '?', ':']) =>
                {
                    flags.ascii_casei = true;
                }
                'D' => flags.ascii_digit = on,
                'P' => flags.ascii_posix = on,
                'S' => flags.ascii_space = on,
                'W' => flags.ascii_word = on,
                'a' => {
                    flags.ascii_digit = on;
                    flags.ascii_posix = on;
                    flags.ascii_space = on;
                    flags.ascii_word = on;
                }
                '-' if on => on = false,
                c if c == close && i > self.pos + 2 => return Some((flags, i + 1)),
                _ => return None,
            }
        }
        None
    }

    /// The interval quantifier opening at `start` (`{n}`, `{n,}`, `{,m}`,
    /// `{n,m}`), as written. `None` when the brace opens no interval and is
    /// a literal.
    fn interval(&self, start: usize) -> Result<Option<Interval>, Error> {
        let digits_from = |at: usize| {
            self.chars[at..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count()
        };
        let number = |from: usize, len: usize| -> Result<Option<u32>, Error> {
            if len == 0 {
                return Ok(None);
            }
            let text: String = self.chars[from..from + len].iter().collect();
            match text.parse::<u32>() {
                Ok(n) if n <= MAX_REPEAT => Ok(Some(n)),
                Ok(_) | Err(_) => Err(syntax(
                    "too big number for repeat range",
                    self.offset(start),
                )),
            }
        };
        let low_len = digits_from(start + 1);
        let mut at = start + 1 + low_len;
        let low = number(start + 1, low_len)?;
        let (min, max) = match self.chars.get(at) {
            Some('}') => match low {
                Some(n) => (n, Some(n)),
                None => return Ok(None),
            },
            Some(',') => {
                let high_len = digits_from(at + 1);
                let high = number(at + 1, high_len)?;
                at += 1 + high_len;
                if self.chars.get(at) != Some(&'}') || (low.is_none() && high.is_none()) {
                    return Ok(None);
                }
                (low.unwrap_or(0), high)
            }
            _ => return Ok(None),
        };
        Ok(Some(Interval {
            end: at + 1,
            max,
            min,
        }))
    }

    /// The character the octal digits at the parser make, at most
    /// `max_digits` of them, read past.
    fn octal(&mut self, max_digits: usize) -> Option<char> {
        let digits = self.chars[self.pos..]
            .iter()
            .take(max_digits)
            .take_while(|c| matches!(c, '0'..='7'))
            .count();
        if digits == 0 {
            return None;
        }
        let text: String = self.chars[self.pos..self.pos + digits].iter().collect();
        self.pos += digits;
        u32::from_str_radix(&text, 8).ok().and_then(char::from_u32)
    }

    /// In extended mode, skips whitespace and `#` comments.
    fn skip_extended(&mut self) {
        while self.flags.extended {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.pos += 1,
                Some('#') => while self.next().is_some_and(|c| c != '\n') {},
                _ => break,
            }
        }
    }

    /// The literal `c`, matching either case where the flags say.
    fn literal(&self, c: char) -> Node {
        match self.flags.casei {
            true => single_or_class(fold(
                ClassUnicode::new([ClassUnicodeRange::new(c, c)]),
                self.flags.ascii_casei,
            )),
            false => Node::Char(c),
        }
    }

    /// The character at the parser.
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// The character at the parser, read past.
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// Whether the character at the parser is `c`, read past if it is.
    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.pos += 1;
        }
        found
    }

    /// The byte offset of character `at` in the pattern.
    fn offset(&self, at: usize) -> usize {
        self.chars[..at.min(self.chars.len())]
            .iter()
            .map(|c| c.len_utf8())
            .sum()
    }
}

// ========================================================================
// Functions
// ========================================================================

/// `pattern` parsed, with `flags` in effect from its start.
pub(crate) fn parse(pattern: &str, flags: Flags) -> Result<Parsed, Error> {
    parse_with(pattern, flags, None)
}

/// `patterns` parsed, each with `flags` in effect from its start, as one
/// alternation tried in their order, with the group that holds each whole
/// pattern: a pattern's group `n` is that group's number plus `n`.
pub(crate) fn parse_set(
    patterns: &[impl AsRef<str>],
    flags: Flags,
) -> Result<(Parsed, Box<[usize]>), Error> {
    let mut alternatives = Vec::with_capacity(patterns.len());
    let mut names = Vec::new();
    let mut bases = Vec::with_capacity(patterns.len());
    let mut groups = 0;
    for pattern in patterns {
        let parsed = parse(pattern.as_ref(), flags)?;
        let base = groups + 1;
        let mut node = parsed.node;
        renumber(&mut node, base);
        alternatives.push(Node::Capture(Capture {
            index: base,
            node: Box::new(node),
        }));
        names.extend(
            parsed
                .names
                .into_iter()
                .map(|(name, index)| (name, base + index)),
        );
        bases.push(base);
        groups = base + parsed.groups;
    }
    // No pattern matches nothing, as an empty class does.
    let node = match alternatives.is_empty() {
        true => Node::Class(ClassUnicode::empty()),
        false => Node::Alt(alternatives),
    };
    Ok((
        Parsed {
            calls: check_calls(&node, groups)?,
            groups,
            names,
            node,
        },
        bases.into(),
    ))
}

/// `pattern` parsed, with `flags` in effect from its start and calls by
/// name referring to `known_names`: when `None` and a call names a group,
/// parsed again once every name is known.
fn parse_with(
    pattern: &str,
    flags: Flags,
    known_names: Option<Vec<(String, usize)>>,
) -> Result<Parsed, Error> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        flags,
        groups: 0,
        highest_call: None,
        highest_reference: None,
        known_names,
        names: Vec::new(),
        named_calls: false,
        pos: 0,
        range_end: false,
    };
    let node = parser.parse_alt()?;
    if parser.pos < parser.chars.len() {
        return Err(syntax(
            "unmatched close parenthesis",
            parser.offset(parser.pos),
        ));
    }
    if let Some((group, at)) = parser.highest_reference
        && group > parser.groups
    {
        return Err(syntax("invalid backref number/name", parser.offset(at)));
    }
    if let Some((group, at)) = parser.highest_call
        && group > parser.groups
    {
        let message = format!("undefined group <{group}> reference");
        return Err(syntax(message, parser.offset(at)));
    }
    if parser.named_calls {
        return parse_with(pattern, flags, Some(parser.names));
    }
    Ok(Parsed {
        calls: check_calls(&node, parser.groups)?,
        groups: parser.groups,
        names: parser.names,
        node,
    })
}

/// Renumbers the groups of `node`, and what refers to them, as a part of a
/// set held by group `base`: group `n` becomes `base + n`, group 0 `base`.
fn renumber(node: &mut Node, base: usize) {
    match node {
        Node::Capture(capture) => {
            capture.index += base;
            renumber(&mut capture.node, base);
        }
        Node::BackRef(reference) => {
            for group in &mut reference.groups {
                *group += base;
            }
        }
        Node::Call(group) => *group += base,
        Node::Alt(parts) | Node::Concat(parts) => {
            for part in parts {
                renumber(part, base);
            }
        }
        Node::Atomic(inner) => renumber(inner, base),
        Node::Conditional(cond) => {
            for group in &mut cond.groups {
                if *group != 0 {
                    *group += base;
                }
            }
            renumber(&mut cond.yes, base);
            renumber(&mut cond.no, base);
        }
        Node::Look(look) => renumber(&mut look.node, base),
        Node::Repeat(repeat) => renumber(&mut repeat.node, base),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => {}
    }
}

/// A linebreak (`\R`): CRLF, or any one line-ending character.
fn linebreak() -> Node {
    let endings = ClassUnicode::new([
        ClassUnicodeRange::new('\n', '\r'),
        ClassUnicodeRange::new('\u{85}', '\u{85}'),
        ClassUnicodeRange::new('\u{2028}', '\u{2029}'),
    ]);
    Node::Atomic(Box::new(Node::Alt(vec![
        Node::Concat(vec![Node::Char('\r'), Node::Char('\n')]),
        Node::Class(endings),
    ])))
}

/// An extended grapheme cluster (`\X`), as Unicode's text segmentation
/// defines one: a CRLF, a control character, or a core, such as a Hangul
/// syllable, a flag, or an emoji sequence, with what attaches to it.
fn grapheme_cluster() -> Node {
    let class = |source: &str| Node::Class(unicode_class(source));
    let gcb = |value: &str| class(&format!(r"\p{{gcb={value}}}"));
    let repeat = |min, node| {
        Node::Repeat(Repeat {
            greedy: true,
            max: None,
            min,
            node: Box::new(node),
        })
    };
    let hangul = Node::Alt(vec![
        Node::Concat(vec![
            repeat(0, gcb("L")),
            Node::Alt(vec![
                repeat(1, gcb("V")),
                Node::Concat(vec![gcb("LV"), repeat(0, gcb("V"))]),
                gcb("LVT"),
            ]),
            repeat(0, gcb("T")),
        ]),
        repeat(1, gcb("L")),
        repeat(1, gcb("T")),
    ]);
    let behind = |negated, node| {
        Node::Look(Look {
            behind: true,
            negated,
            node: Box::new(node),
        })
    };
    // Two regional indicators make a flag, counted from the first of a run:
    // not after an odd number of them, which pair with this one.
    let odd_indicators = Node::Concat(vec![
        behind(true, gcb("RI")),
        repeat(0, Node::Concat(vec![gcb("RI"), gcb("RI")])),
        gcb("RI"),
    ]);
    let flag = Node::Concat(vec![behind(true, odd_indicators), gcb("RI"), gcb("RI")]);
    let pictographic = || class(r"\p{Extended_Pictographic}");
    let joined = || Node::Concat(vec![repeat(0, gcb("Extend")), gcb("ZWJ"), pictographic()]);
    let emoji = Node::Concat(vec![pictographic(), repeat(0, joined())]);
    // A joiner goes on with an emoji before where the cluster starts.
    let emoji_continued = Node::Concat(vec![
        behind(
            false,
            Node::Concat(vec![pictographic(), repeat(0, gcb("Extend"))]),
        ),
        joined(),
        repeat(0, joined()),
    ]);
    let core = Node::Alt(vec![
        hangul,
        flag,
        emoji,
        emoji_continued,
        class(r"[^\p{gcb=Control}\p{gcb=CR}\p{gcb=LF}]"),
    ]);
    Node::Atomic(Box::new(Node::Alt(vec![
        Node::Concat(vec![Node::Char('\r'), Node::Char('\n')]),
        class(r"[\p{gcb=Control}\p{gcb=CR}\p{gcb=LF}]"),
        Node::Concat(vec![
            repeat(0, gcb("Prepend")),
            core,
            repeat(0, class(r"[\p{gcb=Extend}\p{gcb=ZWJ}\p{gcb=SpacingMark}]")),
        ]),
    ])))
}

/// `class` as a node: a class of one character as that character.
fn single_or_class(class: ClassUnicode) -> Node {
    match class.ranges() {
        [range] if range.start() == range.end() => Node::Char(range.start()),
        _ => Node::Class(class),
    }
}

/// A syntax error at byte `offset` of the pattern.
fn syntax(message: impl Into<Box<str>>, offset: usize) -> Error {
    Error::Syntax(PatternError::new(message, offset))
}

/// An error for a construct reox does not support yet.
fn unsupported(message: impl Into<Box<str>>, offset: usize) -> Error {
    Error::Unsupported(PatternError::new(message, offset))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pattern: &str) -> String {
        format!(
            "{:?}",
            parse(pattern, Flags::default()).expect("parses").node
        )
    }

    fn error(pattern: &str) -> String {
        match parse(pattern, Flags::default()) {
            Err(Error::Syntax(error)) => error.message().to_owned(),
            other => panic!("{pattern} parsed as {other:?}"),
        }
    }

    #[test]
    fn test_isolated_flags_take_in_the_later_alternatives() {
        assert_eq!(tree("a(?i)b|c"), tree("a(?i:b|c)"));
        assert_eq!(tree("(x(?i)b|c)d"), tree("(x(?i:b|c))d"));
    }

    #[test]
    fn test_intervals_read_as_oniguruma_reads_them() {
        assert_eq!(tree("a{,2}"), tree("a{0,2}"));
        assert_eq!(
            tree("a{2}+"),
            tree("(?:a{2})+"),
            "+ after an interval repeats"
        );
        assert_eq!(tree("a{2}?"), tree("(?:a{2})?"), "? after an exact count");
        assert!(tree("a{1,2}?").contains("greedy: false"), "lazy");
        assert_eq!(tree("a{x}"), tree(r"a\{x\}"), "no interval");
        assert_eq!(tree("a{3,2}"), tree("(?>a{2,3})"), "reversed is possessive");
        assert_eq!(error("a{100001}"), "too big number for repeat range");
    }

    #[test]
    fn test_numbers_refer_to_groups_or_are_octal() {
        assert!(tree(r"(a)\1").contains("BackRef"));
        assert_eq!(tree(r"(a)\12"), tree("(a)\n"), "octal past the groups");
        assert_eq!(error(r"\8"), "invalid backref number/name");
        assert_eq!(error(r"(a)\2"), "invalid backref number/name");
        assert!(
            tree(r"\1(a)").contains("BackRef"),
            "a single digit refers forward"
        );
    }

    #[test]
    fn test_named_references_need_the_name_defined_first() {
        assert!(tree(r"(?<n>a)\k<n>").contains("BackRef"));
        assert_eq!(error(r"\k<n>(?<n>a)"), "undefined name <n> reference");
        assert!(tree(r"(a)(b)\k<-1>").contains("groups: [2]"), "relative");
    }

    #[test]
    fn test_invalid_patterns_are_rejected() {
        assert_eq!(error("*a"), "target of repeat operator is not specified");
        assert_eq!(error("(a"), "end pattern with unmatched parenthesis");
        assert_eq!(error("a)"), "unmatched close parenthesis");
        assert_eq!(error("(?Q)"), "undefined group option");
    }

    #[test]
    fn test_code_point_limits_depend_on_where_the_code_point_is() {
        assert_eq!(error(r"\x{140000}"), "invalid code point value");
        assert!(
            parse(r"[\x{140000}]", Flags::default()).is_ok(),
            "in a class"
        );
        assert_eq!(error(r"[\x{200000}]"), "invalid code point value");
        assert!(
            parse(r"[\x{7f}-\x{7fffffff}]", Flags::default()).is_ok(),
            "a range's end may name any"
        );
    }

    #[test]
    fn test_a_list_range_ends_no_class_range() {
        assert_eq!(error(r"[\d-\x{61-63}]"), "invalid code point value");
        assert!(
            parse(r"[\x{61-63}-\x{70-72}]", Flags::default()).is_ok(),
            "after a list range, the dash is a literal"
        );
    }

    #[test]
    fn test_an_ascii_case_group_with_a_body_ends_the_pattern() {
        assert_eq!(error("(?I:a)|b"), "invalid group option");
        assert!(
            parse("(?:(?I:a))", Flags::default()).is_ok(),
            "closing groups"
        );
        assert_eq!(
            error("(?I:a"),
            "end pattern with unmatched parenthesis",
            "the body's own error first"
        );
    }
}
