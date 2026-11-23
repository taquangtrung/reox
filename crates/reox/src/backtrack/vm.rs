//! The backtracking machine: runs a compiled pattern at each place a match
//! may start, trying alternatives in order, its backtracking bounded so a
//! runaway pattern gives up instead of stalling.

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::accel::{AutomataCache, Filter};
use crate::error::Error;
use crate::regex::SearchOptions;
use crate::syntax::{Assertion, BackRef, Posix};

use super::program::{AltTarget, Anchor, CharClass, CharLoop, Inst, LookInst, Program};

// ========================================================================
// Constants
// ========================================================================

/// How many times a match attempt backtracks before the search starts
/// memoizing, for the rest of it: most
/// searches never get there, and so never pay for the memo.
const MEMO_AFTER: usize = 256;

/// The most places a memo may cover, one bit each.
const MEMO_MAX_BITS: usize = 1 << 24;

/// Bits in a memo word.
const WORD_BITS: usize = 64;

/// The most places a memo keyed by groups' spans holds: past it, no more
/// are noted.
const MEMO_MAX_KEYS: usize = 1 << 20;

/// How deep calls nest before a deeper one fails, as in Oniguruma: a
/// called group counts once where it stands, and once per call.
const CALL_DEPTH_LIMIT: usize = 20;

// ========================================================================
// Data Structures
// ========================================================================

/// Reusable search state, so a search allocates nothing once warm.
#[derive(Clone, Debug, Default)]
pub struct Cache {
    /// The automata's search state.
    pub(crate) automata: AutomataCache,
    /// Where each call running returns to, the innermost last.
    calls: Vec<usize>,
    /// The backtracking stack: where to resume, and what to undo on the way
    /// back.
    frames: Vec<Frame>,
    /// Where each loop's current iteration started, and each called group's
    /// match: a set of registers per depth of calls, so a call nested in
    /// the same group keeps its own.
    registers: Vec<Mark>,
    /// The places memoized, one bit per split and position.
    visited: Vec<u64>,
    /// The places memoized for a pattern with back-references, each with
    /// what the groups they refer to held there.
    visited_keys: HashSet<MemoKey>,
}

/// A place memoized for a pattern with back-references: the memo point,
/// the position, and the slots of the groups they refer to.
type MemoKey = (usize, usize, Box<[Option<usize>]>);

/// How a search memoizes the places it failed from.
#[derive(Clone, Copy, Debug)]
enum Memo {
    /// One bit per memo point and position, so many positions apart.
    Bits(usize),
    /// A set of places, each keyed by what the groups back-references refer
    /// to held: what follows a place depends on them.
    Keyed,
    /// Not yet, or not at all.
    Off,
}

/// An entry of the backtracking stack.
#[derive(Clone, Debug)]
enum Frame {
    /// A place to resume from when what follows fails.
    Branch(Branch),
    /// A call to take back when backtracking past it.
    Called,
    /// A register's value to restore when backtracking past it.
    Register(Restore<Mark>),
    /// A return to take back when backtracking past it: where it went.
    Returned(usize),
    /// A character loop that may give back a character.
    Shrink(Shrink),
    /// A capture slot's value to restore when backtracking past it.
    Slot(Restore<Option<usize>>),
}

/// A place to resume from.
#[derive(Clone, Debug)]
struct Branch {
    /// The instruction to resume at.
    pc: usize,
    /// Where in the text to resume, in bytes.
    pos: usize,
}

/// Where a loop's iteration started.
#[derive(Clone, Copy, Debug, Default)]
struct Mark {
    /// How deep the backtracking stack was.
    frames: usize,
    /// Where in the text it started, in bytes.
    pos: usize,
}

/// A character loop that has taken more than it must.
#[derive(Clone, Debug)]
struct Shrink {
    /// The loop's memo point.
    id: Option<usize>,
    /// Whether places it resumes at may be memoized.
    memo: bool,
    /// Where it ends once it has taken the fewest it must.
    min: usize,
    /// The instruction after it.
    pc: usize,
    /// Where it ends now.
    pos: usize,
}

/// A value to restore.
#[derive(Clone, Debug)]
struct Restore<T> {
    /// Which register or slot.
    index: usize,
    /// What it held before the change being undone.
    value: T,
}

