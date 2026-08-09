//! 本地代理服务器：接收 Codex 的 Responses 请求，按模型名路由并透传。
//! 双方均为原生 Responses API，仅替换 URL 与 Authorization，SSE 流式原样转发。

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use futures_util::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::GatewayConfig;
use crate::network::{is_retryable, build_clients, UpstreamClients};
use crate::upstream::{classify_model, Upstream};

/// 请求体上限（Responses 请求为 JSON，64MB 足够）。
const BODY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct ProxyState {
    pub deepseek_key: Arc<str>,
    pub clients: Arc<UpstreamClients>,
    pub deepseek_base: Arc<str>,
    pub official_base: Arc<str>,
}

impl ProxyState {
    pub fn production(cfg: &GatewayConfig) -> Result<Self> {
        let clients = build_clients(&cfg.proxy, cfg.auto_proxy)?;
        Ok(Self {
            deepseek_key: Arc::from(cfg.deepseek_api_key.as_str()),
            clients: Arc::new(clients),
            deepseek_base: Arc::from(crate::upstream::DEEPSEEK_BASE),
            official_base: Arc::from(crate::upstream::OFFICIAL_BASE),
        })
    }
}

/// 前台运行代理服务（阻塞直到退出）。
pub async fn serve(cfg: GatewayConfig) -> Result<()> {
    if cfg.deepseek_api_key.trim().is_empty() {
        anyhow::bail!("DeepSeek API Key 未设置，请先运行 `codex-gateway setup`");
    }
    let state = ProxyState::production(&cfg)?;
    let app = Router::new().fallback(proxy_all).with_state(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("无法监听 http://{addr}（端口可能被占用），可运行 `codex-gateway setup` 换端口"))?;
    tracing::info!(
        "codex-gateway v{} listening on http://{addr}",
        env!("CARGO_PKG_VERSION")
    );
    axum::serve(listener, app).await.context("代理服务异常退出")?;
    Ok(())
}

/// 统一入口：/healthz 健康检查；/responses 按模型路由；其余透传官方。
async fn proxy_all(State(st): State<ProxyState>, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();

    if path.ends_with("/healthz") && method == Method::GET {
        return health_json();
    }

    let body = match axum::body::to_bytes(req.into_body(), BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "请求体过大"),
    };

    if (path.ends_with("/responses") || path.ends_with("/responses/")) && (method == Method::POST || method == Method::PUT) {
        let model = parse_model(&body);
        let up = classify_model(&model);
        tracing::info!(method = %method, path = %path, model = %model, route = ?up, "代理请求");
        log_input_shape(&body);
        // 官方后端要求 input 里 reasoning/developer 项的 content 为空数组（由服务端注入），
        // 桌面端会原样带 content，被 400 "input[N].content: array too long" 拒绝，故清洗后再转发
        let body = if up == Upstream::Official {
            sanitize_for_official(&body)
        } else {
            body
        };
        forward(&st, method, &uri, &headers, body, up).await
    } else {
        tracing::info!(method = %method, path = %path, "代理请求(默认转发官方)");
        forward(&st, method, &uri, &headers, body, Upstream::Official).await
    }
}

/// 诊断：只记录每个 input 项的类型/角色/content 长度，不打印内容（含用户与推理文本）。
fn log_input_shape(body: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    if let Some(arr) = v.get("input").and_then(|i| i.as_array()) {
        for (i, item) in arr.iter().enumerate() {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("?");
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("-");
            let clen = item
                .get("content")
                .and_then(|c| c.as_array())
                .map(|c| c.len())
                .unwrap_or(0);
            tracing::info!("input[{i}] type={ty} role={role} content_len={clen}");
        }
    }
}

