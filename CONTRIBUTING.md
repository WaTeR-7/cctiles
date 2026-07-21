# Contributing to CC-Tiles

Thanks for your interest in contributing! This project is in an early stage, so
expect things to move and change quickly.

## Reporting bugs / requesting features

Please open a [GitHub Issue](https://github.com/WaTeR-7/cctiles/issues) using
the appropriate template. Include as much detail as you can: what you expected,
what happened, and steps to reproduce (for bugs).

## Development setup

You'll need a recent stable Rust toolchain (see `rust-toolchain.toml` for the
pinned version, installed automatically via `rustup`).

```sh
git clone https://github.com/WaTeR-7/cctiles.git
cd cctiles
cargo build
```

Before opening a pull request, please make sure the following all pass locally:

```sh
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
```

## Submitting changes

1. Fork the repository and create a branch off `main`.
2. Make your changes, with tests where it makes sense.
3. Make sure `cargo fmt`, `cargo clippy`, and `cargo test` all pass.
4. Open a pull request describing what changed and why.

Small, focused pull requests are easier to review than large ones — if you're
planning a big change, consider opening an issue first to discuss the approach.

## Code of Conduct

This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). By
participating, you agree to abide by it.
