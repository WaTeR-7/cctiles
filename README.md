[日本語](README.ja.md) | English

# CC-Tiles

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/WaTeR-7/cctiles/actions/workflows/ci.yml/badge.svg)](https://github.com/WaTeR-7/cctiles/actions/workflows/ci.yml)

> 🚧 **Work in progress.** CC-Tiles is in the early design/scaffolding stage — most features described below are not implemented yet.

A terminal UI (TUI) application for running and monitoring multiple [Claude Code](https://github.com/anthropics/claude-code) CUI sessions in parallel, written in Rust.

## What it does (planned)

- Launching `cctiles` first shows a setup screen where you choose things like the grid layout (e.g. number of panes).
- Running it then opens a split-screen layout (e.g. 2x3), similar to `tmux`.
- Each pane in the grid is called a **tile**.
- Each tile watches the `.jsonl` transcript that Claude Code writes out, and shows a live summary of what that session is currently doing.
- A tile changes color when its session is waiting on user permission or waiting for an answer to a question, so you can spot it at a glance.
- Selecting a tile and pressing `ENTER` opens a floating terminal where you can interact with that session as a normal Claude Code CUI.

## Status

This repository currently contains only project scaffolding (license, CI, contribution guidelines, etc.). Implementation has not started yet. See open issues for the current roadmap.

## Installation

Not published yet. Once a first version is available, it will be installable via:

```sh
cargo install cctiles
```

## Contributing

Contributions, bug reports, and ideas are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request, and note that participation in this project is governed by our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
