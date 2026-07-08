default: check

# Everything CI runs, locally
check: fmt-check lint test

build:
    cargo build --workspace --locked

test:
    cargo test --workspace --locked

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings
