//! The backtracking engine: a parsed pattern compiled to a short program,
//! and the machine that runs it, its backtracking memoized and bounded.

mod compile;
mod program;
mod vm;

pub(crate) use self::compile::compile;
pub(crate) use self::program::Program;
pub(crate) use self::vm::{Bounds, search};

pub use self::vm::Cache;
