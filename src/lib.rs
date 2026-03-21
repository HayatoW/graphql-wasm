//! Axum + async-graphql を Cloudflare Workers (WASM) 上で動かす ToDo API。
//! ストアは Isolate 内のメモリ (本番では D1 / KV 等への置き換えを想定)。

use async_graphql::http::{
    create_multipart_mixed_stream, is_accept_multipart_mixed, MultipartOptions,
};
use async_graphql::{Context, Object, Request as GqlRequest, Schema, SimpleObject, Subscription};
use async_graphql_parser::types::{DocumentOperations, OperationType};
use async_graphql_value::Name;
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::header::{self, HeaderValue};
use axum::http::{Method, Request as AxumRequest, Response as AxumResponse, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::io::Cursor as AsyncCursor;
use futures_util::stream;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::sync::{Mutex, OnceLock};
use tower_service::Service;
use worker::{event, Env, HttpRequest, Result};

/// アプリ全体で共有する ToDo ストア (Worker Isolate 単位)。
type TodoStore = std::sync::Arc<Mutex<Vec<Todo>>>;

#[derive(Clone, SimpleObject)]
struct Todo {
    id: u32,
    title: String,
    done: bool,
}

struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn todos(&self, ctx: &Context<'_>) -> Vec<Todo> {
        let store = ctx.data::<TodoStore>().expect("TodoStore");
        store.lock().expect("lock").clone()
    }
}

struct MutationRoot;

#[Object]
impl MutationRoot {
    async fn create_todo(&self, ctx: &Context<'_>, title: String) -> Todo {
        let store = ctx.data::<TodoStore>().expect("TodoStore");
        let mut g = store.lock().expect("lock");
        let id = g.iter().map(|t| t.id).max().unwrap_or(0).saturating_add(1);
        let todo = Todo {
            id,
            title,
            done: false,
        };
        g.push(todo.clone());
        todo
    }

    async fn toggle_todo(&self, ctx: &Context<'_>, id: u32) -> async_graphql::Result<Todo> {
        let store = ctx.data::<TodoStore>().expect("TodoStore");
        let mut g = store.lock().expect("lock");
        let t = g
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| async_graphql::Error::new("Todo が見つかりません"))?;
        t.done = !t.done;
        Ok(t.clone())
    }

    async fn delete_todo(&self, ctx: &Context<'_>, id: u32) -> bool {
        let store = ctx.data::<TodoStore>().expect("TodoStore");
        let mut g = store.lock().expect("lock");
        let len = g.len();
        g.retain(|t| t.id != id);
        g.len() < len
    }
}

struct SubscriptionRoot;

#[Subscription]
impl SubscriptionRoot {
    /// 定期的に現在の ToDo 一覧をプッシュ (multipart サブスクリプション経由で利用)。
    /// `impl Stream + Send` だと derive が最後の `Send` だけを拾う既知の挙動があるため、具象型で返す。
    async fn todo_list_updates(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<stream::Iter<std::vec::IntoIter<Vec<Todo>>>> {
        let store = ctx.data::<TodoStore>().expect("TodoStore").clone();
        let snapshots: Vec<Vec<Todo>> = (0..8)
            .map(|_| store.lock().expect("lock").clone())
            .collect();
        Ok(stream::iter(snapshots))
    }
}

type AppSchema = Schema<QueryRoot, MutationRoot, SubscriptionRoot>;

fn todo_store() -> TodoStore {
    static STORE: OnceLock<TodoStore> = OnceLock::new();
    STORE
        .get_or_init(|| std::sync::Arc::new(Mutex::new(Vec::new())))
        .clone()
}

fn app_schema() -> AppSchema {
    static SCHEMA: OnceLock<AppSchema> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
                .data(todo_store())
                .finish()
        })
        .clone()
}

#[derive(Clone)]
struct AppState {
    schema: AppSchema,
}

fn operation_is_subscription(request: &GqlRequest) -> bool {
    let Ok(doc) = async_graphql_parser::parse_query(&request.query) else {
        return false;
    };
    match &doc.operations {
        DocumentOperations::Single(op) => op.node.ty == OperationType::Subscription,
        DocumentOperations::Multiple(map) => {
            let Some(name) = &request.operation_name else {
                return false;
            };
            let key = Name::new(name);
            map.get(&key)
                .map(|op| op.node.ty == OperationType::Subscription)
                .unwrap_or(false)
        }
    }
}

