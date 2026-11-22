//! Axum + async-graphql を Cloudflare Workers (WASM) 上で動かす ToDo API。
//! ストアは Isolate 内のメモリ (本番では D1 / KV 等への置き換えを想定)。

use async_graphql::http::{is_accept_multipart_mixed, MultipartOptions};
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
use futures_util::stream::{self, Stream};
use futures_util::StreamExt;
use std::convert::Infallible;
use std::sync::{Mutex, OnceLock};
use tower_service::Service;
use worker::{event, Env, HttpRequest, Result};

/// アプリ全体で共有する ToDo ストア (Worker Isolate 単位)。
type TodoStore = std::sync::Arc<Mutex<Vec<Todo>>>;

fn lock_todos(store: &TodoStore) -> async_graphql::Result<std::sync::MutexGuard<'_, Vec<Todo>>> {
    store
        .lock()
        .map_err(|_| async_graphql::Error::new("Todo store is unavailable"))
}

#[derive(Clone, SimpleObject)]
struct Todo {
    id: u32,
    title: String,
    done: bool,
}

struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn todos(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Todo>> {
        Ok(lock_todos(ctx.data::<TodoStore>()?)?.clone())
    }
}

struct MutationRoot;

#[Object]
impl MutationRoot {
    async fn create_todo(&self, ctx: &Context<'_>, title: String) -> async_graphql::Result<Todo> {
        let mut g = lock_todos(ctx.data::<TodoStore>()?)?;
        let id = g
            .iter()
            .map(|t| t.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| async_graphql::Error::new("Todo ID space is exhausted"))?;
        let todo = Todo {
            id,
            title,
            done: false,
        };
        g.push(todo.clone());
        Ok(todo)
    }

    async fn toggle_todo(&self, ctx: &Context<'_>, id: u32) -> async_graphql::Result<Todo> {
        let mut g = lock_todos(ctx.data::<TodoStore>()?)?;
        let t = g
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| async_graphql::Error::new("Todo が見つかりません"))?;
        t.done = !t.done;
        Ok(t.clone())
    }

    async fn delete_todo(&self, ctx: &Context<'_>, id: u32) -> async_graphql::Result<bool> {
        let mut g = lock_todos(ctx.data::<TodoStore>()?)?;
        let len = g.len();
        g.retain(|t| t.id != id);
        Ok(g.len() < len)
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
        let store = ctx.data::<TodoStore>()?;
        let snapshots: Vec<Vec<Todo>> = (0..8)
            .map(|_| Ok(lock_todos(store)?.clone()))
            .collect::<async_graphql::Result<_>>()?;
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

fn build_schema(store: TodoStore) -> AppSchema {
    Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
        .data(store)
        .finish()
}

fn app_schema() -> AppSchema {
    static SCHEMA: OnceLock<AppSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| build_schema(todo_store())).clone()
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

/// GraphQL over HTTP: GET/HEAD は query のみ。mutation / subscription は CSRF 等のリスクのため拒否する。
fn operation_allowed_on_get(request: &GqlRequest) -> bool {
    let Ok(doc) = async_graphql_parser::parse_query(&request.query) else {
        return true;
    };
    match &doc.operations {
        DocumentOperations::Single(op) => op.node.ty == OperationType::Query,
        DocumentOperations::Multiple(map) => {
            let Some(name) = &request.operation_name else {
                return false;
            };
            let key = Name::new(name);
            map.get(&key)
                .map(|op| op.node.ty == OperationType::Query)
                .unwrap_or(false)
        }
    }
}

const MULTIPART_CONTENT_TYPE: &str =
    "multipart/mixed; boundary=\"graphql\"; subscriptionSpec=\"1.0\"";

/// Apollo multipart サブスクリプションプロトコルで GraphQL レスポンスを符号化する。
/// `async_graphql::http::create_multipart_mixed_stream` は wasm32 でハートビート用タイマーが
/// `unimplemented!()` のため使わない。
fn multipart_mixed_stream(
    input: impl Stream<Item = async_graphql::Response>,
) -> impl Stream<Item = Vec<u8>> {
    input
        .filter_map(|resp| async move { serde_json::to_vec(&resp).ok() })
        .map(|json| {
            let mut part = b"--graphql\r\nContent-Type: application/json\r\n\r\n".to_vec();
            part.extend_from_slice(&json);
            part.extend_from_slice(b"\r\n");
            part
        })
        .chain(stream::once(async { b"--graphql--\r\n".to_vec() }))
}

fn apply_cors<B>(mut res: AxumResponse<B>) -> AxumResponse<B> {
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,HEAD,POST,OPTIONS"),
    );
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, accept"),
    );
    res
}

