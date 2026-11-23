//! Full case folding, where one character folds to several (`ß` to `ss`),
//! which Oniguruma applies ignoring case: literals expand into the ways a
//! text may spell their folded form, and classes take in their members'
//! folded forms.

use std::sync::OnceLock;

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

use super::ast::Node;
use super::class::fold;
use super::single_or_class;

// ========================================================================
// Constants
// ========================================================================

/// How many parts the expansion of one run of literals may take before it
/// is given up, the run then matched one character at a time: each place a
/// fold may start doubles what follows it.
const EXPANSION_LIMIT: usize = 256;

/// The most folds to several characters a class's members may have for
/// the class to take them in: a class named by a property (`[\w]`) holds
/// every such member, and trying each fold after it would slow it down.
const CLASS_FOLDS_MAX: usize = 4;

/// The pairs of ASCII letters a full fold may start with, lowercase: a run
/// of ASCII literals holding none needs no full folding.
const ASCII_FOLD_STARTS: &[[u8; 2]] = &[*b"ff", *b"fi", *b"fl", *b"ss", *b"st"];

/// How many ASCII characters there are, for their cached closures.
const ASCII_COUNT: usize = 128;

/// Every character whose full case fold is several characters, with that
/// fold, sorted: Unicode 15.1's `CaseFolding.txt`, status F.
const FULL_FOLDS: &[(char, &str)] = &[
    ('\u{DF}', "ss"),
    ('\u{130}', "i\u{307}"),
    ('\u{149}', "\u{2BC}n"),
    ('\u{1F0}', "j\u{30C}"),
    ('\u{390}', "\u{3B9}\u{308}\u{301}"),
    ('\u{3B0}', "\u{3C5}\u{308}\u{301}"),
    ('\u{587}', "\u{565}\u{582}"),
    ('\u{1E96}', "h\u{331}"),
    ('\u{1E97}', "t\u{308}"),
    ('\u{1E98}', "w\u{30A}"),
    ('\u{1E99}', "y\u{30A}"),
    ('\u{1E9A}', "a\u{2BE}"),
    ('\u{1E9E}', "ss"),
    ('\u{1F50}', "\u{3C5}\u{313}"),
    ('\u{1F52}', "\u{3C5}\u{313}\u{300}"),
    ('\u{1F54}', "\u{3C5}\u{313}\u{301}"),
    ('\u{1F56}', "\u{3C5}\u{313}\u{342}"),
    ('\u{1F80}', "\u{1F00}\u{3B9}"),
    ('\u{1F81}', "\u{1F01}\u{3B9}"),
    ('\u{1F82}', "\u{1F02}\u{3B9}"),
    ('\u{1F83}', "\u{1F03}\u{3B9}"),
    ('\u{1F84}', "\u{1F04}\u{3B9}"),
    ('\u{1F85}', "\u{1F05}\u{3B9}"),
    ('\u{1F86}', "\u{1F06}\u{3B9}"),
    ('\u{1F87}', "\u{1F07}\u{3B9}"),
    ('\u{1F88}', "\u{1F00}\u{3B9}"),
    ('\u{1F89}', "\u{1F01}\u{3B9}"),
    ('\u{1F8A}', "\u{1F02}\u{3B9}"),
    ('\u{1F8B}', "\u{1F03}\u{3B9}"),
    ('\u{1F8C}', "\u{1F04}\u{3B9}"),
    ('\u{1F8D}', "\u{1F05}\u{3B9}"),
    ('\u{1F8E}', "\u{1F06}\u{3B9}"),
    ('\u{1F8F}', "\u{1F07}\u{3B9}"),
    ('\u{1F90}', "\u{1F20}\u{3B9}"),
    ('\u{1F91}', "\u{1F21}\u{3B9}"),
    ('\u{1F92}', "\u{1F22}\u{3B9}"),
    ('\u{1F93}', "\u{1F23}\u{3B9}"),
    ('\u{1F94}', "\u{1F24}\u{3B9}"),
    ('\u{1F95}', "\u{1F25}\u{3B9}"),
    ('\u{1F96}', "\u{1F26}\u{3B9}"),
    ('\u{1F97}', "\u{1F27}\u{3B9}"),
    ('\u{1F98}', "\u{1F20}\u{3B9}"),
    ('\u{1F99}', "\u{1F21}\u{3B9}"),
    ('\u{1F9A}', "\u{1F22}\u{3B9}"),
    ('\u{1F9B}', "\u{1F23}\u{3B9}"),
    ('\u{1F9C}', "\u{1F24}\u{3B9}"),
    ('\u{1F9D}', "\u{1F25}\u{3B9}"),
    ('\u{1F9E}', "\u{1F26}\u{3B9}"),
    ('\u{1F9F}', "\u{1F27}\u{3B9}"),
    ('\u{1FA0}', "\u{1F60}\u{3B9}"),
    ('\u{1FA1}', "\u{1F61}\u{3B9}"),
    ('\u{1FA2}', "\u{1F62}\u{3B9}"),
    ('\u{1FA3}', "\u{1F63}\u{3B9}"),
    ('\u{1FA4}', "\u{1F64}\u{3B9}"),
    ('\u{1FA5}', "\u{1F65}\u{3B9}"),
    ('\u{1FA6}', "\u{1F66}\u{3B9}"),
    ('\u{1FA7}', "\u{1F67}\u{3B9}"),
    ('\u{1FA8}', "\u{1F60}\u{3B9}"),
    ('\u{1FA9}', "\u{1F61}\u{3B9}"),
    ('\u{1FAA}', "\u{1F62}\u{3B9}"),
    ('\u{1FAB}', "\u{1F63}\u{3B9}"),
    ('\u{1FAC}', "\u{1F64}\u{3B9}"),
    ('\u{1FAD}', "\u{1F65}\u{3B9}"),
    ('\u{1FAE}', "\u{1F66}\u{3B9}"),
    ('\u{1FAF}', "\u{1F67}\u{3B9}"),
    ('\u{1FB2}', "\u{1F70}\u{3B9}"),
    ('\u{1FB3}', "\u{3B1}\u{3B9}"),
    ('\u{1FB4}', "\u{3AC}\u{3B9}"),
    ('\u{1FB6}', "\u{3B1}\u{342}"),
    ('\u{1FB7}', "\u{3B1}\u{342}\u{3B9}"),
    ('\u{1FBC}', "\u{3B1}\u{3B9}"),
    ('\u{1FC2}', "\u{1F74}\u{3B9}"),
    ('\u{1FC3}', "\u{3B7}\u{3B9}"),
    ('\u{1FC4}', "\u{3AE}\u{3B9}"),
    ('\u{1FC6}', "\u{3B7}\u{342}"),
    ('\u{1FC7}', "\u{3B7}\u{342}\u{3B9}"),
    ('\u{1FCC}', "\u{3B7}\u{3B9}"),
    ('\u{1FD2}', "\u{3B9}\u{308}\u{300}"),
    ('\u{1FD3}', "\u{3B9}\u{308}\u{301}"),
    ('\u{1FD6}', "\u{3B9}\u{342}"),
    ('\u{1FD7}', "\u{3B9}\u{308}\u{342}"),
    ('\u{1FE2}', "\u{3C5}\u{308}\u{300}"),
    ('\u{1FE3}', "\u{3C5}\u{308}\u{301}"),
    ('\u{1FE4}', "\u{3C1}\u{313}"),
    ('\u{1FE6}', "\u{3C5}\u{342}"),
    ('\u{1FE7}', "\u{3C5}\u{308}\u{342}"),
    ('\u{1FF2}', "\u{1F7C}\u{3B9}"),
    ('\u{1FF3}', "\u{3C9}\u{3B9}"),
    ('\u{1FF4}', "\u{3CE}\u{3B9}"),
    ('\u{1FF6}', "\u{3C9}\u{342}"),
    ('\u{1FF7}', "\u{3C9}\u{342}\u{3B9}"),
    ('\u{1FFC}', "\u{3C9}\u{3B9}"),
    ('\u{FB00}', "ff"),
    ('\u{FB01}', "fi"),
    ('\u{FB02}', "fl"),
    ('\u{FB03}', "ffi"),
    ('\u{FB04}', "ffl"),
    ('\u{FB05}', "st"),
    ('\u{FB06}', "st"),
    ('\u{FB13}', "\u{574}\u{576}"),
    ('\u{FB14}', "\u{574}\u{565}"),
    ('\u{FB15}', "\u{574}\u{56B}"),
    ('\u{FB16}', "\u{57E}\u{576}"),
    ('\u{FB17}', "\u{574}\u{56D}"),
];

