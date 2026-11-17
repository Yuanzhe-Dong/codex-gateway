//! 本地代理服务器：接收 Codex 的 Responses 请求，按模型名路由并透传。
//! 双方均为原生 Responses API，仅替换 URL 与 Authorization，SSE 流式原样转发。

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use futures_util::StreamExt;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::proxy::routing::{classify_model, Upstream};
use crate::runtime::network::{build_clients, UpstreamClients};
use crate::settings::gateway::GatewayConfig;

/// 请求体上限（Responses 请求为 JSON，64MB 足够）。
const BODY_LIMIT: usize = 64 * 1024 * 1024;
const MAX_CONCURRENT: usize = 8;

#[derive(Clone)]
pub struct ProxyState {
    pub deepseek_key: Arc<str>,
    pub clients: Arc<UpstreamClients>,
    pub deepseek_base: Arc<str>,
    pub official_base: Arc<str>,
    pub permits: Arc<Semaphore>,
    pub model_catalog: Arc<serde_json::Value>,
    pub catalog_file: Option<std::path::PathBuf>,
}

impl ProxyState {
    pub fn production(cfg: &GatewayConfig) -> Result<Self> {
        let clients = build_clients(&cfg.proxy, cfg.auto_proxy)?;
        Ok(Self {
            deepseek_key: Arc::from(cfg.deepseek_api_key.as_str()),
            clients: Arc::new(clients),
            deepseek_base: Arc::from(crate::proxy::routing::DEEPSEEK_BASE),
            official_base: Arc::from(crate::proxy::routing::OFFICIAL_BASE),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            model_catalog: Arc::new(crate::models::catalog::current_catalog(
                &cfg.deepseek_models,
            )?),
            catalog_file: Some(crate::runtime::paths::catalog_path()?),
        })
    }
}

/// 前台运行代理服务（阻塞直到退出）。
pub async fn serve(cfg: GatewayConfig, activate: bool) -> Result<()> {
    cfg.validate()?;
    let _instance = crate::runtime::storage::lock(
        &crate::runtime::paths::gateway_dir()?.join("instance.lock"),
    )?;
    let state = ProxyState::production(&cfg)?;
    let app = Router::new().fallback(proxy_all).with_state(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("无法监听 http://{addr}（端口可能被占用），可运行 `codex-gateway setup` 换端口")
    })?;
    crate::runtime::process::write_pid(std::process::id())?;
    if activate {
        let _control = crate::app::lifecycle::control_lock()?;
        if let Err(e) =
            crate::settings::codex::ensure_custom_active(cfg.port, &cfg.deepseek_models[0])
        {
            crate::runtime::process::remove_pid()?;
            return Err(e);
        }
    }
    tracing::info!(
        "codex-gateway v{} listening on http://{addr}",
        env!("CARGO_PKG_VERSION")
    );
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("代理服务异常退出");
    crate::runtime::process::remove_pid()?;
    if activate {
        let _control = crate::app::lifecycle::control_lock()?;
        crate::settings::codex::switch_to_native_for_stop()?;
    }
    result?;
    Ok(())
}

/// 统一入口：本地访问控制、健康检查与并发额度；仅允许明确支持的路径。
async fn proxy_all(State(st): State<ProxyState>, req: Request) -> Response {
    // 浏览器跨站请求和 DNS 重绑定不得利用本地保存的 API Key。
    if req.headers().contains_key(header::ORIGIN) || req.headers().contains_key("sec-fetch-site") {
        return error_response(StatusCode::FORBIDDEN, "网关仅接受本地 Codex 客户端请求");
    }
    if let Some(authority) = req.uri().authority() {
        if !matches!(authority.host(), "127.0.0.1" | "localhost") {
            return error_response(StatusCode::FORBIDDEN, "请求目标必须是本地回环地址");
        }
    }
    if let Some(host) = req.headers().get(header::HOST) {
        let valid = host
            .to_str()
            .ok()
            .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
            .is_some_and(|h| matches!(h.host(), "127.0.0.1" | "localhost"));
        if !valid {
            return error_response(StatusCode::FORBIDDEN, "Host 必须是本地回环地址");
        }
    }
    if matches!(req.uri().path(), "/healthz" | "/v1/healthz") && req.method() == Method::GET {
        return health_json();
    }
    let permit = match st.permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "网关并发请求已满，请稍后重试",
            )
        }
    };
    let response = proxy_request(st, req).await;
    // 流式响应结束/客户端断开才释放并发额度，限制正在进行的请求与 JSON 缓冲内存。
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), permit),
        |(mut stream, permit)| async move { stream.next().await.map(|item| (item, (stream, permit))) },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}

async fn proxy_request(st: ProxyState, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();

    let endpoint = crate::proxy::routing::strip_v1(&path);
    let endpoint = endpoint.trim_end_matches('/');
    if !matches!(endpoint, "/responses" | "/responses/compact" | "/models") {
        return error_response(StatusCode::NOT_FOUND, "不支持的网关路径");
    }
    if (endpoint == "/models" && method != Method::GET)
        || (endpoint != "/models" && method != Method::POST)
    {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "此路径不支持该 HTTP 方法");
    }
    if !headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|auth| auth.starts_with("Bearer ") && !auth[7..].trim().is_empty())
    {
        return error_response(StatusCode::UNAUTHORIZED, "缺少 Codex Bearer 登录凭证");
    }
    // 远程目录和本地 model_catalog_json 必须包含同一组模型。
    // 不能透传官方 /models，否则从接口刷新的客户端会丢掉 DeepSeek。
    if endpoint == "/models" {
        let mut catalog = st.model_catalog.to_string();
        if let Some(path) = &st.catalog_file {
            if let Ok(raw) = tokio::fs::read_to_string(path).await {
                if serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v["models"].as_array().map(|m| !m.is_empty()))
                    == Some(true)
                {
                    catalog = raw;
                }
            }
        }
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(catalog))
            .unwrap();
    }
    let body = match tokio::time::timeout(
        Duration::from_secs(30),
        axum::body::to_bytes(req.into_body(), BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e))
            if e.source()
                .is_some_and(|s| s.is::<http_body_util::LengthLimitError>()) =>
        {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "请求体超过 64 MiB")
        }
        Ok(Err(_)) => return error_response(StatusCode::BAD_REQUEST, "读取请求体失败"),
        Err(_) => return error_response(StatusCode::REQUEST_TIMEOUT, "读取请求体超时"),
    };

    if endpoint != "/models" {
        let model = match parse_model(&body) {
            Ok(model) => model,
            Err(msg) => return error_response(StatusCode::BAD_REQUEST, msg),
        };
        let up = classify_model(&model);
        if endpoint == "/responses/compact" && up == Upstream::DeepSeek {
            return error_response(
                StatusCode::BAD_REQUEST,
                "DeepSeek 不支持服务端 compact，请使用客户端上下文压缩",
            );
        }
        tracing::info!(method = %method, path = %path, model = %model, route = ?up, "代理请求");
        log_input_shape(&body);
        // 所有兼容改动只作用于当前请求的内存副本；不写回会话历史。
        let body = prepare_request_body(&body, up);
        forward(&st, method, &uri, &headers, body, up).await
    } else {
        tracing::info!(method = %method, path = %path, "代理请求(默认转发官方)");
        forward(&st, method, &uri, &headers, body, Upstream::Official).await
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
        "version": env!("CARGO_PKG_VERSION"),
        "history_compatibility": "request_copy",
        "history_writes": false,
        "pid": std::process::id(),
        "instance": crate::runtime::paths::instance_name().unwrap_or_default()
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(crate) mod protocol;
pub(crate) mod routing;
mod transport;
use protocol::{log_input_shape, parse_model, prepare_request_body};
use transport::forward;
#[cfg(test)]
mod tests;
