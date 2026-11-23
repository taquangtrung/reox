//! Automata for what needs no backtracking, from `regex-automata`: a pattern
//! that is regular runs on them outright, and one that is not runs on the
//! backtracking machine only where a looser, regular form of it, searched on
//! them, says a match may start. They serve long searches only, and are
//! built only once a pattern has scanned enough text to repay them, so
//! compiling stays cheap and a line of code stays on the backtracking
//! machine.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use regex_automata::meta;
use regex_automata::nfa::thompson::WhichCaptures;
use regex_automata::{Anchored, Input};
use regex_syntax::hir::{self, Class, ClassUnicode, ClassUnicodeRange, Hir, Look};

use crate::syntax::{Assertion, Node, Parsed};

// ========================================================================
// Constants
// ========================================================================

/// How many bytes of long searches a pattern scans on the backtracking
/// machine before its automata are built: building them costs what scanning
/// that much does, and they repay it only over long texts.
const HOT_BYTES: usize = 64 << 10;

/// The fewest bytes a search must have ahead of it to run on the automata:
/// on a shorter text, a line of code, the backtracking machine finds a match
/// and its groups in one pass, where the automata would need two.
const LONG_SEARCH: usize = 256;

/// How many candidates a filter finds before it is judged.
const FILTER_PROBE: usize = 256;

/// The fewest bytes a filter must skip per candidate, on average, to be
/// kept: one that finds a candidate nearly everywhere only adds work.
const FILTER_MIN_SKIP: usize = 8;

/// The most bytes a filter may scan per byte it skips, once judged: the
/// automata scan a byte many times faster than the backtracking machine
/// tries a position, but a filter whose loose matches run long scans the
/// same text again for every candidate.
const FILTER_SCAN_PER_SKIP: usize = 32;

/// How many patterns' automata one search state keeps state for at once:
/// a highlighter tries several patterns at each place in a line.
const AUTOMATA_SLOTS: usize = 4;

/// The most memory the automata of one pattern may take.
const SIZE_LIMIT: usize = 16 << 20;

/// The most memory a lazy DFA's cache may take, per search state: kept
/// small, as every hot pattern has one.
const HYBRID_CACHE: usize = 256 << 10;

/// The id the next accelerator takes, which tells a search state whose
/// automata it was made for: an address can be taken again once its
/// pattern is dropped.
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

// ========================================================================
// Data Structures
// ========================================================================

/// What automata a pattern may run on, built once it is hot.
#[derive(Debug)]
pub(crate) struct Accelerator {
    /// Whether its automata are built at once, run on every search, and
    /// its filter never dropped: for testing.
    eager: bool,
    /// Which accelerator it is, unique for the life of the process.
    id: usize,
    /// How the pattern uses them.
    plan: Plan,
    /// How many bytes its long searches have had ahead of them.
    searched: AtomicUsize,
    /// How many candidates its filter has found, until it is judged.
    candidates: AtomicUsize,
    /// How many bytes its filter has scanned, until it is judged.
    scanned: AtomicUsize,
    /// How many bytes its filter has skipped, until it is judged.
    skipped: AtomicUsize,
    /// Whether its filter was judged to skip too little, and dropped.
    dropped: AtomicBool,
    /// The automata, built once hot; `None` when they cannot be built.
    regex: OnceLock<Option<meta::Regex>>,
}

/// How a pattern uses automata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Plan {
    /// It does not: no regular form of it filters anything.
    Backtrack,
    /// They find candidates for the backtracking machine to try.
    Filter,
    /// They run it outright.
    Regular(Regular),
}

/// A regular pattern, as the automata run it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Regular {
    /// Where it is anchored, by a leading `\A` or `\G`.
    pub(crate) leading: Leading,
    /// Whether it holds `^`, which in the automata also matches after a
    /// text's final newline, where Oniguruma's does not.
    pub(crate) line_start: bool,
}

/// A regular pattern's leading anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Leading {
    /// None.
    Floating,
    /// `\G`: where the search starts.
    SearchStart,
    /// `\A`: the text's start.
    TextStart,
}

/// A filter's automata, which find where the backtracking machine may
/// match, as the pattern loosened matches there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Filter<'a> {
    /// What the automata belong to.
    pub(crate) accelerator: &'a Accelerator,
    /// The automata, built.
    pub(crate) automata: &'a meta::Regex,
}

