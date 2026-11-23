# reox

Regex, oxidized: a pure-Rust regex engine with lookaround and back-references, in Oniguruma's syntax. This README is for working on reox. For what it does and how to use it, see the [crate's README](crates/reox/README.md), which is also its crates.io page.

## Layout

- [`crates/reox`](crates/reox): the published crate, its sources grouped by stage (`syntax`, `backtrack`, `accel`, `regex`).
- [`crates/reox-bench`](crates/reox-bench): the benchmark, a crate of its own so reox never depends on the engines it is measured against. Not published.
- [`fuzz`](fuzz): the differential fuzzer, a workspace of its own, built on nightly by `cargo-fuzz`.

## Testing

```bash
make                        # lint, test, doc
make test                   # cargo test --workspace --release
make fuzz FUZZ_SECONDS=300  # needs nightly and cargo-fuzz
```

- [`tests/oniguruma.rs`](crates/reox/tests/oniguruma.rs) checks reox against Oniguruma through the `onig` crate: every pattern, text, start position, and group span, on both the backtracker and forced automata. Sets are checked against their patterns searched alone.
- [`tests/oniguruma_suite.rs`](crates/reox/tests/oniguruma_suite.rs) runs Oniguruma 6.9.10's own `test_utf8.c`, vendored with its license. Of its 1,522 cases, 1,362 pass and 160 use features reox does not support yet; none fail.
- [`fuzz`](fuzz) compares reox with Oniguruma on random patterns and texts. It skips what Oniguruma itself answers inconsistently: back-references inside their own group, and calls, captures, word boundaries, and `\R` inside lookbehinds.

## Benchmarks

`reox-bench` compares reox with fancy-regex and Oniguruma. The grammar and tokenizer workloads read TextMate grammars and sample files from an [iris](https://github.com/taquangtrung/iris) checkout next to this one; pass other folders as arguments. regex-redux and ReDoS need nothing.

```bash
cargo run --release -p reox-bench -- [grammars] [samples]
cargo run --release -p reox-bench -- tokenize [samples]
cargo run --release -p reox-bench -- redux
cargo run --release -p reox-bench -- redos
```

### TextMate grammars

iris's 137 grammars searched in VS Code's colorize fixtures (7,117 patterns, 2,104 lines), each engine in its own memory-capped process:

| Engine | Compile | Search | Peak memory |
| --- | --- | --- | --- |
| reox | 169 ms | 4.1 s | 53 MiB |
| fancy-regex | 1,406 ms | 12.5 s | 545 MiB |
| Oniguruma | 171 ms | 8.8 s | 56 MiB |

reox matches as Oniguruma does wherever Oniguruma answers; its 18 extra matches are on lines where Oniguruma gives up. Patterns an engine rejects are left out of every engine's numbers (of 7,295, reox rejects 70 and Oniguruma 66), as is one 14 KB pattern that runs fancy-regex out of memory.

### Other workloads

| Workload | reox | fancy-regex | Oniguruma |
| --- | --- | --- | --- |
| cl100k pre-tokenizer, 4 MiB | 82 ms | 108 ms | 146 ms |
| regex-redux, 2.4 MiB | 37 ms | 31 ms | 363 ms |

- regex-redux is literal and class scanning over long text: reox runs it on automata once hot, and fancy-regex hands it to the `regex` crate.
- On nine patterns known to backtrack catastrophically, reox and fancy-regex answer all nine, mostly in microseconds, where Oniguruma gives up on seven. reox's slowest is 0.3 ms, on the one with a back-reference.

### rebar

A one-off snapshot with [rebar](https://github.com/BurntSushi/rebar), through runners for reox and fancy-regex that neither rebar nor this repository keeps. Compared with fancy-regex and the `regex` crate:

- Compiling: reox is fastest on all but one benchmark, up to 24 times faster than `regex`, and 1.3 times slower on that one (an ASCII date pattern).
- Literal and alternation scanning: 0.7 to 1.2 times `regex`'s speed.
- Faster than both on a lexer and on Russian words.
- 5 to 490 times slower than `regex` where a pattern keeps it on the backtracker over long text: the ReDoS, Unicode date, ruff noqa, dictionary, and quadratic benchmarks.
- Multi-pattern benchmarks don't run yet, since the runner searches one pattern at a time.

## Releasing

Set the version in the root `Cargo.toml`, add a `## <version>` section to `CHANGELOG.md`, update `reox = "<version>"` in the crate's README, and commit. Then:

```bash
make pre-publish  # the checks, the package's files, and a dry-run publish
make publish      # the same checks, then publish, tag v<version>, and push the tag
```

Both refuse a dirty tree, a missing changelog section, a stale README version, or an existing tag.

## License

MIT or Apache-2.0.
