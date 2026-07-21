日本語 | [English](README.md)

# CC-Tiles

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/WaTeR-7/cctiles/actions/workflows/ci.yml/badge.svg)](https://github.com/WaTeR-7/cctiles/actions/workflows/ci.yml)

> 🚧 **開発中です。** CC-Tiles はまだ設計・雛形作成の段階で、以下に書かれている機能の多くは未実装です。

[Claude Code](https://github.com/anthropics/claude-code) の CUI セッションを並列で実行・監視するための TUI（テキストユーザーインターフェース）アプリです。Rust で実装しています。

## やりたいこと（計画中）

- `cctiles` を起動すると、まず画面分割数などを設定するセットアップ画面が表示されます。
- 実行すると、`tmux` のようなイメージで画面が分割されたレイアウト（例: 2x3）が立ち上がります。
- 分割された画面の1つ1つを **タイル** と呼びます。
- 各タイルは、対応する Claude Code セッションが出力する `.jsonl` を監視し、今そのセッションが何をしているのかの概要をリアルタイムに表示します。
- ユーザーの許可待ち・質問への回答待ちになっているタイルは色が変わり、一目で分かるようになります。
- タイルを選択して `ENTER` を押すと、フロートターミナルが開き、通常の Claude Code CUI としてそのセッションを操作できます。

## 現在の状況

現時点ではリポジトリの雛形（ライセンス、CI、コントリビューションガイドなど）のみが整備されている段階で、実装はまだ始まっていません。今後の予定は Issue を参照してください。

## インストール

まだ公開していません。最初のバージョンが用意でき次第、以下のようにインストールできるようにする予定です。

```sh
cargo install cctiles
```

## Contributing

コントリビューション・バグ報告・アイデアを歓迎します。プルリクエストを送る前に [CONTRIBUTING.md](CONTRIBUTING.md)（英語）をご確認ください。また、本プロジェクトへの参加には [Code of Conduct](CODE_OF_CONDUCT.md)（英語）が適用されます。

## ライセンス

以下のいずれかのライセンスの下で提供されます（選択可）。

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