/// The automata's search state, for the few patterns last searched.
#[derive(Clone, Debug, Default)]
pub(crate) struct AutomataCache {
    /// The slot to reuse next when every one is taken.
    next: usize,
    /// The state of each pattern's automata.
    slots: Vec<AutomataSlot>,
}

/// One pattern's automata's search state.
#[derive(Clone, Debug)]
struct AutomataSlot {
    /// The automata's own state.
    cache: meta::Cache,
    /// Which accelerator's automata it is for.
    owner: usize,
}

// ========================================================================
// Accelerator Implementations
// ========================================================================

impl Accelerator {
    /// The automata `node`, parsed, may run on; built at once and run on
    /// every search when `eager`.
    pub(crate) fn new(node: &Node, eager: bool) -> Self {
        Self {
            eager,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            plan: plan(node),
            searched: AtomicUsize::new(0),
            candidates: AtomicUsize::new(0),
            scanned: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            dropped: AtomicBool::new(false),
            regex: OnceLock::new(),
        }
    }

    /// How the pattern uses automata: a filter judged to skip too little
    /// is dropped.
    pub(crate) fn plan(&self) -> Plan {
        match self.plan {
            Plan::Filter if self.dropped.load(Ordering::Relaxed) => Plan::Backtrack,
            plan => plan,
        }
    }

    /// The automata, for a search with `remaining` bytes ahead, once the
    /// pattern is hot: a long search counts what it may scan, and builds
    /// them on the one that makes it hot, from the pattern `reparse` reads
    /// again.
    pub(crate) fn ready(
        &self,
        reparse: impl FnOnce() -> Option<Parsed>,
        remaining: usize,
    ) -> Option<&meta::Regex> {
        if self.plan() == Plan::Backtrack || (remaining < LONG_SEARCH && !self.eager) {
            return None;
        }
        if let Some(built) = self.regex.get() {
            return built.as_ref();
        }
        if self.searched.fetch_add(remaining, Ordering::Relaxed) + remaining < HOT_BYTES
            && !self.eager
        {
            return None;
        }
        self.regex
            .get_or_init(|| build(&reparse()?, self.plan))
            .as_ref()
    }
}

impl Accelerator {
    /// The span of the leftmost match in `input`, found by `automata`, which
    /// must be this accelerator's own. The backtracking machine fills in the
    /// groups afterwards, so the automata run their fastest search. Their
    /// state takes a slot of `cache`: their own, a free one, or else the one
    /// taken longest ago, reset.
    pub(crate) fn find(
        &self,
        cache: &mut AutomataCache,
        automata: &meta::Regex,
        input: &Input<'_>,
    ) -> Option<(usize, usize)> {
        let at = match cache.slots.iter().position(|slot| slot.owner == self.id) {
            Some(at) => at,
            None if cache.slots.len() < AUTOMATA_SLOTS => {
                cache.slots.push(AutomataSlot {
                    cache: automata.create_cache(),
                    owner: self.id,
                });
                cache.slots.len() - 1
            }
            None => {
                let at = cache.next;
                cache.next = (at + 1) % AUTOMATA_SLOTS;
                let slot = &mut cache.slots[at];
                slot.cache.reset(automata);
                slot.owner = self.id;
                at
            }
        };
        automata
            .search_with(&mut cache.slots[at].cache, input)
            .map(|found| (found.start(), found.end()))
    }

    /// Notes that the filter found a candidate `skipped` bytes past where
    /// it looked from, having scanned `scanned` bytes to find it, dropping
    /// the filter once it has found enough to be judged and skips too
    /// little, or scans too much for what it skips.
    fn note_candidate(&self, skipped: usize, scanned: usize) {
        if self.eager || self.dropped.load(Ordering::Relaxed) {
            return;
        }
        let candidates = self.candidates.fetch_add(1, Ordering::Relaxed) + 1;
        let skipped = self.skipped.fetch_add(skipped, Ordering::Relaxed) + skipped;
        let scanned = self.scanned.fetch_add(scanned, Ordering::Relaxed) + scanned;
        if candidates >= FILTER_PROBE
            && (skipped < candidates * FILTER_MIN_SKIP
                || scanned > skipped.saturating_mul(FILTER_SCAN_PER_SKIP))
        {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }
}

// ========================================================================
// Filter Implementations
// ========================================================================

impl Filter<'_> {
    /// Where, at `from` or after it, the first candidate in `haystack` is,
    /// noting how far it skipped; `None` when no match starts there.
    pub(crate) fn next_candidate(
        &self,
        cache: &mut AutomataCache,
        haystack: &str,
        from: usize,
    ) -> Option<usize> {
        let input = Input::new(haystack).range(from..);
        let (candidate, end) = self.accelerator.find(cache, self.automata, &input)?;
        // The automata scan to where the loose match ends, and back.
        self.accelerator
            .note_candidate(candidate - from, (end - from) + (end - candidate));
        Some(candidate)
    }
}

// ========================================================================
// Functions
// ========================================================================

/// The input for searching a regular pattern from `start`, anchored as its
/// leading anchor and `anchored` say; `None` when the anchor rules a match
/// out, as `\A` past the text's start or `\G` where it may not match.
pub(crate) fn regular_input<'h>(
    haystack: &'h str,
    start: usize,
    leading: Leading,
    anchored: bool,
    text_start: bool,
    search_start: bool,
) -> Option<Input<'h>> {
    let input = Input::new(haystack).range(start..);
    let anchored = match leading {
        Leading::Floating => anchored,
        Leading::SearchStart if !search_start => return None,
        Leading::SearchStart => true,
        Leading::TextStart if start != 0 || !text_start => return None,
        Leading::TextStart => true,
    };
    Some(match anchored {
        true => input.anchored(Anchored::Yes),
        false => input,
    })
}

/// How `node` may use automata.
fn plan(node: &Node) -> Plan {
    let (leading, rest) = leading_anchor(node);
    if rest.iter().all(|part| is_regular(part)) {
        return Plan::Regular(Regular {
            leading,
            line_start: rest.iter().any(|part| holds_line_start(part)),
        });
    }
    // A looser form that may match nothing finds a candidate everywhere.
    // It is as long as the pattern at least, lookarounds and
    // back-references counting as nothing, as they do in `min_len`.
    match node.min_len() {
        0 => Plan::Backtrack,
        _ => Plan::Filter,
    }
}

/// `node`'s leading `\A` or `\G`, and the parts after it.
fn leading_anchor(node: &Node) -> (Leading, Vec<&Node>) {
    let parts: Vec<&Node> = match node {
        Node::Concat(parts) => parts.iter().collect(),
        other => vec![other],
    };
    match parts.first() {
        Some(Node::Assert(Assertion::SearchStart)) => (Leading::SearchStart, parts[1..].to_vec()),
        Some(Node::Assert(Assertion::TextStart)) => (Leading::TextStart, parts[1..].to_vec()),
        Some(_) | None => (Leading::Floating, parts),
    }
}

/// Whether the automata match `node` exactly as the backtracking machine
/// does: no lookaround, back-reference, atomic group, `\K`, `\Z`, or anchor
/// on where a search started, and no unbounded loop over what may match
/// nothing, whose captures Oniguruma treats its own way.
fn is_regular(node: &Node) -> bool {
    match node {
        Node::Alt(parts) | Node::Concat(parts) => parts.iter().all(is_regular),
        Node::Any | Node::AnyButNewline | Node::Char(_) | Node::Class(_) | Node::Empty => true,
        Node::Assert(assertion) => match assertion {
            Assertion::LineStart
            | Assertion::LineEnd
            | Assertion::TextEnd
            | Assertion::WordBoundary
            | Assertion::NotWordBoundary
            | Assertion::AsciiWordBoundary
            | Assertion::AsciiNotWordBoundary => true,
            Assertion::SearchStart | Assertion::TextStart | Assertion::TextEndNewline => false,
        },
        Node::Capture(capture) => is_regular(&capture.node),
        Node::Repeat(repeat) => {
            (repeat.max.is_some() || repeat.node.min_len() > 0) && is_regular(&repeat.node)
        }
        Node::Atomic(_) | Node::BackRef(_) | Node::Call(_) | Node::Keep | Node::Look(_) => false,
    }
}

/// Whether `node` holds `^`.
fn holds_line_start(node: &Node) -> bool {
    match node {
        Node::Assert(Assertion::LineStart) => true,
        Node::Alt(parts) | Node::Concat(parts) => parts.iter().any(holds_line_start),
        Node::Capture(capture) => holds_line_start(&capture.node),
        Node::Repeat(repeat) => holds_line_start(&repeat.node),
        Node::Atomic(inner) => holds_line_start(inner),
        Node::Look(look) => holds_line_start(&look.node),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => false,
    }
}

