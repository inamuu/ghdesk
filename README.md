# ghdesk

GitHub の PR / Issue をターミナル全画面で横断する TUI です。

検索はバックグラウンドで実行されるため、カテゴリ切替やフィルタ変更中も UI が止まりにくくなっています。

## 機能

- `私が作成したPR`
- `私が作成したIssue`
- `私がアサインされたPR`
- `私がアサインされたIssue`
- GitHub 検索クエリの追加編集
- organization フィルタの選択
- Pull Request 作成画面
- `Open / Closed / All` の状態フィルタ
- 右ペインでのプレビュー
- 選択項目をブラウザで開く
- URL / 番号のクリップボードコピー

## 前提

- `gh` CLI がインストール済み
- `gh auth login` または `gh auth status` が通ること

## 起動

```bash
cargo run
```

リリースビルド:

```bash
cargo run --release
```

## キーバインド

- `Tab` / `Shift+Tab`: カテゴリ切替
- `j` / `k` または `↓` / `↑`: 移動
- `n`: Pull Request 作成画面
- `e` または `/`: フィルタクエリ編集
- `a`: organization フィルタ編集
- `s`: `Open -> Closed -> All` を切替
- `r`: 再取得
- `Enter` または `o`: ブラウザで開く
- `PageUp` / `PageDown`: プレビューをスクロール
- `J` / `K`: プレビューを少しだけスクロール
- `Command+Shift+,` または `<`: URL をコピー
- `Command+Shift+.` または `>`: 番号をコピー
- `Esc` または `q`: 終了
- `Ctrl+C`: 終了
- `Command+W`: 終了

## Pull Request 作成

- `n` で作成画面を開く
- 項目は `タイトル` `本文` `Draft`
- Draft はデフォルトで `ON`
- `Tab` / `Shift+Tab`: 項目移動
- `Space`: Draft 切替
- `Ctrl+S`: 作成
- `Esc`: キャンセル

## クエリ例

- `repo:owner/name`
- `org:your-org`
- `label:bug`
- `sort:updated-desc`
- `is:draft`

## 補足

ターミナルエミュレータによっては `Command` 修飾キーがアプリへ渡らないため、その場合は `<` / `>` を使ってください。
プレビュー領域上でマウスホイールを回すと、プレビュー本文をスクロールできます。
