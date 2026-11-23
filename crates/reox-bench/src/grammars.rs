//! The grammar benchmark: the patterns of a folder of TextMate grammars
//! compiled by each engine, then searched in the first lines of sample
//! files. Each engine runs in a process of its own, capped in memory, so its
//! peak memory is its own and a pattern that makes it allocate without bound
//! crashes only that process, and counts as a crash.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use onig::{MatchParam, Region, SearchOptions};
use serde_json::Value;

use crate::engines::{Engine, fancy, oniguruma, reox};
use crate::{files, millis, time};

// ========================================================================
// Constants
// ========================================================================

/// Where the grammars are when no folder is given: iris's bundle.
const DEFAULT_GRAMMARS: &str = "../iris/crates/iris-grammars";

/// Where the sample texts are when no folder is given: VS Code's colorize
/// fixtures, in iris's tests.
pub(crate) const DEFAULT_SAMPLES: &str = "../iris/crates/iris/tests/fixtures/colorize/fixtures";

/// The argument that runs one engine on the patterns sent on stdin.
const ENGINE_FLAG: &str = "--engine";

/// The argument that reports which patterns sent on stdin one engine
/// compiles, one line each as it goes.
const CHECK_FLAG: &str = "--check";

/// The most memory an engine's process may take, in kibibytes.
const MEMORY_CAP_KIB: u64 = 4 << 20;

/// The keys of a grammar rule that hold patterns.
const PATTERN_KEYS: [&str; 4] = ["begin", "end", "match", "while"];

/// How many lines of each sample file are searched.
const LINES_PER_FILE: usize = 40;

/// The line of `/proc/self/status` giving a process's peak memory.
const PEAK_MEMORY_FIELD: &str = "VmHWM:";

/// Kibibytes in a mebibyte.
const KIB_PER_MIB: f64 = 1024.0;

// ========================================================================
// Data Structures
// ========================================================================

/// What one engine measured.
struct Measure {
    /// How long compiling every pattern took.
    compile: Duration,
    /// How many searches gave up at the backtrack limit.
    gave_up: usize,
    /// How many searches found a match.
    matches: usize,
    /// The process's peak memory, in kibibytes.
    peak_kib: u64,
    /// How long the searches took.
    search: Duration,
}

// ========================================================================
// Functions
// ========================================================================

/// Runs one engine for the benchmark's parent, if `args` ask for that;
/// whether they did.
pub(crate) fn run_child_mode(args: &[String]) -> bool {
    match args {
        [flag, name, samples] if flag == ENGINE_FLAG => {
            run_engine(engine_named(name), Path::new(samples))
        }
        [flag, name] if flag == CHECK_FLAG => run_check(engine_named(name)),
        _ => return false,
    }
    true
}

/// Runs the benchmark on the grammars and samples `args` name.
pub(crate) fn run(args: &[String]) {
    let grammars = PathBuf::from(args.first().map_or(DEFAULT_GRAMMARS, String::as_str));
    let samples = PathBuf::from(args.get(1).map_or(DEFAULT_SAMPLES, String::as_str));
    let patterns = grammar_patterns(&grammars);
    println!(
        "{} patterns from {}, {} lines from {}",
        patterns.len(),
        grammars.display(),
        sample_lines(&samples).len(),
        samples.display()
    );
    let mut every = vec![true; patterns.len()];
    for engine in Engine::ALL {
        let (compiled, crashed) = check_engine(engine, &patterns);
        let rejected = compiled.iter().filter(|&&ok| !ok).count() - crashed;
        println!(
            "{:<12} rejects {rejected}, crashes on {crashed}",
            engine.name()
        );
        for (all, ok) in every.iter_mut().zip(compiled) {
            *all &= ok;
        }
    }
    let common: Vec<String> = patterns
        .into_iter()
        .zip(every)
        .filter_map(|(pattern, ok)| ok.then_some(pattern))
        .collect();
    println!(
        "{} patterns every engine compiles, searched in every line from its start\n",
        common.len()
    );
    println!(
        "{:<12} {:>10} {:>10} {:>9} {:>8} {:>10}",
        "engine", "compile", "search", "matches", "gave up", "peak"
    );
    for engine in Engine::ALL {
        println!(
            "{:<12} {}",
            engine.name(),
            spawn_engine(engine, &common, &samples)
        );
    }
}

/// The engine named `name`, given on the command line.
fn engine_named(name: &str) -> Engine {
    Engine::from_name(name).expect("a known engine")
}

/// Which of `patterns` `engine` compiles, checked in processes of its
/// own, and how many crashed it: a crash counts as not compiling, and the
/// check goes on past it.
fn check_engine(engine: Engine, patterns: &[String]) -> (Vec<bool>, usize) {
    let mut compiled = Vec::with_capacity(patterns.len());
    let mut crashed = 0;
    while compiled.len() < patterns.len() {
        let rest = &patterns[compiled.len()..];
        let (succeeded, output) = run_child(&[CHECK_FLAG, engine.name()], rest);
        compiled.extend(output.lines().map(|line| line == "1"));
        if !succeeded {
            compiled.push(false);
            crashed += 1;
        }
    }
    compiled.truncate(patterns.len());
    (compiled, crashed)
}

/// Reports which patterns on stdin `engine` compiles, a line each.
fn run_check(engine: Engine) {
    let patterns = read_patterns();
    let mut stdout = io::stdout();
    for pattern in &patterns {
        let line = if engine.compiles(pattern) { "1" } else { "0" };
        writeln!(stdout, "{line}").expect("stdout");
        stdout.flush().expect("stdout");
    }
}

/// Runs this benchmark with `args` in a process of its own, capped in
/// memory, `patterns` sent on its stdin: whether it succeeded, and what it
/// printed.
fn run_child(args: &[&str], patterns: &[String]) -> (bool, String) {
    let exe = env::current_exe().expect("the benchmark's path");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("ulimit -v {MEMORY_CAP_KIB}; exec \"$0\" \"$@\""))
        .arg(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the benchmark runs");
    let input = serde_json::to_string(patterns).expect("patterns as JSON");
    child
        .stdin
        .take()
        .expect("the child's stdin")
        .write_all(input.as_bytes())
        .expect("patterns sent");
    let output = child.wait_with_output().expect("the child finishes");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Runs `engine` on `patterns` in a process of its own, returning its row.
fn spawn_engine(engine: Engine, patterns: &[String], samples: &Path) -> String {
    let samples = samples.to_string_lossy();
    match run_child(&[ENGINE_FLAG, engine.name(), &samples], patterns) {
        (true, output) => output.trim().to_owned(),
        (false, _) => "crashed".to_owned(),
    }
}

/// The patterns sent on stdin.
fn read_patterns() -> Vec<String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .expect("patterns on stdin");
    serde_json::from_str(&input).expect("patterns as JSON")
}

/// Measures `engine` on the patterns on stdin, printing its row.
fn run_engine(engine: Engine, samples: &Path) {
    let patterns = read_patterns();
    let lines = sample_lines(samples);
    let measure = measure(engine, &patterns, &lines);
    println!(
        "{:>8.1}ms {:>8.1}ms {:>9} {:>8} {:>7.0}MiB",
        millis(measure.compile),
        millis(measure.search),
        measure.matches,
        measure.gave_up,
        measure.peak_kib as f64 / KIB_PER_MIB
    );
}

/// `patterns` compiled, then searched in every line.
fn measure(engine: Engine, patterns: &[String], lines: &[String]) -> Measure {
    match engine {
        Engine::Fancy => {
            let (regexes, compile) =
                time(|| patterns.iter().filter_map(|p| fancy(p)).collect::<Vec<_>>());
            search_fancy(&regexes, lines, compile)
        }
        Engine::Onig => {
            let (regexes, compile) = time(|| {
                patterns
                    .iter()
                    .filter_map(|p| oniguruma(p))
                    .collect::<Vec<_>>()
            });
            search_onig(&regexes, lines, compile)
        }
        Engine::Reox => {
            let (regexes, compile) =
                time(|| patterns.iter().filter_map(|p| reox(p)).collect::<Vec<_>>());
            search_reox(&regexes, lines, compile)
        }
    }
}

/// Every regex searched in every line by reox.
fn search_reox(regexes: &[reox::Regex], lines: &[String], compile: Duration) -> Measure {
    let (mut matches, mut gave_up) = (0, 0);
    let ((), search) = time(|| {
        for regex in regexes {
            let mut cache = regex.create_cache();
            let mut locations = regex.capture_locations();
            for line in lines {
                match regex.search(&mut cache, line, 0, &mut locations) {
                    Ok(true) => matches += 1,
                    Ok(false) => {}
                    Err(_) => gave_up += 1,
                }
            }
        }
    });
    measured(compile, search, matches, gave_up)
}

/// Every regex searched in every line by fancy-regex.
fn search_fancy(regexes: &[fancy_regex::Regex], lines: &[String], compile: Duration) -> Measure {
    let (mut matches, mut gave_up) = (0, 0);
    let ((), search) = time(|| {
        for regex in regexes {
            for line in lines {
                match regex.captures_from_pos(line, 0) {
                    Ok(Some(_)) => matches += 1,
                    Ok(None) => {}
                    Err(_) => gave_up += 1,
                }
            }
        }
    });
    measured(compile, search, matches, gave_up)
}

/// Every regex searched in every line by Oniguruma.
fn search_onig(regexes: &[onig::Regex], lines: &[String], compile: Duration) -> Measure {
    let (mut matches, mut gave_up) = (0, 0);
    let ((), search) = time(|| {
        let mut region = Region::new();
        for regex in regexes {
            for line in lines {
                let found = regex.search_with_param(
                    line,
                    0,
                    line.len(),
                    SearchOptions::SEARCH_OPTION_NONE,
                    Some(&mut region),
                    MatchParam::default(),
                );
                match found {
                    Ok(Some(_)) => matches += 1,
                    Ok(None) => {}
                    Err(_) => gave_up += 1,
                }
            }
        }
    });
    measured(compile, search, matches, gave_up)
}

/// A measure, with the process's peak memory so far.
fn measured(compile: Duration, search: Duration, matches: usize, gave_up: usize) -> Measure {
    Measure {
        compile,
        gave_up,
        matches,
        peak_kib: peak_memory_kib(),
        search,
    }
}

/// The process's peak memory, in kibibytes; 0 where it is not known.
fn peak_memory_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix(PEAK_MEMORY_FIELD))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap_or(0)
}

/// Every distinct pattern in the grammar JSON files under `dir`.
fn grammar_patterns(dir: &Path) -> Vec<String> {
    let mut patterns = Vec::new();
    for file in files(dir) {
        let is_grammar = file.extension().is_some_and(|ext| ext == "json")
            && file.file_name().is_some_and(|name| {
                name != "package.json" && name != "language-configuration.json"
            });
        if !is_grammar {
            continue;
        }
        let grammar = fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(grammar) = grammar {
            collect_patterns(&grammar, &mut patterns);
        }
    }
    patterns.sort_unstable();
    patterns.dedup();
    patterns
}

/// Adds the patterns `value`, part of a grammar, holds to `patterns`.
fn collect_patterns(value: &Value, patterns: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                match (PATTERN_KEYS.contains(&key.as_str()), field) {
                    (true, Value::String(pattern)) => patterns.push(pattern.clone()),
                    _ => collect_patterns(field, patterns),
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_patterns(item, patterns)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// The first lines of every file in `dir`, each with its newline.
pub(crate) fn sample_lines(dir: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files(dir) {
        if let Ok(text) = fs::read_to_string(&file) {
            lines.extend(
                text.lines()
                    .take(LINES_PER_FILE)
                    .map(|line| format!("{line}\n")),
            );
        }
    }
    lines
}