/// The automata for `parsed`, as `plan` uses them; `None` when they cannot
/// be built within their size limit.
fn build(parsed: &Parsed, plan: Plan) -> Option<meta::Regex> {
    let hir = match plan {
        Plan::Backtrack => return None,
        Plan::Filter => loosen(&parsed.node),
        Plan::Regular(_) => {
            let (_, rest) = leading_anchor(&parsed.node);
            Hir::concat(rest.into_iter().map(exact).collect())
        }
    };
    meta::Builder::new()
        .configure(
            meta::Config::new()
                .nfa_size_limit(Some(SIZE_LIMIT))
                .hybrid_cache_capacity(HYBRID_CACHE)
                .which_captures(WhichCaptures::Implicit)
                .onepass(false)
                .backtrack(false),
        )
        .build_from_hir(&hir)
        .ok()
}

/// `node`, regular, as the automata read it, its groups numbered but not
/// named: only where a match is matters, and a set's patterns may share a
/// name.
fn exact(node: &Node) -> Hir {
    match node {
        Node::Alt(parts) => Hir::alternation(parts.iter().map(exact).collect()),
        Node::Concat(parts) => Hir::concat(parts.iter().map(exact).collect()),
        Node::Capture(capture) => Hir::capture(hir::Capture {
            index: capture.index as u32,
            name: None,
            sub: Box::new(exact(&capture.node)),
        }),
        Node::Repeat(repeat) => Hir::repetition(hir::Repetition {
            greedy: repeat.greedy,
            max: repeat.max,
            min: repeat.min,
            sub: Box::new(exact(&repeat.node)),
        }),
        Node::Assert(assertion) => match assertion {
            Assertion::LineStart => Hir::look(Look::StartLF),
            Assertion::LineEnd => Hir::look(Look::EndLF),
            Assertion::TextEnd => Hir::look(Look::End),
            Assertion::WordBoundary => Hir::look(Look::WordUnicode),
            Assertion::NotWordBoundary => Hir::look(Look::WordUnicodeNegate),
            Assertion::AsciiWordBoundary => Hir::look(Look::WordAscii),
            Assertion::AsciiNotWordBoundary => Hir::look(Look::WordAsciiNegate),
            Assertion::SearchStart | Assertion::TextStart | Assertion::TextEndNewline => {
                unreachable!("not regular")
            }
        },
        Node::Any
        | Node::AnyButNewline
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Atomic(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Keep
        | Node::Look(_) => char_part(node),
    }
}

/// `node` loosened into a regular pattern that matches wherever it does, at
/// least: what asserts is dropped, an atomic group made ordinary, and a
/// back-reference or call matches anything. Groups capture nothing.
fn loosen(node: &Node) -> Hir {
    match node {
        Node::Alt(parts) => Hir::alternation(parts.iter().map(loosen).collect()),
        Node::Concat(parts) => Hir::concat(parts.iter().map(loosen).collect()),
        Node::Capture(capture) => loosen(&capture.node),
        Node::Atomic(inner) => loosen(inner),
        Node::Repeat(repeat) => Hir::repetition(hir::Repetition {
            greedy: repeat.greedy,
            max: repeat.max,
            min: repeat.min,
            sub: Box::new(loosen(&repeat.node)),
        }),
        Node::BackRef(_) | Node::Call(_) => Hir::repetition(hir::Repetition {
            greedy: false,
            max: None,
            min: 0,
            sub: Box::new(any()),
        }),
        Node::Assert(_) | Node::Keep | Node::Look(_) | Node::Empty => Hir::empty(),
        Node::Any | Node::AnyButNewline | Node::Char(_) | Node::Class(_) => char_part(node),
    }
}

/// A part matching one character, or nothing, as the automata read it.
fn char_part(node: &Node) -> Hir {
    match node {
        Node::Any => any(),
        Node::AnyButNewline => {
            let mut class = ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]);
            class.negate();
            Hir::class(Class::Unicode(class))
        }
        Node::Char(c) => Hir::literal(c.to_string().into_bytes()),
        Node::Class(class) => Hir::class(Class::Unicode(class.clone())),
        Node::Alt(_)
        | Node::Assert(_)
        | Node::Atomic(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Capture(_)
        | Node::Concat(_)
        | Node::Empty
        | Node::Keep
        | Node::Look(_)
        | Node::Repeat(_) => Hir::empty(),
    }
}

/// Any character.
fn any() -> Hir {
    Hir::class(Class::Unicode(ClassUnicode::new([ClassUnicodeRange::new(
        '\0',
        char::MAX,
    )])))
}
