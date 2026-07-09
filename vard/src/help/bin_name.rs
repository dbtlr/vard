//! Single source of truth for the binary's user-facing name.
//!
//! Reads from `CARGO_BIN_NAME` so a rename is a one-line change in `Cargo.toml`
//! rather than a project-wide string sweep. `CARGO_BIN_NAME` is only defined
//! when compiling a binary, so any non-binary build (e.g. unit tests) falls
//! back to the product name `vard` — the command users type regardless of which
//! binary is running.

pub const BIN_NAME: &str = match option_env!("CARGO_BIN_NAME") {
    Some(name) => name,
    None => "vard",
};
