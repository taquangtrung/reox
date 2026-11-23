//! Benchmarks beyond grammars: a tokenizer's pre-tokenizing, regex-redux,
//! and patterns known to backtrack catastrophically.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::engines::{Engine, Finder, GaveUp};
use crate::grammars::DEFAULT_SAMPLES;
use crate::{files, millis, time};

// ========================================================================
// Constants
// ========================================================================

/// The pre-tokenizer pattern of OpenAI's cl100k tokenizer.
const CL100K: &str = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";

/// The least text the tokenizer runs over, the samples repeated to reach it.
const TOKENIZE_BYTES: usize = 4 << 20;

/// Bytes in a mebibyte.
const BYTES_PER_MIB: f64 = (1 << 20) as f64;

/// The regex-redux input's size: the benchmark's `n`.
const REDUX_N: usize = 250_000;

/// The patterns regex-redux counts the matches of.
const REDUX_VARIANTS: [&str; 9] = [
    "agggtaaa|tttaccct",
    "[cgt]gggtaaa|tttaccc[acg]",
    "a[act]ggtaaa|tttacc[agt]t",
    "ag[act]gtaaa|tttac[agt]ct",
    "agg[act]taaa|ttta[agt]cct",
    "aggg[acg]aaa|ttt[cgt]ccct",
    "agggt[cgt]aa|tt[acg]accct",
    "agggta[cgt]a|t[acg]taccct",
    "agggtaa[cgt]|[acg]ttaccct",
];

/// The replacements regex-redux makes, in order.
const REDUX_REPLACEMENTS: [(&str, &str); 5] = [
    ("tHa[Nt]", "<4>"),
    ("aND|caN|Ha[DS]|WaS", "<3>"),
    ("a[NSt]|BY", "<2>"),
    ("<[^>]*>", "|"),
    (r"\|[^|][^|]*\|", "-"),
];

/// The Benchmarks Game's random generator: modulus, multiplier, increment,
/// and seed.
const RANDOM: (u32, u32, u32, u32) = (139_968, 3_877, 29_573, 42);

/// How many characters a FASTA line holds.
const FASTA_LINE: usize = 60;

/// The Alu sequence the FASTA input repeats.
const ALU: &str = "GGCCGGGCGCGGTGGCTCACGCCTGTAATCCCAGCACTTTGGGAGGCCGAGGCGGGCGGATCACCTGAGGTCAGGAGTTCGAGACCAGCCTGGCCAACATGGTGAAACCCCGTCTCTACTAAAAATACAAAAATTAGCCGGGCGTGGTGGCGCGCGCCTGTAATCCCAGCTACTCGGGAGGCTGAGGCAGGAGAATCGCTTGAACCCGGGAGGCGGAGGTTGCAGTGAGCCGAGATCGCGCCACTGCACTCCAGCCTGGGCGACAGAGCGAGACTCCGTCTCAAAAA";

/// The ambiguity codes the FASTA input draws from, with their odds.
const IUB: [(u8, f64); 15] = [
    (b'a', 0.27),
    (b'c', 0.12),
    (b'g', 0.12),
    (b't', 0.27),
    (b'B', 0.02),
    (b'D', 0.02),
    (b'H', 0.02),
    (b'K', 0.02),
    (b'M', 0.02),
    (b'N', 0.02),
    (b'R', 0.02),
    (b'S', 0.02),
    (b'V', 0.02),
    (b'W', 0.02),
    (b'Y', 0.02),
];

/// The nucleotides the FASTA input draws from, with human odds.
const HOMO_SAPIENS: [(u8, f64); 4] = [
    (b'a', 0.302_954_942_668_7),
    (b'c', 0.197_988_352_643_5),
    (b'g', 0.197_547_910_983_5),
    (b't', 0.301_508_783_704_3),
];

