//! 上游 HTTP、身份头隔离和流式响应；不负责历史协议转换。
use super::{error_response, ProxyState};
use crate::proxy::routing::Upstream;
use crate::runtime::network::is_retryable;
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::Response,
};
use futures_util::StreamExt;

fn build_upstream_url(
    deepseek_base: &str,
    official_base: &str,
    up: Upstream,
    path: &str,
    query: Option<&str>,
) -> String {
    let stripped = crate::proxy::routing::strip_v1(path);
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
        if hop_header(headers, name)
            || matches!(
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
                | "cookie" | "origin" | "referer" | "x-api-key"
            )
        {
            continue;
        }
        // DeepSeek 只收到协议必需的头，不应收到官方 account/session/组织信息。
        if up == Upstream::DeepSeek && !matches!(name, "accept" | "user-agent") {
            continue;
        }
        builder = builder.header(k, v);
    }
    builder = builder.header(header::CONTENT_TYPE, "application/json");
    builder = builder.header(header::ACCEPT_ENCODING, "identity");
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

pub(super) async fn forward(
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
            tracing::info!(route = ?up, status = resp.status().as_u16(), "上游响应");
            to_response(resp).await
        }
        Err(e) if is_retryable(&e) && st.clients.fallback.is_some() => {
            tracing::warn!("自动代理连接失败: {}，直连重试一次", e.without_url());
            let retry = with_headers(
                st.clients.fallback.as_ref().unwrap().request(method, &url),
                headers,
                &st.deepseek_key,
                up,
            )
            .body(body);
            match retry.send().await {
                Ok(resp) => {
                    tracing::info!(route = ?up, status = resp.status().as_u16(), "上游响应(直连降级)");
                    to_response(resp).await
                }
                Err(e2) => {
                    tracing::error!("兜底直连失败: {}", e2.without_url());
                    error_response(StatusCode::BAD_GATEWAY, "上游连接失败，请检查网络或代理")
                }
            }
        }
        Err(e) => {
            let status = if e.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            tracing::error!("上游请求失败: {}", e.without_url());
            error_response(status, "上游请求失败，请检查网络或代理")
        }
    }
}

/// 把上游响应转成 axum 响应，SSE 流式透传。
pub(super) async fn to_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let mut builder = Response::builder().status(status);
    let mut headers_map = HeaderMap::new();
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if hop_header(resp.headers(), name) || name == "content-length" {
            continue;
        }
        headers_map.append(k.clone(), v.clone());
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
    let stream = resp
        .bytes_stream()
        .map(|r| r.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) }));
    match builder.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("构建响应失败: {e}"),
        ),
    }
}

/// RFC 的逐跳头和 Connection 显式指定的头都只能用于当前连接。
fn hop_header(headers: &HeaderMap, name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn never_sends_official_identity_or_hop_headers_to_deepseek() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer oauth-secret"),
            ("chatgpt-account-id", "account-secret"),
            ("cookie", "session=secret"),
            ("proxy-authorization", "proxy-secret"),
            ("connection", "x-remove"),
            ("x-remove", "hop-secret"),
            ("x-codex-session-id", "session-secret"),
            ("accept", "text/event-stream"),
        ] {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let client = crate::runtime::network::build_client(None).unwrap();
        let request = with_headers(
            client.post("http://127.0.0.1/responses"),
            &headers,
            "sk-deepseek",
            Upstream::DeepSeek,
        )
        .build()
        .unwrap();
        assert_eq!(
            request.headers()[header::AUTHORIZATION],
            "Bearer sk-deepseek"
        );
        for name in [
            "chatgpt-account-id",
            "cookie",
            "proxy-authorization",
            "x-remove",
            "x-codex-session-id",
        ] {
            assert!(
                !request.headers().contains_key(name),
                "{name} 不应发往 DeepSeek"
            );
        }
        let request = with_headers(
            client.post("http://127.0.0.1/responses"),
            &headers,
            "sk-deepseek",
            Upstream::Official,
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["chatgpt-account-id"], "account-secret");
        assert_eq!(
            request.headers()[header::AUTHORIZATION],
            "Bearer oauth-secret"
        );
        assert!(!request.headers().contains_key("x-remove"));
        assert!(!request.headers().contains_key("cookie"));
    }
}