/// 官方 /responses 清洗：按 input 项的 `type` 区分处理 content 字段。
///
/// - `reasoning`：官方要求 content 为空数组（length 0），桌面端会原样带非空 content，
///   被 400 "array too long" 拒绝，故强制置空。
/// - `message`：保留 content（user/assistant/developer/system 的正常内容，含系统指令）。
/// - 其余类型（`additional_tools` / `function_call` / `function_call_output` /
///   `custom_tool_call` / `custom_tool_call_output` / `compaction` 等）不应有 content 字段；
///   桌面端可能误带，移除以免 400 "Unknown parameter: 'input[N].content'"。
fn sanitize_for_official(body: &[u8]) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Bytes::copy_from_slice(body);
    };
    if let Some(input) = v.get_mut("input").and_then(|i| i.as_array_mut()) {
        for item in input.iter_mut() {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match ty {
                "reasoning" => {
                    item["content"] = serde_json::Value::Array(vec![]);
                }
                "message" => {}
                _ => {
                    if let Some(obj) = item.as_object_mut() {
                        obj.remove("content");
                    }
                }
            }
        }
    }
    Bytes::from(v.to_string())
}

fn parse_model(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default()
}

fn build_upstream_url(
    deepseek_base: &str,
    official_base: &str,
    up: Upstream,
    path: &str,
    query: Option<&str>,
) -> String {
    let stripped = crate::upstream::strip_v1(path);
    let base = match up {
        Upstream::DeepSeek => deepseek_base,
        Upstream::Official => official_base,
    };
    let mut url = format!("{base}{stripped}");
    if let Some(q) = query {
        if !q.is_empty() {
            url.push('?');
            url.push_str(q);
        }
    }
    url
}

/// 构建带正确头的 reqwest 请求。
fn with_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &HeaderMap,
    deepseek_key: &str,
    up: Upstream,
) -> reqwest::RequestBuilder {
    for (k, v) in headers.iter() {
        let name = k.as_str();
        if matches!(
            name,
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "accept-encoding"
                | "authorization"
                // 不复制原始 content-type：reqwest 的 header() 是 append 语义，
                // 复制后再强制 application/json 会产生重复 Content-Type，被上游 FastAPI 拒绝
                | "content-type"
        ) {
            continue;
        }
        builder = builder.header(k, v);
    }
    builder = builder.header(header::CONTENT_TYPE, "application/json");
    match up {
        Upstream::DeepSeek => {
            if !deepseek_key.trim().is_empty() {
                builder = builder.bearer_auth(deepseek_key.trim());
            }
        }
        Upstream::Official => {
            if let Some(auth) = headers.get(header::AUTHORIZATION) {
                builder = builder.header(header::AUTHORIZATION, auth);
            }
        }
    }
    builder
}

async fn forward(
    st: &ProxyState,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    up: Upstream,
) -> Response {
    let url = build_upstream_url(
        &st.deepseek_base,
        &st.official_base,
        up,
        uri.path(),
        uri.query(),
    );

    let request = with_headers(
        st.clients.primary.request(method.clone(), &url),
        headers,
        &st.deepseek_key,
        up,
    )
    .body(body.clone());

    match request.send().await {
        Ok(resp) => {
            tracing::info!(url = %url, status = resp.status().as_u16(), "上游响应");
            to_response(resp).await
        }
        Err(e) if is_retryable(&e) => {
            tracing::warn!(
                "主客户端连接失败（{url}）: {e}，降级直连重试一次"
            );
            let retry = with_headers(
                st.clients.fallback.request(method, &url),
                headers,
                &st.deepseek_key,
                up,
            )
            .body(body);
            match retry.send().await {
                Ok(resp) => {
                    tracing::info!(url = %url, status = resp.status().as_u16(), "上游响应(直连降级)");
                    to_response(resp).await
                }
                Err(e2) => {
                    tracing::error!("兜底直连也失败（{url}）: {e2}");
                    error_response(StatusCode::BAD_GATEWAY, &format!("上游连接失败: {e2}"))
                }
            }
        }
        Err(e) => {
            tracing::error!("上游请求失败（{url}）: {e}");
            error_response(StatusCode::BAD_GATEWAY, &format!("上游请求失败: {e}"))
        }
    }
}

