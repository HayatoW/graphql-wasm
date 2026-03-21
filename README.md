# graphql-wasm

Cloudflare Workers にデプロイ可能な **Rust / WASM** の GraphQL API です。ルーティングは **Axum** (`default-features = false`、Tokio ランタイム機能なし)、スキーマは **async-graphql** です。

## 前提

- [Rust](https://www.rust-lang.org/) (`rust-toolchain.toml` どおり **1.88**)
- `wasm32-unknown-unknown` ターゲット: `rustup target add wasm32-unknown-unknown --toolchain 1.88.0`
- [worker-build](https://github.com/cloudflare/workers-rs) (`cargo install worker-build`、ビルドには OpenSSL 開発パッケージが必要な場合あり)
- [Node.js](https://nodejs.org/) + [Wrangler](https://developers.cloudflare.com/workers/wrangler/) (`npx wrangler` で可)

## スキーマ (ToDo)

| 種別 | フィールド | 説明 |
|------|------------|------|
| Query | `todos` | 一覧 |
| Mutation | `createTodo(title)` | 作成 |
| Mutation | `toggleTodo(id)` | 完了フラグ反転 |
| Mutation | `deleteTodo(id)` | 削除 |
| Subscription | `todoListUpdates` | 現在の一覧スナップショットを最大 8 件ストリーム (HTTP では下記 Accept 必須) |

ストアは **Isolate 内メモリ** (`std::sync::Mutex`) です。本番では D1 / KV 等に差し替えてください。

## Tokio について

アプリ側の Axum は **Tokio 機能を有効にしていません**。`async-graphql` も `default-features = false` で **tokio-sync 等はオフ**です。  
なお Cloudflare 公式の [`worker`](https://crates.io/crates/worker) クレートは内部で `tokio` (`default-features = false`) を参照します。これは Workers 向け WASM ビルドで解決済みの構成です。

## ローカル実行

```bash
# wrangler CLI をインストール
npm install
```

```bash
worker-build --release   # または wrangler dev が自動実行
npx wrangler dev
```

ブラウザで `http://localhost:8787/` の簡易プレイグラウンド、または:

```bash
# クエリ
curl -sS -X POST http://localhost:8787/graphql \
  -H 'Content-Type: application/json' \
  -d '{"query":"query { todos { id title done } }"}'

# ミューテーション
curl -sS -X POST http://localhost:8787/graphql \
  -H 'Content-Type: application/json' \
  -d '{"query":"mutation { createTodo(title: \"Rust\") { id title done } }"}'
```

サブスクリプション ([Apollo multipart プロトコル](https://www.apollographql.com/docs/router/executing-operations/subscription-multipart-protocol/)):

```bash
curl -sS -X POST http://localhost:8787/graphql \
  -H 'Content-Type: application/json' \
  -H 'Accept: multipart/mixed; boundary="graphql"; subscriptionSpec="1.0"' \
  -d '{"query":"subscription { todoListUpdates { id title done } }"}'
```

## デプロイ

```bash
npx wrangler deploy
```

Cloudflare アカウントの認証が必要です。<br>
dev container でやる場合は以下を確認
- https://developers.cloudflare.com/workers/wrangler/commands/general/#use-wrangler-login-on-a-remote-machine


## Wrangler の警告について

Wrangler 4 では `[build.upload]` が非推奨扱いの場合があり、設定警告が出ることがあります。`wrangler dev` / デプロイは動作することを確認済みです。公式テンプレートに合わせて `[rules]` 等へ移行する場合は [Wrangler の設定ドキュメント](https://developers.cloudflare.com/workers/wrangler/configuration/) を参照してください。