/// Patterns known to backtrack catastrophically, with the text that makes
/// them: the subject repeated, then the tail.
const REDOS: [(&str, &str, usize, &str); 9] = [
    (r"^(a+)+$", "a", 30, "!"),
    (r"^(a|a)*$", "a", 30, "!"),
    (r"^(a|aa)+$", "a", 40, "!"),
    (r"^(x+x+)+y$", "x", 30, ""),
    (r"^(\w+\s?)*$", "a", 30, "!"),
    (r"(\d+)*$", "1", 30, "a"),
    (r"^(([a-z])+.)+[A-Z]([a-z])+$", "a", 30, "!"),
    (
        r"^([a-zA-Z0-9])(([\-.]|[_]+)?([a-zA-Z0-9]+))*(@){1}[a-z0-9]+[.]{1}(([a-z]{2,3})|([a-z]{2,3}[.]{1}[a-z]{2,3}))$",
        "a",
        30,
        "!",
    ),
    (r"^(a+)+\1$", "a", 30, "!"),
];

// ========================================================================
// Functions
// ========================================================================

/// Runs cl100k's pre-tokenizer over the sample files `args` names, or
/// iris's colorize fixtures.
pub(crate) fn tokenize(args: &[String]) {
    let samples = PathBuf::from(args.first().map_or(DEFAULT_SAMPLES, String::as_str));
    let mut sample = String::new();
    for file in files(&samples) {
        sample.push_str(&fs::read_to_string(&file).unwrap_or_default());
    }
    if sample.is_empty() {
        println!("no sample text in {}", samples.display());
        return;
    }
    let text = sample.repeat(TOKENIZE_BYTES.div_ceil(sample.len()));
    println!(
        "cl100k pre-tokenizer over {:.1} MiB from {}\n",
        text.len() as f64 / BYTES_PER_MIB,
        samples.display()
    );
    println!(
        "{:<12} {:>10} {:>10} {:>12}",
        "engine", "time", "MiB/s", "tokens"
    );
    for engine in Engine::ALL {
        let Some(mut finder) = engine.finder(CL100K) else {
            println!("{:<12} does not compile", engine.name());
            continue;
        };
        let (tokens, elapsed) = time(|| count_matches(finder.as_mut(), &text));
        match tokens {
            Ok(tokens) => println!(
                "{:<12} {:>8.1}ms {:>10.1} {:>12}",
                engine.name(),
                millis(elapsed),
                text.len() as f64 / BYTES_PER_MIB / elapsed.as_secs_f64(),
                tokens
            ),
            Err(GaveUp) => println!("{:<12} gave up", engine.name()),
        }
    }
}

/// Runs regex-redux on a generated DNA sequence.
pub(crate) fn redux() {
    let input = fasta(REDUX_N);
    println!(
        "regex-redux over {:.1} MiB of FASTA (n = {REDUX_N})\n",
        input.len() as f64 / BYTES_PER_MIB
    );
    println!(
        "{:<12} {:>10}  lengths (input, cleaned, replaced) and counts",
        "engine", "time"
    );
    for engine in Engine::ALL {
        let (result, elapsed) = time(|| redux_on(engine, &input));
        match result {
            Some(summary) => println!(
                "{:<12} {:>8.1}ms  {summary}",
                engine.name(),
                millis(elapsed)
            ),
            None => println!("{:<12} failed", engine.name()),
        }
    }
}

/// Runs patterns known to backtrack catastrophically on the texts that
/// make them, with every engine.
pub(crate) fn redos() {
    println!("patterns that backtrack catastrophically; m = match, - = none, X = gave up\n");
    print!("{:<40}", "pattern");
    for engine in Engine::ALL {
        print!(" {:>14}", engine.name());
    }
    println!();
    for (pattern, subject, repeat, tail) in REDOS {
        let text = format!("{}{tail}", subject.repeat(repeat));
        let shown: String = pattern.chars().take(38).collect();
        print!("{shown:<40}");
        for engine in Engine::ALL {
            let Some(mut finder) = engine.finder(pattern) else {
                print!(" {:>14}", "rejects");
                continue;
            };
            let (found, elapsed) = time(|| finder.find(&text, 0));
            let mark = match found {
                Ok(Some(_)) => "m",
                Ok(None) => "-",
                Err(GaveUp) => "X",
            };
            print!(" {:>11} {mark}", short_duration(elapsed));
        }
        println!();
    }
}

