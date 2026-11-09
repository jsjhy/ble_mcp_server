//! MCP 的 HTTP + SSE 传输层（MCP 2024-11-05 规范）。
//!
//! 协议流程：
//! 1. 客户端 `GET /sse`，服务端返回 `text/event-stream`；
//!    第一个事件是 `event: endpoint`，data 为发消息的 URL（`/message?sessionId=xxx`）。
//! 2. 客户端 `POST /message?sessionId=xxx` 发 JSON-RPC（每行一条），服务端立即返回 `202 Accepted`。
//! 3. 处理结果通过该会话的 SSE 流以 `event: message` 推送回去。
//!
//! 另外提供一个非规范但方便调试的同步接口 `POST /rpc`：直接返回 JSON-RPC 响应。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

use crate::ble::BleCentral;
use crate::mcp;
use crate::state::AppState;

type SseSender = mpsc::UnboundedSender<Result<Event, Infallible>>;

#[derive(Clone)]
struct Ctx {
    ble: Arc<Mutex<BleCentral>>,
    state: Arc<RwLock<AppState>>,
    sessions: Arc<Mutex<HashMap<String, SseSender>>>,
}

/// 启动 HTTP + SSE 服务，监听 `bind:port`。
pub async fn serve(
    ble: Arc<Mutex<BleCentral>>,
    state: Arc<RwLock<AppState>>,
    bind: &str,
    port: u16,
) -> anyhow::Result<()> {
    let ctx = Ctx {
        ble,
        state,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let router = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/sse", get(sse_handler))
        .route("/message", post(message_handler))
        .route("/rpc", post(rpc_handler))
        .with_state(ctx);

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    log::info!("MCP HTTP+SSE 已监听 http://{addr}");
    log::info!("  SSE 入口:   GET  http://{addr}/sse");
    log::info!("  消息入口:   POST http://{addr}/message?sessionId=xxx");
    log::info!("  同步调试:   POST http://{addr}/rpc   （直接返回 JSON-RPC 响应）");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    Json(json!({
        "server": "ble_server",
        "protocol": "MCP 2024-11-05",
        "transport": "http+sse",
        "endpoints": {
            "sse": "GET /sse",
            "message": "POST /message?sessionId=<from endpoint event>",
            "rpc": "POST /rpc  (non-standard convenience, returns JSON-RPC response inline)",
            "health": "GET /health"
        }
    }))
}

async fn health(State(ctx): State<Ctx>) -> impl IntoResponse {
    let sessions = ctx.sessions.lock().await.len();
    let connected = ctx
        .state
        .read()
        .map(|s| s.connected_address.clone())
        .unwrap_or(None);
    Json(json!({
        "ok": true,
        "sessions": sessions,
        "connected": connected
    }))
}

/// GET /sse —— 建立 SSE 会话，首帧下发 POST 端点。
async fn sse_handler(State(ctx): State<Ctx>) -> Response {
    let sid = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();

    // 第一帧：告诉客户端往哪里 POST
    let _ = tx.send(Ok(Event::default()
        .event("endpoint")
        .data(format!("/message?sessionId={sid}"))));

    ctx.sessions.lock().await.insert(sid.clone(), tx.clone());
    let sessions = Arc::downgrade(&ctx.sessions);
    let cleanup_sid = sid.clone();
    tokio::spawn(async move {
        tx.closed().await;
        if let Some(sessions) = sessions.upgrade() {
            sessions.lock().await.remove(&cleanup_sid);
        }
    });
    log::info!("SSE 会话建立: {sid}");

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// POST /message?sessionId=xxx —— 收 JSON-RPC，异步处理后经 SSE 回推。
async fn message_handler(
    State(ctx): State<Ctx>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let sid = q
        .get("sessionId")
        .or_else(|| q.get("session_id"))
        .cloned()
        .unwrap_or_default();

    let tx = match ctx.sessions.lock().await.get(&sid).cloned() {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("未知 sessionId: {sid:?}；请先 GET /sse 获取 endpoint"),
            )
                .into_response()
        }
    };

    let msg: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            });
            let _ = tx.send(Ok(Event::default().event("message").data(err.to_string())));
            return (StatusCode::BAD_REQUEST, format!("parse error: {e}")).into_response();
        }
    };

    let ctx2 = ctx.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        if let Some(resp) = mcp::handle_message(&ctx2.ble, &ctx2.state, msg).await {
            let data = serde_json::to_string(&resp).unwrap_or_default();
            if tx2
                .send(Ok(Event::default().event("message").data(data)))
                .is_err()
            {
                log::warn!("SSE 推送失败（客户端可能已断开）");
            }
        }
    });

    StatusCode::ACCEPTED.into_response()
}

/// POST /rpc —— 同步调试接口：一次请求直接拿到 JSON-RPC 响应。
async fn rpc_handler(State(ctx): State<Ctx>, body: String) -> Response {
    let msg: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                })),
            )
                .into_response()
        }
    };

    match mcp::handle_message(&ctx.ble, &ctx.state, msg).await {
        Some(resp) => Json(resp).into_response(),
        None => (StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, HttpBody};
    async fn context() -> Ctx {
        let state = Arc::new(RwLock::new(AppState::default()));
        Ctx {
            ble: Arc::new(Mutex::new(BleCentral::simulated(state.clone()).await)),
            state,
            sessions: Default::default(),
        }
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
    }
    #[tokio::test]
    async fn rpc_success_parse_error_and_notification() {
        let ctx = context().await;
        let r = rpc_handler(
            State(ctx.clone()),
            r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#.into(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(json_body(r).await["id"], 7);
        let r = rpc_handler(State(ctx.clone()), "{".into()).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(r).await["error"]["code"], -32700);
        let r = rpc_handler(
            State(ctx),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }
    #[tokio::test]
    async fn sse_endpoint_delivery_isolation_and_cleanup() {
        let ctx = context().await;
        let a = sse_handler(State(ctx.clone())).await;
        let b = sse_handler(State(ctx.clone())).await;
        assert_eq!(a.headers()["content-type"], "text/event-stream");
        assert_eq!(ctx.sessions.lock().await.len(), 2);
        let mut body = a.into_body();
        let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap();
        let data = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(data.contains("event: endpoint"));
        assert!(data.contains("/message?sessionId="));
        let sid = data.split("sessionId=").nth(1).unwrap().trim().to_string();
        let query = HashMap::from([("sessionId".into(), sid)]);
        let r = message_handler(
            State(ctx.clone()),
            Query(query),
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#.into(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let data = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(data.contains("event: message"));
        assert!(data.contains("\"id\":3"));
        drop(body);
        drop(b);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !ctx.sessions.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let r = message_handler(State(ctx.clone()), Query(HashMap::new()), "{}".into()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_body(health(State(ctx)).await.into_response()).await["sessions"],
            0
        );
    }
    #[tokio::test]
    async fn message_parse_error_is_sent_to_own_session() {
        let ctx = context().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        ctx.sessions.lock().await.insert("test".into(), tx);
        let r = message_handler(
            State(ctx),
            Query(HashMap::from([("session_id".into(), "test".into())])),
            "{".into(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(rx.recv().await.unwrap().is_ok());
    }
}
