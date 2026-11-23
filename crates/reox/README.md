# reox

Regex, oxidized. reox (say "ree-ox") is its own regex engine in pure Rust, with the features the `regex` crate leaves out: lookaround, back-references, atomic groups, and subroutine calls. It reads Oniguruma's syntax and matches as Oniguruma does, groups included, so TextMate grammars and Ruby patterns work as written.

- **Never hangs.** Backtracking is memoized and bounded, so a runaway pattern finishes or gives up instead of stalling.
- **Cheap to compile.** A pattern becomes a short program, with no automata built up front, so loading thousands of patterns costs little.
- **Fast on long text.** Searches skip ahead to where a match can start, and a pattern that has scanned enough text switches to automata from `regex-automata`.

```toml
[dependencies]
reox = "0.0.1"
```

```rust
use reox::Regex;

let regex = Regex::new(r"(?<!\w)(?<name>\w+)(?=\()").unwrap();
let captures = regex.captures("x = call(1)").unwrap().unwrap();
assert_eq!(captures.name("name").unwrap().as_str(), "call");
```

## Status

Early. Supported:

- Oniguruma's syntax: line anchors, `\A`, `\z`, `\Z`, `\G`, Unicode POSIX classes and properties, `(?imx)` flags, lookahead and lookbehind (unbounded too), atomic groups, possessive quantifiers, back-references, named groups, subroutine calls (`\g<name>`, recursive too), `\K`, `\R`, and `\X`.
- Case folding to several characters, both ways: `ß` matches `ss` and `ss` matches `ß`.
- Searching from a position, with `\G` anchored there, allocating nothing once warm.
- `RegexSet`: several patterns searched as one, returning the leftmost match, ties going to the first pattern, as a TextMate tokenizer needs.

Not yet: absent operators (`(?~...)`), conditionals, text segment boundaries (`\y`, `\Y`), and back-references to a recursion level (`\k<name+1>`).

## License

MIT or Apache-2.0.