fn cors_error(
    status: StatusCode,
    msg: impl Into<String>,
    head_request: bool,
) -> AxumResponse<Body> {
    let body = if head_request {
        Body::empty()
    } else {
        let gql = async_graphql::Response::from_errors(vec![async_graphql::ServerError::new(
            msg.into(),
            None,
        )]);
        let json = serde_json::to_vec(&gql)
            .unwrap_or_else(|_| b"{\"errors\":[{\"message\":\"internal\"}]}".to_vec());
        Body::from(json)
    };
    apply_cors(
        AxumResponse::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
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

/// `/graphql` にパスは一致するがメソッドが GET / HEAD / POST / OPTIONS 以外のとき。
/// Axum の既定 405 には CORS が付かないため、ブラウザのクロスオリジンで失敗しうる。
async fn graphql_method_not_allowed() -> AxumResponse<Body> {
    cors_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "GET, HEAD または POST のみ",
        false,
    )
}

async fn graphql(State(state): State<AppState>, req: AxumRequest<Body>) -> AxumResponse<Body> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let is_head = method == Method::HEAD;
    let uri = parts.uri.clone();
    let headers = parts.headers;

    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    let gql_request = if method == Method::GET || is_head {
        let Some(qs) = uri.query() else {
            return cors_error(
                StatusCode::BAD_REQUEST,
                "GET または HEAD の /graphql には query 文字列が必要です",
                is_head,
            );
        };
        match async_graphql::http::parse_query_string(qs) {
            Ok(req) => req,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string(), is_head),
        }
    } else if method == Method::POST {
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = match to_bytes(body, 2 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string(), is_head),
        };
        let cursor = AsyncCursor::new(bytes.to_vec());
        match async_graphql::http::receive_body(ct.as_deref(), cursor, MultipartOptions::default())
            .await
        {
            Ok(req) => req,
            Err(e) => return cors_error(StatusCode::BAD_REQUEST, e.to_string(), is_head),
        }
    } else {
        // GET / HEAD / POST 以外はルートに載せていないため通常ここへは来ない。
        // 未対応メソッドは `method_not_allowed_fallback` で CORS 付き 405 を返す。
        unreachable!("graphql は GET/HEAD/POST のみルーティングされる")
    };

    if (method == Method::GET || is_head) && !operation_allowed_on_get(&gql_request) {
        return cors_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "GET または HEAD では query 操作のみ許可されます。mutation と subscription は POST を使用してください。",
            is_head,
        );
    }

    let schema = state.schema.clone();

    if operation_is_subscription(&gql_request) {
        if !is_accept_multipart_mixed(accept) {
            return cors_error(
                StatusCode::NOT_ACCEPTABLE,
                "サブスクリプションは Accept: multipart/mixed; boundary=\"graphql\"; subscriptionSpec=\"1.0\" が必要です",
                is_head,
            );
        }

        if is_head {
            return apply_cors(
                AxumResponse::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, MULTIPART_CONTENT_TYPE)
                    .body(Body::empty())
                    .unwrap(),
            );
        }

        let gql_stream = schema.execute_stream(gql_request);
        let try_stream = multipart_mixed_stream(gql_stream).map(Ok::<_, Infallible>);
        let body = Body::from_stream(try_stream);
        return apply_cors(
            AxumResponse::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, MULTIPART_CONTENT_TYPE)
                .body(body)
                .unwrap(),
        );
    }

    let gql_response = schema.execute(gql_request).await;
    let json = match serde_json::to_vec(&gql_response) {
        Ok(json) => json,
        Err(e) => return cors_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), is_head),
    };
    let mut builder = AxumResponse::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if is_head {
        builder = builder.header(header::CONTENT_LENGTH, json.len());
    }
    let res_body = if is_head {
        Body::empty()
    } else {
        Body::from(json)
    };
    apply_cors(builder.body(res_body).unwrap())
}

