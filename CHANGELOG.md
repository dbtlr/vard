# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it ships v1.0. Pre-1.0 versions may include breaking changes in minor
releases.

## [Unreleased]

Entries here have landed on `main` but have not yet been cut into a tagged
release. When a release is cut, this section is promoted to
`## v0.X.0 - YYYY-MM-DD` and a fresh `## [Unreleased]` header is added above it.

### Added

- Cargo workspace scaffold: `vard-core` (the embeddable engine) and `vard`
  (the binary), with XDG base-directory resolution for the config, state,
  data, and log paths.
- Minimal `vard` command-line skeleton built on `clap`. Shell completions
  (bash, zsh, fish, nushell) and a manpage are generated from the real CLI
  definitions at build time.
- Release machinery via cargo-dist: a shell installer and tag-driven GitHub
  Releases for macOS and Linux (aarch64 and x86_64), bundling the generated
  completions and manpage. Release notes are drawn from this changelog.
- Continuous integration: formatting, clippy (`-D warnings`), the test suite,
  `cargo audit`, `cargo deny`, and install/completion/manpage smoke checks.
