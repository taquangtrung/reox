//! Parsed patterns compiled to the instructions the backtracking machine
//! runs: cheap to build, with no automata, so a pattern costs little until
//! it is searched with.

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

use crate::accel::{StartBytes, first_chars, last_chars};
use crate::error::Error;
use crate::syntax::{Assertion, Calls, Capture, Look, Node, Parsed, Repeat};

use super::program::{
    AltInst, AltTarget, Anchor, CharClass, CharLoop, CharMatcher, CondJump, GroupEnd, Inst,
    LookInst, Program, Progress, Split,
};

// ========================================================================
// Constants
// ========================================================================

/// How many alternatives an alternation has before it skips those that
/// cannot start at the position, rather than trying each in turn.
const ALT_DISPATCH_MIN: usize = 3;

// ========================================================================
// Data Structures
// ========================================================================

/// The state of compiling one pattern.
struct Compiler<'p> {
    /// How many back-references refer to each group, by number.
    back_references: Vec<usize>,
    /// Which groups are called, how much each takes.
    calls: &'p Calls,
    /// Each call so far, and the group it calls, to point at the group once
    /// it is compiled.
    call_sites: Vec<(usize, usize)>,
    /// How many called groups enclose what is compiled: a group inside one
    /// may run again before it ends, in a call nested in it.
    called_depth: usize,
    /// How many loops that may match nothing enclose what is compiled: the
    /// places inside one depend on where its iteration started, so are not
    /// memoized.
    empty_loops: usize,
    /// Where each called group's instructions start, once compiled.
    group_starts: Vec<Option<usize>>,
    /// The instructions so far.
    insts: Vec<Inst>,
    /// How many position registers its loops use so far.
    registers: usize,
    /// The most instructions the program may hold.
    size_limit: usize,
    /// How many places it may memoize so far: splits and character loops.
    memo_points: usize,
}

// ========================================================================
// Compiler Implementations
// ========================================================================