/// Where a search starts and tries, and how.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds<'a> {
    /// The most times one match attempt may backtrack.
    pub(crate) backtrack_limit: usize,
    /// Automata that say where a match may start, tried only there; one
    /// search over every candidate, so what the memo learns at one carries
    /// to the next.
    pub(crate) filter: Option<Filter<'a>>,
    /// Where match attempts begin.
    pub(crate) from: usize,
    /// How the search treats where it starts.
    pub(crate) options: SearchOptions,
    /// Where the search started, where `\G` holds.
    pub(crate) start: usize,
}

/// One search.
struct Machine<'s> {
    /// How many times it has backtracked in the current match attempt.
    backtracks: usize,
    /// The most times one match attempt may backtrack, as Oniguruma limits
    /// each attempt rather than the whole search.
    backtrack_limit: usize,
    /// Buffers kept between searches, so a warm search allocates nothing.
    cache: &'s mut Cache,
    /// The text searched.
    haystack: &'s str,
    /// How far characters are read: the text's end, or in a lookbehind's
    /// part the position it looks behind from, as Oniguruma reads them.
    limit: usize,
    /// How it memoizes, once it does.
    memo: Memo,
    /// How the search treats where it starts.
    options: SearchOptions,
    /// The compiled pattern being run.
    program: &'s Program,
    /// Where the search started, where `\G` holds.
    search_start: usize,
    /// Each group's start and end, two slots per group, the match first.
    slots: &'s mut [Option<usize>],
}

// ========================================================================
// Machine Implementations
// ========================================================================

