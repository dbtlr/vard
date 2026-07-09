default: check

# The fast local gate: fmt, clippy, tests
check: fmt-check lint test

# The full local gate: everything CI enforces, including supply-chain checks
check-all: check doc audit deny

build:
    cargo build --workspace --locked

build-release:
    cargo build -p vard --release --locked

test:
    cargo test --workspace --locked

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Docs gate: rustdoc warnings (e.g. broken intra-doc links) are errors
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Supply-chain: known-vulnerability scan (RustSec advisory DB)
audit:
    cargo audit

# Supply-chain: license, advisory, ban, and source policy (deny.toml)
deny:
    cargo deny check
