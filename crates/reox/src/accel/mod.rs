//! What speeds a search up beside the backtracking machine: the characters
//! a match may start with, to skip ahead to, and automata for long texts.

mod automata;
mod prefilter;

pub(crate) use self::automata::{Accelerator, AutomataCache, Filter, Plan, regular_input};
pub(crate) use self::prefilter::{StartBytes, first_chars, last_chars};