/// 把上游响应转成 axum 响应，SSE 流式透传。
async fn to_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let mut builder = Response::builder().status(status);
    let mut headers_map = HeaderMap::new();
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if matches!(
            name,
            "transfer-encoding" | "connection" | "content-length" | "content-encoding"
        ) {
            continue;
        }
        headers_map.insert(k.clone(), v.clone());
    }
    if !headers_map.contains_key(header::CONTENT_TYPE) {
        headers_map.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    if let Some(hdrs) = builder.headers_mut() {
        *hdrs = headers_map;
    }
    let stream = resp.bytes_stream().map(|r| {
        r.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
    });
    match builder.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("构建响应失败: {e}")),
    }
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::json!({
        "error": { "message": msg, "type": "codex_gateway_error", "code": status.as_u16() }
    });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn health_json() -> Response {
    let body = serde_json::json!({
        "ok": true,
        "service": "codex-gateway",
        "version": env!("CARGO_PKG_VERSION")
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as AxRequest;
    use axum::response::IntoResponse;
    use serde_json::json;
    use tower::util::ServiceExt;

    fn test_state(deepseek_base: String, official_base: String) -> ProxyState {
        let clients = build_clients(&None, false).unwrap();
        ProxyState {
            deepseek_key: Arc::from("sk-test-key"),
            clients: Arc::new(clients),
            deepseek_base: Arc::from(deepseek_base),
            official_base: Arc::from(official_base),
        }
    }

    /// 启动一个 echo 型 mock 上游，返回其地址。
    async fn mock_echo_upstream() -> SocketAddr {
        let app = Router::new().fallback(|req: Request| async move {
            let (parts, body) = req.into_parts();
            let auth = parts
                .headers
                .get(header::AUTHORIZATION)
                .map(|v| v.to_str().unwrap_or("").to_string())
                .unwrap_or_default();
            let ct = parts
                .headers
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or("").to_string())
                .unwrap_or_default();
            let ct_count = parts.headers.get_all(header::CONTENT_TYPE).iter().count();
            let path = parts.uri.path().to_string();
            let method = parts.method.to_string();
            let body_bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
            let echo = String::from_utf8_lossy(&body_bytes).to_string();
            let resp = json!({
                "mock": true,
                "method": method,
                "path": path,
                "auth": auth,
                "content_type": ct,
                "content_type_count": ct_count,
                "echo": serde_json::from_str::<serde_json::Value>(&echo).ok()
            });
            axum::Json(resp).into_response()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn send(app: &Router, method: &str, path: &str, auth: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let req = AxRequest::builder()
            .method(method)
            .uri(format!("http://127.0.0.1{path}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, auth)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes).to_string()}));
        (status, value)
    }

    #[tokio::test]
    async fn deepseek_route_injects_deepseek_key() {
        let ds = mock_echo_upstream().await;
        let off = mock_echo_upstream().await;
        let state = test_state(format!("http://{ds}"), format!("http://{off}"));
        let app = Router::new().fallback(proxy_all).with_state(state);

        let (status, body) = send(
            &app,
            "POST",
            "/v1/responses",
            "Bearer codex-oauth-token",
            json!({"model": "deepseek-v4-flash", "input": "hi"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["auth"], "Bearer sk-test-key", "DeepSeek 路由应注入 DeepSeek Key");
        assert_eq!(body["path"], "/responses", "应去掉 /v1 前缀");
        assert_eq!(body["echo"]["model"], "deepseek-v4-flash");
        assert_eq!(body["content_type"], "application/json");
        assert_eq!(body["content_type_count"], 1, "转发请求只能有一个 Content-Type");
    }

    #[tokio::test]
    async fn content_type_forced_single_even_with_client_ct() {
        // 客户端带任意 Content-Type 时，转发到上游也必须只有一个 application/json
        // （回归：复制原始 content-type + 强制 application/json 会重复，被 FastAPI 拒为
        //   {"detail":"Unsupported content type"}）
        let ds = mock_echo_upstream().await;
        let off = mock_echo_upstream().await;
        let state = test_state(format!("http://{ds}"), format!("http://{off}"));
        let app = Router::new().fallback(proxy_all).with_state(state);

        let req = AxRequest::builder()
            .method("POST")
            .uri("http://127.0.0.1/v1/responses")
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(header::AUTHORIZATION, "Bearer codex-oauth-token")
            .body(Body::from(json!({"model": "gpt-5.6-sol", "input": "hi"}).to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["content_type"], "application/json");
        assert_eq!(body["content_type_count"], 1);
    }

    #[test]
    fn sanitize_official_empties_reasoning_keeps_message_removes_others() {
        // reasoning content 置空；message content 保留（含 developer 系统指令）；
        // 非 message 类型（additional_tools / custom_tool_call 等）移除 content 字段
        let raw = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sys"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [], "content": [{"type": "reasoning_text", "text": "think"}]},
                {"type": "additional_tools", "role": "developer", "content": [], "tools": [{"type": "function", "name": "f"}]},
                {"type": "custom_tool_call", "name": "f", "arguments": "{}", "content": [{"type": "output_text", "text": "x"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(raw.to_string().as_bytes())).unwrap();
        let input = out["input"].as_array().unwrap();
        // developer message content 保留（系统指令）
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 1, "developer message content 应保留");
        // reasoning content 置空，id/summary 保留
        assert!(input[1]["content"].as_array().unwrap().is_empty(), "reasoning.content 应置空");
        assert_eq!(input[1]["id"], "rs_1", "reasoning 的 id/summary 应保留");
        // additional_tools 的 content 应被移除，tools 保留
        assert!(input[2].get("content").is_none(), "additional_tools 不应残留 content");
        assert!(input[2].get("tools").is_some(), "additional_tools 的 tools 应保留");
        // custom_tool_call 的 content 应被移除，name 保留
        assert!(input[3].get("content").is_none(), "custom_tool_call 不应残留 content");
        assert_eq!(input[3]["name"], "f", "custom_tool_call 的 name 应保留");
        // 用户消息保留
        assert_eq!(input[4]["content"].as_array().unwrap().len(), 1, "用户消息 content 应保留");
    }

    #[tokio::test]
    async fn official_route_passthroughs_oauth() {
        let ds = mock_echo_upstream().await;
        let off = mock_echo_upstream().await;
        let state = test_state(format!("http://{ds}"), format!("http://{off}"));
        let app = Router::new().fallback(proxy_all).with_state(state);

        let (status, body) = send(
            &app,
            "POST",
            "/v1/responses",
            "Bearer codex-oauth-token",
            json!({"model": "gpt-5.6-sol", "input": "hi"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["auth"], "Bearer codex-oauth-token", "官方路由应透传 Codex 的 OAuth Bearer");
    }

    #[tokio::test]
    async fn sse_stream_passthrough() {
        // 官方 mock 返回 SSE 流
        let off_addr = {
            let app = Router::new().fallback(|_req: Request| async move {
                let stream = futures_util::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from("data: {\"type\":\"response.created\"}\n\n")),
                    Ok::<_, std::io::Error>(Bytes::from("data: {\"type\":\"response.completed\"}\n\n")),
                    Ok::<_, std::io::Error>(Bytes::from("data: [DONE]\n\n")),
                ]);
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            addr
        };
        let ds = mock_echo_upstream().await;
        let state = test_state(format!("http://{ds}"), format!("http://{off_addr}"));
        let app = Router::new().fallback(proxy_all).with_state(state);

        let req = AxRequest::builder()
            .method("POST")
            .uri("http://127.0.0.1/v1/responses")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, "Bearer codex-oauth-token")
            .body(Body::from(json!({"model": "gpt-5.6-sol", "stream": true}).to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).map(|v| v.to_str().unwrap()),
            Some("text/event-stream")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("response.created"));
        assert!(text.contains("response.completed"));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn healthz_ok() {
        let ds = mock_echo_upstream().await;
        let off = mock_echo_upstream().await;
        let state = test_state(format!("http://{ds}"), format!("http://{off}"));
        let app = Router::new().fallback(proxy_all).with_state(state);

        let req = AxRequest::builder()
            .method("GET")
            .uri("http://127.0.0.1/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}