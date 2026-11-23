//! Checks on subroutine calls, once a pattern is parsed: which groups are
//! called, and that no call recurses forever, as Oniguruma rejects.

use crate::error::Error;

use super::ast::Node;
use super::syntax;

// ========================================================================
// Constants
// ========================================================================

/// What Oniguruma reports for a recursion that never ends.
const NEVER_ENDING: &str = "never ending recursion";

// ========================================================================
// Data Structures
// ========================================================================

/// What compiling a pattern's calls needs to know of its groups, group 0
/// the whole pattern, by number.
#[derive(Debug)]
pub(crate) struct Calls {
    /// Whether each is called.
    pub(crate) called: Box<[bool]>,
    /// The most characters each takes; `None` when unbounded, as one that
    /// calls itself is. Empty without calls.
    pub(crate) max_lens: Box<[Option<usize>]>,
    /// The fewest characters each takes. Empty without calls.
    pub(crate) min_lens: Box<[usize]>,
}

/// Where a group is in the search for a cycle of calls.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Visit {
    /// Its calls are all searched.
    Done,
    /// Not reached yet.
    New,
    /// Its calls are being searched.
    Open,
}

// ========================================================================
// Functions
// ========================================================================

/// What compiling the calls of `node` needs, `groups` groups past group 0,
/// the whole pattern; an error for a recursion that never ends: one that
/// cannot stop calling, or calls itself where it started (`(?<a>\g<a>b|c)`).
pub(super) fn check_calls(node: &Node, groups: usize) -> Result<Calls, Error> {
    let mut called = vec![false; groups + 1];
    let mut bodies = vec![node; groups + 1];
    collect(node, &mut called, &mut bodies);
    if !called.contains(&true) {
        return Ok(Calls {
            called: called.into(),
            max_lens: Box::default(),
            min_lens: Box::default(),
        });
    }
    // Only what a match may run is checked: not a group repeated no times
    // and called only from itself.
    let mut reached = vec![false; groups + 1];
    reached[0] = true;
    reach(node, &bodies, &mut reached);
    let ends = fixpoint(vec![false; groups + 1], |ends| {
        bodies.iter().map(|body| can_end(body, ends)).collect()
    });
    if ends
        .iter()
        .zip(&reached)
        .any(|(&ends, &reached)| reached && !ends)
    {
        return Err(syntax(NEVER_ENDING, 0));
    }
    let min_lens = fixpoint(vec![0; groups + 1], |min_lens| {
        bodies
            .iter()
            .map(|body| body.min_len_with(min_lens))
            .collect()
    });
    let edges: Vec<Vec<usize>> = bodies
        .iter()
        .map(|body| {
            let mut targets = Vec::new();
            leading_calls(body, &min_lens, &mut targets);
            targets
        })
        .collect();
    let mut state = vec![Visit::New; groups + 1];
    for group in (0..=groups).filter(|&group| reached[group]) {
        if has_cycle(group, &edges, &mut state) {
            return Err(syntax(NEVER_ENDING, 0));
        }
    }
    // Known once every group it calls is: never for one that calls itself.
    let max_lens = fixpoint(vec![None; groups + 1], |max_lens| {
        bodies
            .iter()
            .map(|body| body.max_len_with(max_lens))
            .collect()
    });
    Ok(Calls {
        called: called.into(),
        max_lens: max_lens.into(),
        min_lens: min_lens.into(),
    })
}

/// Marks in `called` the groups `node` calls, and records each group's
/// body in `bodies`.
fn collect<'a>(node: &'a Node, called: &mut [bool], bodies: &mut [&'a Node]) {
    match node {
        Node::Call(group) => called[*group] = true,
        Node::Capture(capture) => {
            bodies[capture.index] = &capture.node;
            collect(&capture.node, called, bodies);
        }
        Node::Alt(parts) | Node::Concat(parts) => {
            for part in parts {
                collect(part, called, bodies);
            }
        }
        Node::Atomic(inner) => collect(inner, called, bodies),
        Node::Look(look) => collect(&look.node, called, bodies),
        Node::Repeat(repeat) => collect(&repeat.node, called, bodies),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => {}
    }
}

