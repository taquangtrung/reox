//! Benchmarks reox against fancy-regex and Oniguruma.
//!
//! - `reox-bench [grammars] [samples]`: the patterns of a folder of
//!   TextMate grammars compiled, then searched in sample texts; both
//!   folders default to an iris checkout beside this one.
//! - `reox-bench tokenize [samples]`: the pre-tokenizer pattern of
//!   OpenAI's cl100k tokenizer run over the sample texts.
//! - `reox-bench redux`: the Computer Language Benchmarks Game's
//!   regex-redux, on a DNA sequence it generates.
//! - `reox-bench redos`: patterns known to backtrack catastrophically, on
//!   the inputs that make them.

mod engines;
mod grammars;
mod workloads;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if grammars::run_child_mode(&args) {
        return;
    }
    match args.first().map(String::as_str) {
        Some("tokenize") => workloads::tokenize(&args[1..]),
        Some("redux") => workloads::redux(),
        Some("redos") => workloads::redos(),
        Some(_) | None => grammars::run(&args),
    }
}

/// Every file under `dir`, sorted.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match path.is_dir() {
                true => pending.push(path),
                false => found.push(path),
            }
        }
    }
    found.sort();
    found
}

/// What `f` returns, and how long it took.
pub(crate) fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

/// `duration` in milliseconds.
pub(crate) fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}