// ========================================================================
// Functions
// ========================================================================

/// The parts matching the literals `chars` ignoring case, full folds
/// included; `None` when none applies, so each may be matched alone.
pub(super) fn fold_literals(chars: &[char]) -> Option<Vec<Node>> {
    // Most runs are ASCII words, which only a few letter pairs fold from.
    if chars.iter().all(char::is_ascii)
        && !chars.windows(2).any(|pair| {
            let pair = [pair[0] as u8, pair[1] as u8].map(|b| b.to_ascii_lowercase());
            ASCII_FOLD_STARTS.contains(&pair)
        })
    {
        return None;
    }
    let folded: Vec<char> = chars
        .iter()
        .flat_map(|&c| match full_fold(c) {
            Some(fold) => fold.chars().collect(),
            None => vec![c],
        })
        .collect();
    let closures: Vec<ClassUnicode> = folded.iter().map(|&c| closure(c)).collect();
    // The stretches folds cover, those overlapping merged: each is expanded
    // on its own, so what follows one is not repeated in every way of it.
    let mut stretches: Vec<(usize, usize)> = Vec::new();
    for at in 0..closures.len() {
        let Some(end) = folds_at(&closures, at)
            .iter()
            .map(|(_, spans)| at + spans)
            .max()
        else {
            continue;
        };
        match stretches.last_mut() {
            Some((_, last_end)) if at < *last_end => *last_end = end.max(*last_end),
            Some(_) | None => stretches.push((at, end)),
        }
    }
    if stretches.is_empty() && folded.len() == chars.len() {
        return None;
    }
    let mut budget = EXPANSION_LIMIT;
    let mut parts = Vec::new();
    let mut at = 0;
    for (start, end) in stretches {
        parts.extend(closures[at..start].iter().cloned().map(single_or_class));
        parts.extend(expand(&closures[start..end], 0, &mut budget)?);
        at = end;
    }
    parts.extend(closures[at..].iter().cloned().map(single_or_class));
    Some(parts)
}