/// Marks in `reached` the groups a match of `node` may run, where it stands
/// or called, as `bodies` gives each group's body.
fn reach(node: &Node, bodies: &[&Node], reached: &mut [bool]) {
    match node {
        Node::Call(group) if !reached[*group] => {
            reached[*group] = true;
            reach(bodies[*group], bodies, reached);
        }
        Node::Capture(capture) => {
            reached[capture.index] = true;
            reach(&capture.node, bodies, reached);
        }
        Node::Alt(parts) | Node::Concat(parts) => {
            for part in parts {
                reach(part, bodies, reached);
            }
        }
        Node::Atomic(inner) => reach(inner, bodies, reached),
        Node::Look(look) => reach(&look.node, bodies, reached),
        Node::Repeat(repeat) if repeat.max == Some(0) => {}
        Node::Repeat(repeat) => reach(&repeat.node, bodies, reached),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Call(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => {}
    }
}

/// `start` with `step` applied until it stops changing, at most once per
/// group and once more: every step only moves it one way, settling one
/// more group's value at least.
fn fixpoint<T: PartialEq>(start: Vec<T>, step: impl Fn(&[T]) -> Vec<T>) -> Vec<T> {
    let mut current = start;
    for _ in 0..=current.len() {
        let next = step(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

/// Whether `node` can match without calling a group `ends` says cannot
/// stop calling.
fn can_end(node: &Node, ends: &[bool]) -> bool {
    match node {
        Node::Call(group) => ends[*group],
        Node::Alt(parts) => parts.iter().any(|part| can_end(part, ends)),
        Node::Concat(parts) => parts.iter().all(|part| can_end(part, ends)),
        Node::Atomic(inner) => can_end(inner, ends),
        Node::Capture(capture) => can_end(&capture.node, ends),
        Node::Look(look) => can_end(&look.node, ends),
        Node::Repeat(repeat) => repeat.min == 0 || can_end(&repeat.node, ends),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => true,
    }
}

/// Adds to `targets` the groups `node` may call before it has matched any
/// character, each group taking what `min_lens` says.
fn leading_calls(node: &Node, min_lens: &[usize], targets: &mut Vec<usize>) {
    match node {
        Node::Call(group) => targets.push(*group),
        Node::Alt(parts) => {
            for part in parts {
                leading_calls(part, min_lens, targets);
            }
        }
        Node::Concat(parts) => {
            for part in parts {
                leading_calls(part, min_lens, targets);
                if part.min_len_with(min_lens) > 0 {
                    break;
                }
            }
        }
        Node::Atomic(inner) => leading_calls(inner, min_lens, targets),
        Node::Capture(capture) => leading_calls(&capture.node, min_lens, targets),
        Node::Look(look) => leading_calls(&look.node, min_lens, targets),
        // A part repeated no times is never run (`(?<a>...){0}`).
        Node::Repeat(repeat) if repeat.max == Some(0) => {}
        Node::Repeat(repeat) => leading_calls(&repeat.node, min_lens, targets),
        Node::Any
        | Node::AnyButNewline
        | Node::Assert(_)
        | Node::BackRef(_)
        | Node::Char(_)
        | Node::Class(_)
        | Node::Empty
        | Node::Keep => {}
    }
}

/// Whether a cycle of `edges` runs through `group` or a group it reaches.
fn has_cycle(group: usize, edges: &[Vec<usize>], state: &mut [Visit]) -> bool {
    match state[group] {
        Visit::Done => return false,
        Visit::Open => return true,
        Visit::New => {}
    }
    state[group] = Visit::Open;
    if edges[group]
        .iter()
        .any(|&target| has_cycle(target, edges, state))
    {
        return true;
    }
    state[group] = Visit::Done;
    false
}