impl Compiler<'_> {
    /// Appends `inst`, returning where it is.
    fn emit(&mut self, inst: Inst) -> Result<usize, Error> {
        if self.insts.len() >= self.size_limit {
            return Err(Error::TooBig);
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    /// A new position register's number.
    fn register(&mut self) -> usize {
        self.registers += 1;
        self.registers - 1
    }

    /// Where the next instruction goes.
    fn next(&self) -> usize {
        self.insts.len()
    }

    /// A new memo point's number; `None` inside a loop that may match
    /// nothing.
    fn memo_point(&mut self) -> Option<usize> {
        if self.empty_loops > 0 {
            return None;
        }
        self.memo_points += 1;
        Some(self.memo_points - 1)
    }

    /// A split trying `first`, then `second`, numbered.
    fn split(&mut self, first: usize, second: usize) -> Split {
        Split {
            first,
            id: self.memo_point(),
            second,
        }
    }

    /// A split trying `body` first when `greedy`, else `exit` first.
    fn choice(&mut self, greedy: bool, body: usize, exit: usize) -> Split {
        match greedy {
            true => self.split(body, exit),
            false => self.split(exit, body),
        }
    }

    /// Appends the instructions matching `node`.
    fn compile(&mut self, node: &Node) -> Result<(), Error> {
        match node {
            Node::Alt(alternatives) => self.compile_alt(alternatives)?,
            Node::Any => {
                self.emit(Inst::Any)?;
            }
            Node::AnyButNewline => {
                self.emit(Inst::AnyButNewline)?;
            }
            Node::Assert(assertion) => {
                self.emit(Inst::Assert(*assertion))?;
            }
            Node::Atomic(inner) if self.compile_char_loop(inner, true)? => {}
            Node::Atomic(inner) => {
                let atomic = self.emit(Inst::Atomic(0))?;
                self.compile(inner)?;
                self.emit(Inst::End)?;
                self.insts[atomic] = Inst::Atomic(self.next());
            }
            Node::BackRef(reference) => {
                self.emit(Inst::BackRef(reference.clone()))?;
            }
            Node::Call(group) => {
                let at = self.emit(Inst::Call(0))?;
                self.call_sites.push((at, *group));
            }
            Node::Capture(capture) if self.calls.called[capture.index] => {
                self.compile_called(capture.index, |compiler| compiler.compile(&capture.node))?;
            }
            Node::Capture(capture) if self.called_depth > 0 => {
                // Where it started kept per depth of calls, so a nested run
                // of it does not move where this one started.
                let register = self.register();
                self.emit(Inst::Mark(register))?;
                self.compile(&capture.node)?;
                self.emit(Inst::SaveGroup(GroupEnd {
                    index: capture.index,
                    register,
                }))?;
            }
            Node::Capture(capture) => {
                self.emit(Inst::Save(capture.index * 2))?;
                self.compile(&capture.node)?;
                self.emit(Inst::Save(capture.index * 2 + 1))?;
            }
            Node::Char(c) => {
                self.emit(Inst::Char(*c))?;
            }
            Node::Class(class) => {
                self.emit(Inst::Class(CharClass::shared(class)))?;
            }
            Node::Concat(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    let never_gives_back = parts
                        .get(i + 1)
                        .is_some_and(|next| never_gives_back(part, next));
                    if !(never_gives_back && self.compile_char_loop(part, true)?) {
                        self.compile(part)?;
                    }
                }
            }
            Node::Conditional(cond) => {
                let branch = self.emit(Inst::CondJump(CondJump {
                    groups: cond.groups.clone(),
                    no: 0,
                    yes: 0,
                }))?;
                let yes = self.next();
                self.compile(&cond.yes)?;
                let end_yes = self.emit(Inst::Jump(0))?;
                let no = self.next();
                self.compile(&cond.no)?;
                let end = self.next();
                self.insts[branch] = Inst::CondJump(CondJump {
                    groups: cond.groups.clone(),
                    no,
                    yes,
                });
                self.insts[end_yes] = Inst::Jump(end);
            }
            Node::Empty => {}
            Node::Keep => {
                self.emit(Inst::Keep)?;
            }
            Node::Look(look) => self.compile_look(look)?,
            Node::Repeat(repeat) => self.compile_repeat(repeat)?,
        }
        Ok(())
    }

    /// Appends group `index`, called, as `compile_body` appends its part: a
    /// call to it where it stands, and the group after, run only by calls.
    /// Group 0, the whole pattern, captures nothing.
    fn compile_called(
        &mut self,
        index: usize,
        compile_body: impl FnOnce(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let call = self.emit(Inst::Call(0))?;
        let jump = self.emit(Inst::Jump(0))?;
        let start = self.compile_subroutine(index, compile_body)?;
        self.insts[call] = Inst::Call(start);
        self.insts[jump] = Inst::Jump(self.next());
        Ok(())
    }

    /// Appends group `index` as `compile_body` appends its part, run only
    /// by calls, returning where it starts.
    fn compile_subroutine(
        &mut self,
        index: usize,
        compile_body: impl FnOnce(&mut Self) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        let start = self.next();
        let register = self.register();
        self.emit(Inst::Mark(register))?;
        self.called_depth += 1;
        let body = compile_body(self);
        self.called_depth -= 1;
        body?;
        if index > 0 {
            self.emit(Inst::SaveGroup(GroupEnd { index, register }))?;
        }
        self.emit(Inst::Return)?;
        self.group_starts[index] = Some(start);
        Ok(start)
    }

    /// Appends alternatives, each tried when those before it fail.
    fn compile_alt(&mut self, alternatives: &[Node]) -> Result<(), Error> {
        if alternatives.is_empty() {
            self.emit(Inst::Class(CharClass::shared(&ClassUnicode::empty())))?;
            return Ok(());
        }
        if alternatives.len() >= ALT_DISPATCH_MIN {
            return self.compile_dispatch(alternatives);
        }
        let mut jumps = Vec::new();
        for (i, alternative) in alternatives.iter().enumerate() {
            let is_last = i + 1 == alternatives.len();
            let split = match is_last {
                true => None,
                false => Some(self.emit(Inst::Jump(0))?),
            };
            self.compile(alternative)?;
            if let Some(split) = split {
                jumps.push(self.emit(Inst::Jump(0))?);
                let second = self.next();
                self.insts[split] = Inst::Split(self.split(split + 1, second));
            }
        }
        let end = self.next();
        for jump in jumps {
            self.insts[jump] = Inst::Jump(end);
        }
        Ok(())
    }

    /// Appends alternatives behind one instruction that skips those that
    /// cannot start with the character at the position.
    fn compile_dispatch(&mut self, alternatives: &[Node]) -> Result<(), Error> {
        let at = self.emit(Inst::Jump(0))?;
        let mut targets = Vec::with_capacity(alternatives.len());
        let mut jumps = Vec::new();
        for (i, alternative) in alternatives.iter().enumerate() {
            targets.push(AltTarget {
                first: first_chars(alternative).map(|chars| CharClass::shared(&chars)),
                pc: self.next(),
            });
            self.compile(alternative)?;
            if i + 1 < alternatives.len() {
                jumps.push(self.emit(Inst::Jump(0))?);
            }
        }
        let end = self.next();
        for jump in jumps {
            self.insts[jump] = Inst::Jump(end);
        }
        self.insts[at] = Inst::Alt(Box::new(AltInst {
            id: self.memo_point(),
            targets: targets.into(),
        }));
        Ok(())
    }

    /// Appends a lookaround.
    fn compile_look(&mut self, look: &Look) -> Result<(), Error> {
        let at = self.emit(Inst::Jump(0))?;
        self.compile(&look.node)?;
        self.emit(Inst::End)?;
        let last = match look.behind {
            true => last_chars(&look.node).map(|chars| CharClass::shared(&chars)),
            false => None,
        };
        let min_len = |node: &Node| node.min_len_with(&self.calls.min_lens);
        let max_len = |node: &Node| node.max_len_with(&self.calls.max_lens);
        let lengths = match (look.behind, look.node.as_ref()) {
            (true, Node::Alt(alternatives))
                if alternatives
                    .iter()
                    .all(|alt| max_len(alt) == Some(min_len(alt))) =>
            {
                let mut lengths: Vec<usize> = Vec::new();
                for alternative in alternatives {
                    if !lengths.contains(&min_len(alternative)) {
                        lengths.push(min_len(alternative));
                    }
                }
                Some(lengths.into())
            }
            _ => None,
        };
        self.insts[at] = Inst::Look(Box::new(LookInst {
            behind: look.behind,
            last,
            lengths,
            max_len: max_len(&look.node),
            min_len: min_len(&look.node),
            negated: look.negated,
            next: self.next(),
        }));
        Ok(())
    }

    /// Appends a repetition: its required copies, then its optional ones,
    /// or a loop for no limit.
    fn compile_repeat(&mut self, repeat: &Repeat) -> Result<(), Error> {
        if repeat.greedy && self.compile_char_loop_of(repeat, false)? {
            return Ok(());
        }
        for _ in 0..repeat.min {
            self.compile(&repeat.node)?;
        }
        let Some(max) = repeat.max else {
            return self.compile_loop(repeat);
        };
        let mut splits = Vec::new();
        for _ in repeat.min..max {
            splits.push(self.emit(Inst::Jump(0))?);
            self.compile(&repeat.node)?;
        }
        let end = self.next();
        for split in splits {
            self.insts[split] = Inst::Split(self.choice(repeat.greedy, split + 1, end));
        }
        Ok(())
    }

    /// Appends `node` as a character loop, if it is a greedy repetition of
    /// one character; whether it was.
    fn compile_char_loop(&mut self, node: &Node, possessive: bool) -> Result<bool, Error> {
        match node {
            Node::Repeat(repeat) if repeat.greedy => self.compile_char_loop_of(repeat, possessive),
            Node::Repeat(_)
            | Node::Alt(_)
            | Node::Any
            | Node::AnyButNewline
            | Node::Assert(_)
            | Node::Atomic(_)
            | Node::BackRef(_)
            | Node::Call(_)
            | Node::Capture(_)
            | Node::Char(_)
            | Node::Class(_)
            | Node::Concat(_)
            | Node::Conditional(_)
            | Node::Empty
            | Node::Keep
            | Node::Look(_) => Ok(false),
        }
    }

    /// Appends `repeat` as a character loop, if it repeats one character;
    /// whether it did.
    fn compile_char_loop_of(&mut self, repeat: &Repeat, possessive: bool) -> Result<bool, Error> {
        let Some(matcher) = char_matcher(&repeat.node) else {
            return Ok(false);
        };
        let id = self.memo_point();
        self.emit(Inst::CharLoop(Box::new(CharLoop {
            id,
            matcher,
            max: repeat.max,
            min: repeat.min,
            possessive,
        })))?;
        Ok(true)
    }

    /// The groups a loop over `body` that may match nothing watches. As in
    /// Oniguruma, an iteration that matched nothing ends the loop unless it
    /// changed one of them. They are the groups a back-reference outside the
    /// loop refers to, for which this is the innermost such loop.
    fn watched_groups(&self, body: &Node) -> Box<[usize]> {
        let mut owned = Vec::new();
        owned_captures(body, &mut owned);
        let mut inside = vec![0; self.back_references.len()];
        count_back_references(body, &mut inside);
        owned
            .into_iter()
            .filter(|&group| self.back_references[group] > inside[group])
            .collect()
    }

    /// Appends a loop with no limit. One whose body may match nothing ends
    /// as soon as an iteration matches nothing, as Oniguruma's does, unless
    /// it changed a group it watches.
    fn compile_loop(&mut self, repeat: &Repeat) -> Result<(), Error> {
        let split = self.emit(Inst::Jump(0))?;
        let may_be_empty = repeat.node.min_len() == 0;
        let mut watched = Box::default();
        let progress = match may_be_empty {
            true => {
                let register = self.register();
                self.emit(Inst::Mark(register))?;
                watched = self.watched_groups(&repeat.node);
                self.empty_loops += 1;
                let body = self.compile(&repeat.node);
                self.empty_loops -= 1;
                body?;
                Some((self.emit(Inst::Jump(0))?, register))
            }
            false => {
                self.compile(&repeat.node)?;
                self.emit(Inst::Jump(split))?;
                None
            }
        };
        let end = self.next();
        self.insts[split] = Inst::Split(self.choice(repeat.greedy, split + 1, end));
        if let Some((at, register)) = progress {
            self.insts[at] = Inst::Progress(Box::new(Progress {
                exit: end,
                register,
                start: split,
                watched,
            }));
        }
        Ok(())
    }
}

// ========================================================================
// Functions
// ========================================================================

/// `parsed` compiled, holding at most `size_limit` instructions.
pub(crate) fn compile(parsed: &Parsed, size_limit: usize) -> Result<Program, Error> {
    let mut back_references = vec![0; parsed.groups + 1];
    count_back_references(&parsed.node, &mut back_references);
    let mut compiler = Compiler {
        back_references,
        call_sites: Vec::new(),
        called_depth: 0,
        calls: &parsed.calls,
        empty_loops: 0,
        group_starts: vec![None; parsed.calls.called.len()],
        insts: Vec::new(),
        registers: 0,
        size_limit,
        memo_points: 0,
    };
    match parsed.calls.called[0] {
        true => compiler.compile_called(0, |compiler| compiler.compile(&parsed.node))?,
        false => compiler.compile(&parsed.node)?,
    }
    compiler.emit(Inst::Match)?;
    // A called group never reached where it stands, as one defined to be
    // called (`(?<name>...){0}`), after the match.
    for index in 1..parsed.calls.called.len() {
        if parsed.calls.called[index] && compiler.group_starts[index].is_none() {
            let capture = find_capture(&parsed.node, index).expect("every group is in the tree");
            compiler.compile_subroutine(index, |compiler| compiler.compile(&capture.node))?;
        }
    }
    for &(at, group) in &compiler.call_sites {
        let start = compiler.group_starts[group].expect("a called group is compiled");
        compiler.insts[at] = Inst::Call(start);
    }
    let referenced = (0..compiler.back_references.len())
        .filter(|&group| compiler.back_references[group] > 0)
        .collect();
    Ok(Program {
        anchor: anchor(&parsed.node),
        insts: compiler.insts,
        referenced,
        registers: compiler.registers,
        slots: (parsed.groups + 1) * 2,
        memo_points: compiler.memo_points,
        start_bytes: StartBytes::of(&parsed.node),
    })
}

/// Adds to `counts` how many back-references in `node` refer to each group.
fn count_back_references(node: &Node, counts: &mut [usize]) {
    match node {
        Node::BackRef(reference) => {
            for &group in &reference.groups {
                counts[group] += 1;
            }
        }
        Node::Alt(parts) | Node::Concat(parts) => {
            for part in parts {
                count_back_references(part, counts);
            }
        }
        Node::Atomic(inner) => count_back_references(inner, counts),
        Node::Capture(capture) => count_back_references(&capture.node, counts),
        Node::Conditional(cond) => {
            for &group in &cond.groups {
                counts[group] += 1;
            }
            count_back_references(&cond.yes, counts);
            count_back_references(&cond.no, counts);
        }
        Node::Look(look) => count_back_references(&look.node, counts),
        Node::Repeat(repeat) => count_back_references(&repeat.node, counts),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => {}
    }
}

/// Adds to `owned` the groups in `node` outside any loop in it that may
/// match nothing, and outside lookarounds.
fn owned_captures(node: &Node, owned: &mut Vec<usize>) {
    match node {
        Node::Capture(capture) => {
            owned.push(capture.index);
            owned_captures(&capture.node, owned);
        }
        Node::Alt(parts) | Node::Concat(parts) => {
            for part in parts {
                owned_captures(part, owned);
            }
        }
        Node::Atomic(inner) => owned_captures(inner, owned),
        Node::Repeat(repeat) if repeat.max.is_none() && repeat.node.min_len() == 0 => {}
        Node::Repeat(repeat) => owned_captures(&repeat.node, owned),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Conditional(_)
        | Node::Empty
        | Node::Keep
        | Node::Look(_) => {}
    }
}

/// The capturing group numbered `index` in `node`.
fn find_capture(node: &Node, index: usize) -> Option<&Capture> {
    match node {
        Node::Capture(capture) if capture.index == index => Some(capture),
        Node::Capture(capture) => find_capture(&capture.node, index),
        Node::Alt(parts) | Node::Concat(parts) => {
            parts.iter().find_map(|part| find_capture(part, index))
        }
        Node::Atomic(inner) => find_capture(inner, index),
        Node::Conditional(cond) => {
            find_capture(&cond.yes, index).or_else(|| find_capture(&cond.no, index))
        }
        Node::Look(look) => find_capture(&look.node, index),
        Node::Repeat(repeat) => find_capture(&repeat.node, index),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => None,
    }
}

/// The matcher of `node`, if it matches exactly one character.
fn char_matcher(node: &Node) -> Option<CharMatcher> {
    Some(match node {
        Node::Any => CharMatcher::Any,
        Node::AnyButNewline => CharMatcher::AnyButNewline,
        Node::Char(c) => CharMatcher::Char(*c),
        Node::Class(class) => CharMatcher::Class(CharClass::shared(class)),
        Node::Alt(_)
        | Node::Assert(_)
        | Node::Atomic(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Capture(_)
        | Node::Concat(_)
        | Node::Conditional(_)
        | Node::Empty
        | Node::Keep
        | Node::Look(_)
        | Node::Repeat(_) => return None,
    })
}

/// The characters `node` matches, if it matches exactly one.
fn char_set(node: &Node) -> Option<ClassUnicode> {
    let all = ClassUnicodeRange::new('\0', char::MAX);
    Some(match node {
        Node::Any => ClassUnicode::new([all]),
        Node::AnyButNewline => {
            let mut class = ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]);
            class.negate();
            class
        }
        Node::Char(c) => ClassUnicode::new([ClassUnicodeRange::new(*c, *c)]),
        Node::Class(class) => class.clone(),
        Node::Alt(_)
        | Node::Assert(_)
        | Node::Atomic(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Capture(_)
        | Node::Concat(_)
        | Node::Conditional(_)
        | Node::Empty
        | Node::Keep
        | Node::Look(_)
        | Node::Repeat(_) => return None,
    })
}

/// Whether `part`, a greedy loop over one kind of character, never needs to
/// give characters back when `next` follows it: every match of `next`
/// starts with a character the loop does not take, so a shorter loop would
/// leave `next` facing one it cannot start with.
fn never_gives_back(part: &Node, next: &Node) -> bool {
    let Node::Repeat(repeat) = part else {
        return false;
    };
    if !repeat.greedy {
        return false;
    }
    let (Some(taken), Some(mut first)) = (char_set(&repeat.node), first_chars(next)) else {
        return false;
    };
    first.intersect(&taken);
    first.ranges().is_empty()
}

/// For a part of a concatenation, the anchor it sets, `Floating` once it
/// may consume text; `None` for a zero-width part that sets none.
fn anchor_or_width(part: &Node) -> Option<Anchor> {
    match part {
        Node::Assert(
            Assertion::LineEnd
            | Assertion::NotWordBoundary
            | Assertion::WordBoundary
            | Assertion::AsciiNotWordBoundary
            | Assertion::AsciiWordBoundary,
        )
        | Node::Assert(Assertion::TextEnd | Assertion::TextEndNewline)
        | Node::Look(_)
        | Node::Keep
        | Node::Empty => None,
        Node::Alt(_)
        | Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::Atomic(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Capture(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Concat(_)
        | Node::Conditional(_)
        | Node::Repeat(_) => Some(anchor(part)),
    }
}

/// Where every match of `node` must start, if one place is known.
fn anchor(node: &Node) -> Anchor {
    match node {
        Node::Assert(Assertion::TextStart) => Anchor::TextStart,
        Node::Assert(Assertion::LineStart) => Anchor::LineStart,
        Node::Assert(Assertion::SearchStart) => Anchor::SearchStart,
        // Zero-width parts before an anchor (`(?<=x)\G`) do not move it.
        Node::Concat(parts) => parts
            .iter()
            .map(anchor_or_width)
            .find_map(|found| found)
            .unwrap_or(Anchor::Floating),
        Node::Capture(capture) => anchor(&capture.node),
        Node::Atomic(inner) => anchor(inner),
        Node::Alt(alternatives) => {
            let anchors: Vec<Anchor> = alternatives.iter().map(anchor).collect();
            match anchors.split_first() {
                Some((first, rest)) if rest.iter().all(|other| other == first) => *first,
                Some(_) | None => Anchor::Floating,
            }
        }
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Conditional(_)
        | Node::Empty
        | Node::Keep
        | Node::Look(_)
        | Node::Repeat(_) => Anchor::Floating,
    }
}