/// The other ways a class's members may be spelled ignoring case: for each
/// fold to several characters of a member, those characters, one by one.
/// Nothing for a class with more such folds than [`CLASS_FOLDS_MAX`], which
/// names its members by property rather than one by one.
pub(super) fn class_folds(class: &ClassUnicode) -> Vec<Node> {
    static FOLDING: OnceLock<ClassUnicode> = OnceLock::new();
    let folding = FOLDING.get_or_init(|| {
        ClassUnicode::new(
            FULL_FOLDS
                .iter()
                .map(|&(c, _)| ClassUnicodeRange::new(c, c)),
        )
    });
    let mut members = class.clone();
    members.intersect(folding);
    if members.ranges().is_empty() {
        return Vec::new();
    }
    let mut folds: Vec<&str> = FULL_FOLDS
        .iter()
        .filter(|(c, _)| contains(&members, *c))
        .map(|(_, fold)| *fold)
        .collect();
    folds.sort_unstable();
    folds.dedup();
    if folds.len() > CLASS_FOLDS_MAX {
        return Vec::new();
    }
    folds
        .into_iter()
        .map(|fold| Node::Concat(fold.chars().map(|c| single_or_class(closure(c))).collect()))
        .collect()
}

/// Whether `class` holds `c`.
fn contains(class: &ClassUnicode, c: char) -> bool {
    class
        .ranges()
        .iter()
        .any(|range| range.start() <= c && c <= range.end())
}

/// The full fold of `c`, if it folds to several characters.
fn full_fold(c: char) -> Option<&'static str> {
    FULL_FOLDS
        .binary_search_by_key(&c, |(from, _)| *from)
        .ok()
        .map(|at| FULL_FOLDS[at].1)
}

/// `c` and every character that matches it ignoring case, one to one:
/// cached for ASCII, which most literals are.
fn closure(c: char) -> ClassUnicode {
    static ASCII: OnceLock<Vec<ClassUnicode>> = OnceLock::new();
    let compute = |c: char| fold(ClassUnicode::new([ClassUnicodeRange::new(c, c)]), false);
    match c.is_ascii() {
        true => ASCII.get_or_init(|| {
            (0..ASCII_COUNT as u8)
                .map(|b| compute(char::from(b)))
                .collect()
        })[c as usize]
            .clone(),
        false => compute(c),
    }
}

/// The characters whose full fold matches the folded literals from `at`,
/// as `closures` gives what each matches, grouped by how many literals
/// their fold spans.
fn folds_at(closures: &[ClassUnicode], at: usize) -> Vec<(ClassUnicode, usize)> {
    let mut found: Vec<(ClassUnicode, usize)> = Vec::new();
    for (c, fold) in FULL_FOLDS {
        let len = fold.chars().count();
        let matches = closures.len() - at >= len
            && fold
                .chars()
                .zip(&closures[at..])
                .all(|(folded, closure)| contains(closure, folded));
        if !matches {
            continue;
        }
        let one = ClassUnicode::new([ClassUnicodeRange::new(*c, *c)]);
        match found.iter_mut().find(|(_, spans)| *spans == len) {
            Some((class, _)) => class.union(&one),
            None => found.push((one, len)),
        }
    }
    found
}

/// The parts matching the folded literals from `at`, as `closures` gives
/// what each matches: where a full fold may start, an alternation between
/// the literals one by one and the character folding to them. `None` once
/// more than `budget` parts are made.
fn expand(closures: &[ClassUnicode], at: usize, budget: &mut usize) -> Option<Vec<Node>> {
    let mut parts = Vec::new();
    for at in at..closures.len() {
        *budget = budget.checked_sub(1)?;
        let folds = folds_at(closures, at);
        let single = single_or_class(closures[at].clone());
        if folds.is_empty() {
            parts.push(single);
            continue;
        }
        let mut one_by_one = vec![single];
        one_by_one.extend(expand(closures, at + 1, budget)?);
        let mut alternatives = vec![Node::Concat(one_by_one)];
        for (class, spans) in folds {
            let mut folded = vec![single_or_class(class)];
            folded.extend(expand(closures, at + spans, budget)?);
            alternatives.push(Node::Concat(folded));
        }
        parts.push(Node::Alt(alternatives));
        return Some(parts);
    }
    Some(parts)
}
