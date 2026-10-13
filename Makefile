.PHONY: check rust-check init-check deny msrv fmt clippy test scrub-tree

# Full pre-commit check suite. CI's check job runs rust-check.
check: rust-check deny init-check

rust-check: msrv fmt clippy test

# rust-version claims only the pinned toolchain: it must equal the
# major.minor of rust-toolchain.toml's channel.
msrv:
	@claim=$$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml); \
	pin=$$(sed -n 's/^channel *= *"\([0-9]*\.[0-9]*\).*"/\1/p' rust-toolchain.toml); \
	if [ -z "$$claim" ] || [ "$$claim" != "$$pin" ]; then \
		echo "msrv: Cargo.toml rust-version '$$claim' != rust-toolchain.toml channel major.minor '$$pin'" >&2; \
		exit 1; \
	fi

fmt:
	cargo fmt -- --check

clippy:
	cargo clippy --locked --all-targets -- -D warnings

test:
	cargo test --locked

# Every dependency's license is on deny.toml's allow list.
deny:
	cargo deny --locked check licenses

# The scrub kit (hook shims, .gitleaks.toml rules, overlay gitignore line,
# core.hooksPath) still matches what the installed brenn-scrub expects.
init-check:
	brenn-scrub init --check

scrub-tree:
	brenn-scrub tree
