# 開発ガイド

## 必要なもの

- Rust と Cargo（Rust 1.91 で動作確認）
- Mastodon のアカウントと、HTTPS で接続できるサーバー
- Linux で画像を貼り付ける場合は Wayland の `wl-paste` または X11 の `xclip`（macOS は未対応）
- 外部アプリでリンクやメディアを開く場合は Linux の `xdg-open` または macOS の `open`

## ソースからの起動

```sh
git clone https://github.com/hama6767/LazyDon.git
cd LazyDon
cargo run --release
```

ネットワーク要求はバックグラウンドで実行します。実際の投稿・お気に入り・ブーストは接続先サーバーに反映されます。

## テストとビルド

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
```

## ビルドと Release

GitHub Actions は `main` への push と pull request ごとに Linux x86_64、Linux ARM64（いずれも Ubuntu 22.04）、macOS Apple Silicon 用のテストとリリースビルドを実行し、アーカイブと SHA-256 ファイルを成果物に保存します。GitHub Release の公開時には、`main` の最新コミットに対応する 3 種類すべての成功済みビルドを添付します。ビルドが実行中なら完了を待ち、成果物が期限切れの場合は `main` のビルドを再実行します。Release のタグが指すコミットとは異なる場合があるため、同梱の `BUILD.txt` でソースのコミットを確認できます。GitHub はタグ側のワークフロー定義を使うため、Release のタグはこのワークフローが含まれるコミット以降に作成してください。