fn apply_cors<B>(mut res: AxumResponse<B>) -> AxumResponse<B> {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,OPTIONS"),
    );
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, accept"),
    );
    res
}

fn cors_error(status: StatusCode, msg: impl Into<String>) -> AxumResponse<Body> {
    let body = async_graphql::Response::from_errors(vec![async_graphql::ServerError::new(
        msg.into(),
        None,
    )]);
    let json = serde_json::to_vec(&body)
        .unwrap_or_else(|_| b"{\"errors\":[{\"message\":\"internal\"}]}".to_vec());
    apply_cors(
        AxumResponse::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap(),
    )
}

async fn graphql_options() -> impl IntoResponse {
    apply_cors(
        AxumResponse::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap(),
    )
}

async fn graphql(State(state): State<AppState>, req: AxumRequest<Body>) -> AxumResponse<Body> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers;

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    let gql_request = if method == Method::GET {
        let Some(qs) = uri.query() else {
            return cors_error(
                StatusCode::BAD_REQUEST,
                "GET /graphql には query 文字列が必要です",
            );
        };
        match async_graphql::http::parse_query_string(qs) {
            Ok(req) => req,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    } else if method == Method::POST {
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = match to_bytes(body, 2 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string()),
        };
        let cursor = AsyncCursor::new(bytes.to_vec());
        match async_graphql::http::receive_body(ct.as_deref(), cursor, MultipartOptions::default())
            .await
        {
            Ok(req) => req,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    } else {
        return cors_error(StatusCode::METHOD_NOT_ALLOWED, "GET または POST のみ");
    };

    let schema = state.schema.clone();

    if operation_is_subscription(&gql_request) {
        if !is_accept_multipart_mixed(accept) {
            return cors_error(
                StatusCode::NOT_ACCEPTABLE,
                "サブスクリプションは Accept: multipart/mixed; boundary=\"graphql\"; subscriptionSpec=\"1.0\" が必要です",
            );
        }

        let gql_stream = schema.execute_stream(gql_request);
        let heartbeat = stream::empty::<()>();
        let bytes_stream = create_multipart_mixed_stream(gql_stream, heartbeat);
        let try_stream = bytes_stream.map(|chunk| Ok::<_, Infallible>(chunk));
        let body = Body::from_stream(try_stream);
        return apply_cors(
            AxumResponse::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    "multipart/mixed; boundary=\"graphql\"; subscriptionSpec=\"1.0\"",
                )
                .body(body)
                .unwrap(),
        );
    }

    let gql_response = schema.execute(gql_request).await;
    let json = match serde_json::to_vec(&gql_response) {
        Ok(json) => json,
        Err(e) => return cors_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    apply_cors(
        AxumResponse::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap(),
    )
}

async fn playground() -> impl IntoResponse {
    const HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"/><title>ToDo GraphQL</title>
<style>body{font-family:system-ui;margin:2rem;}textarea{width:100%;height:180px;}</style></head>
<body>
<h1>ToDo GraphQL (Workers)</h1>
<p><code>POST /graphql</code> (JSON) または GET (query 文字列)。サブスクリプションは multipart Accept ヘッダが必要です。</p>
<textarea id="q">{"query":"query { todos { id title done } }"}</textarea><br/>
<button id="run">実行</button>
<pre id="out"></pre>
<script>
document.getElementById('run').onclick = async () => {
  const body = document.getElementById('q').value;
  const r = await fetch('/graphql', { method:'POST', headers:{'Content-Type':'application/json'}, body });
  document.getElementById('out').textContent = await r.text();
};
</script>
</body></html>"#;
    AxumResponse::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(HTML))
        .unwrap()
}

#[event(fetch)]
async fn fetch(req: HttpRequest, _env: Env, _ctx: worker::Context) -> Result<http::Response<Body>> {
    let state = AppState {
        schema: app_schema(),
    };

    let mut router: Router = Router::new()
        .route("/", get(playground))
        .route(
            "/graphql",
            get(graphql).post(graphql).options(graphql_options),
        )
        .with_state(state);

    let axum_req = req.map(Body::new);
    let axum_res = router.call(axum_req).await.unwrap();
    Ok(axum_res)
}