/// regex-redux with `engine` on `input`: its lengths and counts, as the
/// benchmark prints them, on one line; `None` when a pattern does not
/// compile or a search gives up.
fn redux_on(engine: Engine, input: &str) -> Option<String> {
    let cleaned = replace_all(engine.finder(">.*\n|\n")?.as_mut(), input, "").ok()?;
    let mut counts = Vec::new();
    for variant in REDUX_VARIANTS {
        counts.push(count_matches(engine.finder(variant)?.as_mut(), &cleaned).ok()?);
    }
    let mut replaced = cleaned.clone();
    for (pattern, replacement) in REDUX_REPLACEMENTS {
        replaced = replace_all(engine.finder(pattern)?.as_mut(), &replaced, replacement).ok()?;
    }
    let counts: Vec<String> = counts.iter().map(usize::to_string).collect();
    Some(format!(
        "{} {} {}  [{}]",
        input.len(),
        cleaned.len(),
        replaced.len(),
        counts.join(" ")
    ))
}

/// How many matches `finder` finds in `text`, one after another.
fn count_matches(finder: &mut dyn Finder, text: &str) -> Result<usize, GaveUp> {
    let mut count = 0;
    let mut at = 0;
    while at <= text.len() {
        let Some((start, end)) = finder.find(text, at)? else {
            break;
        };
        count += 1;
        at = next_start(text, start, end);
    }
    Ok(count)
}

/// `text` with every match `finder` finds replaced by `replacement`.
fn replace_all(finder: &mut dyn Finder, text: &str, replacement: &str) -> Result<String, GaveUp> {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while at <= text.len() {
        let Some((start, end)) = finder.find(text, at)? else {
            break;
        };
        out.push_str(&text[copied..start]);
        out.push_str(replacement);
        copied = end;
        at = next_start(text, start, end);
    }
    out.push_str(&text[copied..]);
    Ok(out)
}

/// Where the search after a match from `start` to `end` goes on: past an
/// empty match's next character, so it never finds it again.
fn next_start(text: &str, start: usize, end: usize) -> usize {
    match end > start {
        true => end,
        false => end + text[end..].chars().next().map_or(1, char::len_utf8),
    }
}

/// The Benchmarks Game's FASTA output for `n`: Alu repeated, then random
/// ambiguity codes, then random nucleotides.
fn fasta(n: usize) -> String {
    let mut out = String::new();
    let mut seed = RANDOM.3;
    out.push_str(">ONE Homo sapiens alu\n");
    let alu: Vec<u8> = ALU.bytes().cycle().take(n * 2).collect();
    push_lines(&mut out, &alu);
    out.push_str(">TWO IUB ambiguity codes\n");
    let iub = random_sequence(&IUB, n * 3, &mut seed);
    push_lines(&mut out, &iub);
    out.push_str(">THREE Homo sapiens frequency\n");
    let homo = random_sequence(&HOMO_SAPIENS, n * 5, &mut seed);
    push_lines(&mut out, &homo);
    out
}

/// `len` characters drawn from `table` by the benchmark's generator.
fn random_sequence(table: &[(u8, f64)], len: usize, seed: &mut u32) -> Vec<u8> {
    let (modulus, multiplier, increment, _) = RANDOM;
    let mut cumulative = Vec::with_capacity(table.len());
    let mut total = 0.0;
    for &(byte, odds) in table {
        total += odds;
        cumulative.push((byte, total));
    }
    (0..len)
        .map(|_| {
            *seed = (*seed * multiplier + increment) % modulus;
            let draw = f64::from(*seed) / f64::from(modulus);
            cumulative
                .iter()
                .find(|&&(_, odds)| draw < odds)
                .map_or(table[table.len() - 1].0, |&(byte, _)| byte)
        })
        .collect()
}

/// Appends `sequence` to `out` in FASTA lines.
fn push_lines(out: &mut String, sequence: &[u8]) {
    for line in sequence.chunks(FASTA_LINE) {
        out.push_str(std::str::from_utf8(line).expect("ASCII"));
        out.push('\n');
    }
}

/// `duration` written short: microseconds or milliseconds.
fn short_duration(duration: Duration) -> String {
    match duration.as_micros() {
        micros @ 0..1_000 => format!("{micros}us"),
        _ => format!("{:.1}ms", millis(duration)),
    }
}