impl Machine<'_> {
    /// Runs from instruction `pc` at `pos` to the end of the pattern, or of
    /// the atomic or lookaround part being run, and returns where it ended;
    /// `None` when nothing matched.
    ///
    /// - It backtracks no further than `base` on the stack.
    /// - With `target`, only an end at that position counts.
    /// - `top` marks the run of the whole pattern, the only one whose places
    ///   may be memoized: a place reached twice failed the first time, or
    ///   the search would have ended.
    fn run(
        &mut self,
        mut pc: usize,
        mut pos: usize,
        base: usize,
        target: Option<usize>,
        top: bool,
    ) -> Result<Option<usize>, Error> {
        loop {
            let matched = match &self.program.insts[pc] {
                Inst::Match => return Ok(Some(pos)),
                Inst::End if target.is_none_or(|target| target == pos) => return Ok(Some(pos)),
                Inst::End => false,
                Inst::Any => self.step(&mut pos, |_| true),
                Inst::AnyButNewline => self.step(&mut pos, |c| c != '\n'),
                Inst::Char(expected) => self.step(&mut pos, |c| c == *expected),
                Inst::CharLoop(char_loop) => match self.take(char_loop, pos) {
                    Some((min, end)) => {
                        if !char_loop.possessive && end > min {
                            self.cache.frames.push(Frame::Shrink(Shrink {
                                id: char_loop.id,
                                memo: top,
                                min,
                                pc: pc + 1,
                                pos: end,
                            }));
                        }
                        pos = end;
                        !(top && self.revisits(char_loop.id, end))
                    }
                    None => false,
                },
                Inst::Class(class) => self.step(&mut pos, |c| class.contains(c)),
                Inst::Assert(assertion) => self.holds(*assertion, pos),
                Inst::Alt(alt) if top && self.revisits(alt.id, pos) => false,
                Inst::Alt(alt) => {
                    let c = self.char_within(pos);
                    let viable = |target: &&AltTarget| match (&target.first, c) {
                        (None, _) => true,
                        (Some(first), Some(c)) => first.contains(c),
                        (Some(_), None) => false,
                    };
                    let mut viable_targets = alt.targets.iter().filter(viable);
                    match viable_targets.next() {
                        Some(first) => {
                            let base = self.cache.frames.len();
                            for target in viable_targets {
                                self.cache
                                    .frames
                                    .push(Frame::Branch(Branch { pc: target.pc, pos }));
                            }
                            // Tried in order: the last pushed is popped first.
                            self.cache.frames[base..].reverse();
                            pc = first.pc;
                            continue;
                        }
                        None => false,
                    }
                }
                Inst::BackRef(reference) => match self.back_reference(reference, pos) {
                    Some(end) => {
                        pos = end;
                        true
                    }
                    None => false,
                },
                Inst::Jump(to) => {
                    pc = *to;
                    continue;
                }
                Inst::Split(split) if top && self.revisits(split.id, pos) => false,
                Inst::Split(split) => {
                    self.cache.frames.push(Frame::Branch(Branch {
                        pc: split.second,
                        pos,
                    }));
                    pc = split.first;
                    continue;
                }
                Inst::Save(slot) => {
                    self.save(*slot, pos);
                    true
                }
                Inst::SaveGroup(group) => {
                    let start = self.cache.registers[self.register(group.register)].pos;
                    self.save(group.index * 2, start);
                    self.save(group.index * 2 + 1, pos);
                    true
                }
                Inst::Call(_) if self.cache.calls.len() == CALL_DEPTH_LIMIT => false,
                Inst::Call(target) => {
                    self.cache.calls.push(pc + 1);
                    let registers = (self.cache.calls.len() + 1) * self.program.registers;
                    if self.cache.registers.len() < registers {
                        self.cache.registers.resize(registers, Mark::default());
                    }
                    self.cache.frames.push(Frame::Called);
                    pc = *target;
                    continue;
                }
                Inst::Return => {
                    let to = self.cache.calls.pop().expect("a call to return from");
                    self.cache.frames.push(Frame::Returned(to));
                    pc = to;
                    continue;
                }
                Inst::Keep => {
                    self.save(0, pos);
                    true
                }
                Inst::Mark(register) => {
                    let index = self.register(*register);
                    let value = self.cache.registers[index];
                    self.cache
                        .frames
                        .push(Frame::Register(Restore { index, value }));
                    self.cache.registers[index] = Mark {
                        frames: self.cache.frames.len(),
                        pos,
                    };
                    true
                }
                Inst::Progress(progress) => {
                    let mark = self.cache.registers[self.register(progress.register)];
                    // An iteration that matched nothing but set a group it
                    // watches to something new still counts, as in Oniguruma.
                    let progressed = mark.pos != pos
                        || (!progress.watched.is_empty()
                            && self.changed_groups(mark.frames, &progress.watched));
                    pc = match progressed {
                        true => progress.start,
                        false => progress.exit,
                    };
                    continue;
                }
                Inst::Atomic(next) => {
                    let base = self.cache.frames.len();
                    match self.run(pc + 1, pos, base, None, false)? {
                        Some(end) => {
                            self.cut(base);
                            pos = end;
                            pc = *next;
                            continue;
                        }
                        None => false,
                    }
                }
                Inst::Look(look) => {
                    if self.look(look, pc + 1, pos)? {
                        pc = look.next;
                        continue;
                    }
                    false
                }
            };
            if matched {
                pc += 1;
                continue;
            }
            match self.backtrack(base)? {
                Some(branch) => {
                    pc = branch.pc;
                    pos = branch.pos;
                }
                None => return Ok(None),
            }
        }
    }

    /// Whether the lookaround `look`, its part starting at `pc`, holds at
    /// `pos`.
    fn look(&mut self, look: &LookInst, pc: usize, pos: usize) -> Result<bool, Error> {
        let base = self.cache.frames.len();
        let found = match look.behind {
            false => self.run(pc, pos, base, None, false)?.is_some(),
            true if look.last.as_ref().is_some_and(|last| {
                !self.haystack[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| last.contains(c))
            }) =>
            {
                // The character before cannot end the part.
                false
            }
            true => {
                let mut found = false;
                let nearest_first = look.min_len..=look.max_len.unwrap_or(usize::MAX);
                let lengths: Box<dyn Iterator<Item = usize>> = match &look.lengths {
                    Some(lengths) => Box::new(lengths.iter().copied()),
                    None => Box::new(nearest_first),
                };
                let ordered = look.lengths.is_some();
                for len in lengths {
                    let Some(start) = chars_before(self.haystack, pos, len) else {
                        // Longer ones cannot fit either, unless the order is
                        // the alternatives'.
                        match ordered {
                            true => continue,
                            false => break,
                        }
                    };
                    let outer = self.limit;
                    self.limit = pos;
                    let matched = self.run(pc, start, base, Some(pos), false);
                    self.limit = outer;
                    if matched?.is_some() {
                        found = true;
                        break;
                    }
                }
                found
            }
        };
        match (found, look.negated) {
            // Captures a positive lookaround set stay set.
            (true, false) => self.cut(base),
            (true, true) => self.unwind(base),
            (false, _) => {}
        }
        Ok(found != look.negated)
    }

    /// Where `char_loop`, starting at `pos`, ends once it has taken the
    /// fewest characters it must, and once it has taken all it can; `None`
    /// when it cannot take the fewest.
    fn take(&self, char_loop: &CharLoop, pos: usize) -> Option<(usize, usize)> {
        let mut end = pos;
        let mut min_end = pos;
        let mut count = 0;
        while char_loop.max.is_none_or(|max| count < max) {
            match self.char_within(end) {
                Some(c) if char_loop.matcher.matches(c) => end += c.len_utf8(),
                Some(_) | None => break,
            }
            count += 1;
            if count == char_loop.min {
                min_end = end;
            }
        }
        (count >= char_loop.min).then_some((min_end, end))
    }

    /// The character at byte `pos`, if it is before the limit.
    fn char_within(&self, pos: usize) -> Option<char> {
        match pos < self.limit {
            true => char_at(self.haystack, pos),
            false => None,
        }
    }

    /// Moves `pos` past the character there if it is one `accepts` takes.
    fn step(&self, pos: &mut usize, accepts: impl Fn(char) -> bool) -> bool {
        match self.char_within(*pos) {
            Some(c) if accepts(c) => {
                *pos += c.len_utf8();
                true
            }
            Some(_) | None => false,
        }
    }

    /// Whether `assertion` holds at `pos`.
    fn holds(&self, assertion: Assertion, pos: usize) -> bool {
        let bytes = self.haystack.as_bytes();
        let len = bytes.len();
        match assertion {
            Assertion::LineStart => pos == 0 || (bytes[pos - 1] == b'\n' && pos < len),
            Assertion::LineEnd => pos == len || bytes[pos] == b'\n',
            Assertion::TextStart => pos == 0 && self.options.text_start,
            Assertion::TextEnd => pos == len,
            Assertion::TextEndNewline => pos == len || (pos + 1 == len && bytes[pos] == b'\n'),
            Assertion::SearchStart => pos == self.search_start && self.options.search_start,
            Assertion::WordBoundary => self.is_word_boundary(pos, is_word),
            Assertion::NotWordBoundary => !self.is_word_boundary(pos, is_word),
            Assertion::AsciiWordBoundary => self.is_word_boundary(pos, is_ascii_word),
            Assertion::AsciiNotWordBoundary => !self.is_word_boundary(pos, is_ascii_word),
        }
    }

    /// Whether `pos` is between a word character and a non-word one, as
    /// `is_word` tells them.
    fn is_word_boundary(&self, pos: usize, is_word: fn(char) -> bool) -> bool {
        let before = self.haystack[..pos].chars().next_back();
        let after = char_at(self.haystack, pos);
        before.is_some_and(is_word) != after.is_some_and(is_word)
    }

    /// Where a match of `reference` starting at `pos` ends, if one does: the
    /// text of the last of its groups that took part matched again.
    fn back_reference(&self, reference: &BackRef, pos: usize) -> Option<usize> {
        reference.groups.iter().rev().find_map(|&group| {
            let (start, end) = (self.slots[group * 2]?, self.slots[group * 2 + 1]?);
            let captured = &self.haystack[start..end];
            let rest = &self.haystack[pos..self.limit.max(pos)];
            if !reference.casei {
                return rest.starts_with(captured).then_some(pos + captured.len());
            }
            let mut taken = 0;
            let mut chars = rest.chars();
            for expected in captured.chars() {
                let c = chars.next()?;
                if c != expected && !c.to_lowercase().eq(expected.to_lowercase()) {
                    return None;
                }
                taken += c.len_utf8();
            }
            Some(pos + taken)
        })
    }

    /// Starts memoizing: keyed by groups' spans for a pattern with
    /// back-references, else by bits if they are few enough.
    fn start_memo(&mut self) {
        if !matches!(self.memo, Memo::Off) {
            return;
        }
        if !self.program.referenced.is_empty() {
            self.cache.visited_keys.clear();
            self.memo = Memo::Keyed;
            return;
        }
        let stride = self.haystack.len() + 1;
        let bits = self.program.memo_points.saturating_mul(stride);
        if bits > MEMO_MAX_BITS {
            return;
        }
        self.cache.visited.clear();
        self.cache.visited.resize(bits.div_ceil(WORD_BITS), 0);
        self.memo = Memo::Bits(stride);
    }

    /// Where register `register` of the calls running now is.
    fn register(&self, register: usize) -> usize {
        self.cache.calls.len() * self.program.registers + register
    }

    /// Whether memo point `id` was reached at `pos` before, since memoizing
    /// started; marks it reached. Only outside calls: inside one, what
    /// follows depends on where it returns to.
    fn revisits(&mut self, id: Option<usize>, pos: usize) -> bool {
        let Some(id) = id else {
            return false;
        };
        if !self.cache.calls.is_empty() {
            return false;
        }
        let stride = match self.memo {
            Memo::Off => return false,
            Memo::Keyed => {
                if self.cache.visited_keys.len() >= MEMO_MAX_KEYS {
                    return false;
                }
                let groups = self
                    .program
                    .referenced
                    .iter()
                    .flat_map(|&group| [self.slots[group * 2], self.slots[group * 2 + 1]])
                    .collect();
                return !self.cache.visited_keys.insert((id, pos, groups));
            }
            Memo::Bits(stride) => stride,
        };
        let bit = id * stride + pos;
        let word = &mut self.cache.visited[bit / WORD_BITS];
        let mask = 1 << (bit % WORD_BITS);
        let seen = *word & mask != 0;
        *word |= mask;
        seen
    }

    /// Whether a slot of one of `groups` set since the stack was `from` deep
    /// holds a value other than it held before.
    fn changed_groups(&self, from: usize, groups: &[usize]) -> bool {
        self.cache.frames[from.min(self.cache.frames.len())..]
            .iter()
            .any(|frame| match frame {
                Frame::Slot(restore) => {
                    groups.contains(&(restore.index / 2))
                        && self.slots[restore.index] != restore.value
                }
                Frame::Branch(_)
                | Frame::Called
                | Frame::Register(_)
                | Frame::Returned(_)
                | Frame::Shrink(_) => false,
            })
    }

    /// Sets capture `slot` to `pos`, its old value restored on backtracking.
    fn save(&mut self, slot: usize, pos: usize) {
        self.cache.frames.push(Frame::Slot(Restore {
            index: slot,
            value: self.slots[slot],
        }));
        self.slots[slot] = Some(pos);
    }

    /// Pops the stack to the last place to resume from above `base`,
    /// restoring what it passes; `None` when there is none.
    fn backtrack(&mut self, base: usize) -> Result<Option<Branch>, Error> {
        while self.cache.frames.len() > base {
            match self.cache.frames.pop().expect("a frame above base") {
                Frame::Branch(branch) => {
                    self.backtracks += 1;
                    if self.backtracks > self.backtrack_limit {
                        return Err(Error::BacktrackLimit);
                    }
                    if self.backtracks == MEMO_AFTER {
                        self.start_memo();
                    }
                    return Ok(Some(branch));
                }
                Frame::Shrink(shrink) => {
                    self.backtracks += 1;
                    if self.backtracks > self.backtrack_limit {
                        return Err(Error::BacktrackLimit);
                    }
                    if self.backtracks == MEMO_AFTER {
                        self.start_memo();
                    }
                    let pos = char_start_before(self.haystack, shrink.pos);
                    if pos > shrink.min {
                        self.cache.frames.push(Frame::Shrink(Shrink {
                            pos,
                            ..shrink.clone()
                        }));
                    }
                    if shrink.memo && self.revisits(shrink.id, pos) {
                        continue;
                    }
                    return Ok(Some(Branch { pc: shrink.pc, pos }));
                }
                Frame::Called => {
                    self.cache.calls.pop();
                }
                Frame::Register(restore) => self.cache.registers[restore.index] = restore.value,
                Frame::Returned(to) => self.cache.calls.push(to),
                Frame::Slot(restore) => self.slots[restore.index] = restore.value,
            }
        }
        Ok(None)
    }

    /// Drops the places to resume from above `base`, keeping what restores
    /// registers and slots, so the part above is matched only one way.
    fn cut(&mut self, base: usize) {
        let mut kept = base;
        for i in base..self.cache.frames.len() {
            if !matches!(self.cache.frames[i], Frame::Branch(_) | Frame::Shrink(_)) {
                self.cache.frames.swap(kept, i);
                kept += 1;
            }
        }
        self.cache.frames.truncate(kept);
    }

    /// Pops the stack to `base`, restoring everything it passes.
    fn unwind(&mut self, base: usize) {
        while self.cache.frames.len() > base {
            match self.cache.frames.pop().expect("a frame above base") {
                Frame::Branch(_) | Frame::Shrink(_) => {}
                Frame::Called => {
                    self.cache.calls.pop();
                }
                Frame::Register(restore) => self.cache.registers[restore.index] = restore.value,
                Frame::Returned(to) => self.cache.calls.push(to),
                Frame::Slot(restore) => self.slots[restore.index] = restore.value,
            }
        }
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Searches `haystack` for the leftmost match of `program` within `bounds`,
/// filling `slots`; whether it found one.
pub(crate) fn search(
    program: &Program,
    cache: &mut Cache,
    haystack: &str,
    bounds: Bounds<'_>,
    slots: &mut [Option<usize>],
) -> Result<bool, Error> {
    let Bounds {
        backtrack_limit,
        filter,
        from,
        options,
        start,
    } = bounds;
    // The registers of the pattern outside calls; those of calls nested
    // deeper are added as calls reach them.
    if cache.registers.len() < program.registers {
        cache.registers.resize(program.registers, Mark::default());
    }
    let mut machine = Machine {
        backtracks: 0,
        backtrack_limit,
        cache,
        haystack,
        limit: haystack.len(),
        memo: Memo::Off,
        options,
        program,
        search_start: start,
        slots,
    };
    let bytes = haystack.as_bytes();
    let mut at = from;
    loop {
        // Skip to the next place a match may start: where the filter finds
        // a candidate, if any, and the anchor holds.
        let candidate = match filter {
            Some(filter) => filter.next_candidate(&mut machine.cache.automata, haystack, at),
            None => Some(at),
        };
        let next = candidate.and_then(|at| match program.anchor {
            Anchor::Floating => match &program.start_bytes {
                Some(start_bytes) => start_bytes.find(bytes, at),
                None => Some(at),
            },
            Anchor::LineStart if at == 0 || bytes[at - 1] == b'\n' => Some(at),
            Anchor::LineStart => {
                memchr::memchr(b'\n', &bytes[at..]).map(|newline| at + newline + 1)
            }
            Anchor::SearchStart => (at == start).then_some(at),
            Anchor::TextStart => (at == 0).then_some(at),
        });
        let Some(next) = next.filter(|&next| next <= bytes.len()) else {
            return Ok(false);
        };
        if options.anchored && next != from {
            return Ok(false);
        }
        at = next;
        machine.cache.frames.clear();
        machine.cache.calls.clear();
        machine.backtracks = 0;
        machine.slots.fill(None);
        machine.slots[0] = Some(at);
        if let Some(end) = machine.run(0, at, 0, None, true)? {
            machine.slots[1] = Some(end);
            return Ok(true);
        }
        match char_at(haystack, at) {
            Some(c) => at += c.len_utf8(),
            None => return Ok(false),
        }
    }
}

/// The character at byte `pos` of `haystack`.
fn char_at(haystack: &str, pos: usize) -> Option<char> {
    let byte = *haystack.as_bytes().get(pos)?;
    match byte.is_ascii() {
        true => Some(char::from(byte)),
        false => haystack[pos..].chars().next(),
    }
}

/// Where the character before byte `pos` of `haystack` starts.
fn char_start_before(haystack: &str, pos: usize) -> usize {
    match haystack.as_bytes()[pos - 1].is_ascii() {
        true => pos - 1,
        false => {
            pos - haystack[..pos]
                .chars()
                .next_back()
                .map_or(1, char::len_utf8)
        }
    }
}

/// Where the `len` characters before byte `pos` of `haystack` start.
fn chars_before(haystack: &str, pos: usize, len: usize) -> Option<usize> {
    let mut at = pos;
    let mut chars = haystack[..pos].chars();
    for _ in 0..len {
        at -= chars.next_back()?.len_utf8();
    }
    Some(at)
}

/// Whether `c` is an ASCII word character, as `\w` matches under `(?W)`.
fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `c` is a word character, as `\w` matches.
fn is_word(c: char) -> bool {
    static WORD: OnceLock<CharClass> = OnceLock::new();
    WORD.get_or_init(|| CharClass::new(Posix::Word.class()))
        .contains(c)
}
