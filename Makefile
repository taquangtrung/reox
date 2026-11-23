# Everyday tasks; CI runs the same targets.

# How long `make fuzz` fuzzes, in seconds.
FUZZ_SECONDS ?= 60

# The longest input the fuzzer tries, in bytes.
FUZZ_MAX_LEN ?= 256

.PHONY: all lint test doc fuzz bench release-checks pre-publish publish

all: lint test doc

# Formatting and lints, warnings failing.
lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

# Every test, reox's against Oniguruma included, in release: the
# differential tests search every pattern from every position.
test:
	cargo test --workspace --release

# The documentation, warnings failing.
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# The differential fuzzer, on nightly, for FUZZ_SECONDS.
fuzz:
	cargo +nightly fuzz run differential -- -max_total_time=$(FUZZ_SECONDS) -max_len=$(FUZZ_MAX_LEN)

# The grammar benchmark, which needs an iris checkout next to this one.
bench:
	cargo run --release -p reox-bench

# The version being released, read from the workspace Cargo.toml.
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

# What both pre-publish and publish check first: a clean tree, a CHANGELOG
# section for VERSION, the crate README asking for VERSION, no tag for it
# yet, then the lints, the tests, and the docs.
release-checks:
	@test -z "$$(git status --porcelain)" || \
		{ echo "release: commit or stash the changes first"; exit 1; }
	@grep -qxF '## $(VERSION)' CHANGELOG.md || \
		{ echo "release: add a '## $(VERSION)' section to CHANGELOG.md"; exit 1; }
	@grep -qxF 'reox = "$(VERSION)"' crates/reox/README.md || \
		{ echo "release: set 'reox = \"$(VERSION)\"' in crates/reox/README.md"; exit 1; }
	@! git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null || \
		{ echo "release: tag v$(VERSION) exists; bump the version in Cargo.toml"; exit 1; }
	$(MAKE) all

# Everything a release must pass, without uploading: the release checks,
# the files the package holds, and a dry-run publish of it.
pre-publish: release-checks
	cargo package -p reox --list
	cargo publish -p reox --dry-run

# Uploads VERSION to crates.io once the release checks pass and you confirm,
# then tags the commit and pushes the tag. No dry run first: `cargo publish`
# packages and builds the crate before it uploads. A published version
# cannot be replaced, only yanked.
publish: release-checks
	@printf 'Publish reox %s to crates.io and push tag v%s? [y/N] ' \
		"$(VERSION)" "$(VERSION)"; \
		read answer; \
		case "$$answer" in \
			[yY]|[yY][eE][sS]) ;; \
			*) echo "publish: cancelled"; exit 1 ;; \
		esac
	@cargo publish -p reox
	@git tag -a "v$(VERSION)" -m "reox $(VERSION)"
	@git push origin "v$(VERSION)"