async fn playground() -> impl IntoResponse {
    const HTML: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"/><title>ToDo GraphQL</title>
<style>body{font-family:system-ui;margin:2rem;}textarea{width:100%;height:180px;}</style></head>
<body>
<h1>ToDo GraphQL (Workers)</h1>
<p><code>POST /graphql</code> (JSON) または GET (query 文字列・query のみ)。mutation / subscription は POST。サブスクリプションは multipart Accept ヘッダが必要です。</p>
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

fn app(state: AppState) -> Router {
    let graphql_router = Router::new()
        .route("/", get(graphql).post(graphql).options(graphql_options))
        .method_not_allowed_fallback(graphql_method_not_allowed);

    Router::new()
        .route("/", get(playground))
        .nest("/graphql", graphql_router)
        .with_state(state)
}

#[event(fetch)]
async fn fetch(req: HttpRequest, _env: Env, _ctx: worker::Context) -> Result<http::Response<Body>> {
    let mut router = app(AppState {
        schema: app_schema(),
    });
    let axum_req = req.map(Body::new);
    let axum_res = router.call(axum_req).await.unwrap();
    Ok(axum_res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use serde_json::{json, Value};

    fn test_app() -> Router {
        app(AppState {
            schema: build_schema(TodoStore::default()),
        })
    }

    fn send(router: &mut Router, req: AxumRequest<Body>) -> (StatusCode, String, String) {
        block_on(async {
            let res = router.call(req).await.unwrap();
            let status = res.status();
            let content_type = res
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default();
            let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (
                status,
                content_type,
                String::from_utf8(bytes.to_vec()).unwrap(),
            )
        })
    }

    fn post(router: &mut Router, query: &str) -> Value {
        let req = AxumRequest::post("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "query": query }).to_string()))
            .unwrap();
        let (status, _, body) = send(router, req);
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[test]
    fn poisoned_store_returns_graphql_errors() {
        let store = TodoStore::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = store.lock().unwrap();
            panic!("poison store");
        });
        let schema = build_schema(store);
        for query in [
            "{ todos { id } }",
            r#"mutation { createTodo(title: "Rust") { id } }"#,
            "mutation { toggleTodo(id: 1) { id } }",
            "mutation { deleteTodo(id: 1) }",
        ] {
            let response = block_on(schema.execute(query));
            assert_eq!(response.errors.len(), 1);
            assert_eq!(response.errors[0].message, "Todo store is unavailable");
        }
        let responses = block_on(
            schema
                .execute_stream("subscription { todoListUpdates { id } }")
                .collect::<Vec<_>>(),
        );
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].errors[0].message, "Todo store is unavailable");
    }

    #[test]
    fn exhausted_ids_return_error_without_poisoning_store() {
        let store = TodoStore::default();
        store.lock().unwrap().push(Todo {
            id: u32::MAX,
            title: "Last".into(),
            done: false,
        });
        let schema = build_schema(store.clone());
        let response =
            block_on(schema.execute(r#"mutation { createTodo(title: "Overflow") { id } }"#));
        assert_eq!(response.errors.len(), 1);
        assert_eq!(response.errors[0].message, "Todo ID space is exhausted");
        assert_eq!(store.lock().unwrap().len(), 1);
        assert!(block_on(schema.execute("{ todos { title } }"))
            .errors
            .is_empty());
    }

    #[test]
    fn empty_store_and_invalid_ids() {
        let mut router = test_app();
        assert_eq!(
            post(&mut router, "{ todos { id } }")["data"]["todos"],
            json!([])
        );
        assert_eq!(
            post(&mut router, "mutation { deleteTodo(id: 1) }")["data"]["deleteTodo"],
            json!(false)
        );
        for id in ["-1", "4294967296"] {
            let response = post(
                &mut router,
                &format!("mutation {{ toggleTodo(id: {id}) {{ id }} }}"),
            );
            assert!(response["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()));
        }
    }

    #[test]
    fn todo_crud() {
        let mut router = test_app();
        let created = post(
            &mut router,
            r#"mutation { createTodo(title: "Rust") { id title done } }"#,
        );
        assert_eq!(
            created["data"]["createTodo"],
            json!({ "id": 1, "title": "Rust", "done": false })
        );

        let toggled = post(&mut router, "mutation { toggleTodo(id: 1) { done } }");
        assert_eq!(toggled["data"]["toggleTodo"]["done"], json!(true));

        let missing = post(&mut router, "mutation { toggleTodo(id: 99) { done } }");
        assert_eq!(
            missing["errors"][0]["message"],
            json!("Todo が見つかりません")
        );

        let todos = post(&mut router, "{ todos { id title done } }");
        assert_eq!(
            todos["data"]["todos"],
            json!([{ "id": 1, "title": "Rust", "done": true }])
        );

        let deleted = post(&mut router, "mutation { deleteTodo(id: 1) }");
        assert_eq!(deleted["data"]["deleteTodo"], json!(true));
        let todos = post(&mut router, "{ todos { id } }");
        assert_eq!(todos["data"]["todos"], json!([]));
    }

    #[test]
    fn get_allows_query_only() {
        let mut router = test_app();
        let req = AxumRequest::get("/graphql?query=%7B%20todos%20%7B%20id%20%7D%20%7D")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&mut router, req);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["data"]["todos"],
            json!([])
        );

        let req = AxumRequest::get("/graphql?query=mutation%20%7B%20deleteTodo(id%3A%201)%20%7D")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&mut router, req);
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn unsupported_method_has_cors() {
        let mut router = test_app();
        let req = AxumRequest::put("/graphql").body(Body::empty()).unwrap();
        let res = block_on(router.call(req)).unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }

    #[test]
    fn subscription_requires_multipart_accept() {
        let mut router = test_app();
        let req = AxumRequest::post("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "query": "subscription { todoListUpdates { id } }" }).to_string(),
            ))
            .unwrap();
        let (status, _, _) = send(&mut router, req);
        assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
    }

    #[test]
    fn subscription_streams_multipart() {
        let mut router = test_app();
        let req = AxumRequest::post("/graphql")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, MULTIPART_CONTENT_TYPE)
            .body(Body::from(
                json!({ "query": "subscription { todoListUpdates { id } }" }).to_string(),
            ))
            .unwrap();
        let (status, content_type, body) = send(&mut router, req);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, MULTIPART_CONTENT_TYPE);
        let part = "--graphql\r\nContent-Type: application/json\r\n\r\n{\"data\":{\"todoListUpdates\":[]}}\r\n";
        assert_eq!(body, format!("{}--graphql--\r\n", part.repeat(8)));
    }
}
