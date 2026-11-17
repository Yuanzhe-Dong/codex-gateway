use super::transport::to_response;
use super::*;
use axum::body::Bytes;
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
        permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        model_catalog: Arc::new(crate::models::catalog::build_catalog(
            &["deepseek-flash".into(), "deepseek-v4-pro".into()],
            crate::models::catalog::builtin_official_fallback(),
        )),
        catalog_file: None,
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

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    auth: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = AxRequest::builder()
        .method(method)
        .uri(format!("http://127.0.0.1{path}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, auth)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes).to_string()}));
    (status, value)
}

#[tokio::test]
async fn model_endpoint_keeps_deepseek_and_frontier_models_and_reads_refresh() {
    use tower::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut state = test_state("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    state.catalog_file = Some(path.clone());
    for refreshed in [false, true] {
        if refreshed {
            let mut catalog = (*state.model_catalog).clone();
            catalog["models"][0]["display_name"] = serde_json::json!("refreshed");
            crate::models::catalog::write_catalog_file(&catalog, &path).unwrap();
        }
        let app = Router::new().fallback(proxy_all).with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models?client_version=0.164.0")
                    .header(header::AUTHORIZATION, "Bearer fixture")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let models = catalog["models"].as_array().unwrap();
        for slug in [
            "deepseek-flash",
            "deepseek-v4-pro",
            "gpt-6.1-sol",
            "gpt-6.1-terra",
        ] {
            assert!(models.iter().any(|m| m["slug"] == slug));
        }
        if refreshed {
            assert_eq!(models[0]["display_name"], "refreshed");
        }
    }
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
    assert_eq!(
        body["auth"], "Bearer sk-test-key",
        "DeepSeek 路由应注入 DeepSeek Key"
    );
    assert_eq!(body["path"], "/responses", "应去掉 /v1 前缀");
    assert_eq!(body["echo"]["model"], "deepseek-flash");
    assert_eq!(body["content_type"], "application/json");
    assert_eq!(
        body["content_type_count"], 1,
        "转发请求只能有一个 Content-Type"
    );
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
        .body(Body::from(
            json!({"model": "gpt-5.6-sol", "input": "hi"}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["content_type"], "application/json");
    assert_eq!(body["content_type_count"], 1);
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
    assert_eq!(
        body["auth"], "Bearer codex-oauth-token",
        "官方路由应透传 Codex 的 OAuth Bearer"
    );
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
        .body(Body::from(
            json!({"model": "gpt-5.6-sol", "stream": true}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("text/event-stream")
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
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

#[tokio::test]
async fn rejects_invalid_json_models_paths_methods_and_missing_auth() {
    let state = test_state("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    let app = Router::new().fallback(proxy_all).with_state(state);
    for (method, path, body, auth, expected) in [
        (
            "POST",
            "/v1/responses",
            "{invalid",
            true,
            StatusCode::BAD_REQUEST,
        ),
        ("POST", "/v1/responses", "{}", true, StatusCode::BAD_REQUEST),
        (
            "POST",
            "/v1/responses",
            r#"{"model":42}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/v1/responses",
            r#"{"model":""}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/v1/responses",
            r#"{"model":"deepseek-flash"}"#,
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "POST",
            "/v1/responses/compact",
            r#"{"model":"deepseek-flash"}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "PUT",
            "/v1/responses",
            "{}",
            true,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        ("POST", "/v10/responses", "{}", true, StatusCode::NOT_FOUND),
        ("GET", "/other/healthz", "", false, StatusCode::NOT_FOUND),
    ] {
        let mut req = AxRequest::builder().method(method).uri(path);
        if auth {
            req = req.header(header::AUTHORIZATION, "Bearer test");
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{method} {path}: {body}");
    }
}

#[tokio::test]
async fn rejects_browser_origin_and_rebinding_host() {
    let state = test_state("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    let app = Router::new().fallback(proxy_all).with_state(state);
    for (name, value) in [
        ("origin", "https://malicious.example"),
        ("host", "malicious.example:17899"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let req = AxRequest::builder()
            .uri("/healthz")
            .header(name, value)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let req = AxRequest::builder()
        .uri("http://evil.example/healthz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn preserves_http_errors_repeated_headers_and_compressed_body() {
    let app = Router::new().fallback(|| async {
        Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", "15")
            .header(header::CONTENT_TYPE, "application/json")
            .header("connection", "x-remove")
            .header("x-remove", "hop")
            .header("x-test", "one")
            .header("x-test", "two")
            .header("content-encoding", "gzip")
            .body(Body::from(vec![0x1f, 0x8b, 0x08, 0x00]))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = crate::runtime::network::build_client(None)
        .unwrap()
        .get(format!("http://{address}"))
        .send()
        .await
        .unwrap();
    let response = to_response(response).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "15");
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(response.headers().get_all("x-test").iter().count(), 2);
    assert!(!response.headers().contains_key("x-remove"));
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&bytes[..], &[0x1f, 0x8b, 0x08, 0x00]);
    server.abort();
}

#[tokio::test]
async fn stream_is_incremental_and_disconnect_releases_concurrency_slot() {
    let app = Router::new().fallback(|| async {
        let stream = futures_util::stream::unfold(0, |n| async move {
            if n > 1 {
                return None;
            }
            if n == 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Some((
                Ok::<_, std::io::Error>(Bytes::from_static(b"data: event\n\n")),
                n + 1,
            ))
        });
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let state = test_state(format!("http://{address}"), format!("http://{address}"));
    let permits = state.permits.clone();
    let proxy = Router::new().fallback(proxy_all).with_state(state);
    let req = AxRequest::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::AUTHORIZATION, "Bearer test")
        .body(Body::from(r#"{"model":"gpt-test","stream":true}"#))
        .unwrap();
    let response = proxy.oneshot(req).await.unwrap();
    assert_eq!(permits.available_permits(), MAX_CONCURRENT - 1);
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"data: event\n\n");
    drop(stream);
    assert_eq!(permits.available_permits(), MAX_CONCURRENT);
    server.abort();
}

#[tokio::test]
async fn returns_busy_when_limit_is_reached() {
    let state = test_state("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    let guard = state
        .permits
        .clone()
        .acquire_many_owned(MAX_CONCURRENT as u32)
        .await
        .unwrap();
    let app = Router::new().fallback(proxy_all).with_state(state);
    let req = AxRequest::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::AUTHORIZATION, "Bearer test")
        .body(Body::from(r#"{"model":"gpt-test"}"#))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(guard);
}

#[tokio::test]
async fn distinguishes_oversized_body_from_broken_body() {
    let state = test_state("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    let app = Router::new().fallback(proxy_all).with_state(state);
    let chunk = Bytes::from(vec![b' '; 1024 * 1024]);
    let stream =
        futures_util::stream::iter((0..65).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
    let req = AxRequest::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::AUTHORIZATION, "Bearer test")
        .body(Body::from_stream(stream))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let stream =
        futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("broken client"))]);
    let req = AxRequest::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::AUTHORIZATION, "Bearer test")
        .body(Body::from_stream(stream))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn parallel_agent_messages_survive_real_http_forwarding() {
    let upstream = mock_echo_upstream().await;
    let state = test_state(format!("http://{upstream}"), format!("http://{upstream}"));
    let app = Router::new().fallback(proxy_all).with_state(state);
    let jobs = (0..8).map(|index| {
        let app = app.clone();
        async move {
            let body = json!({"model":"gpt-6.1-sol", "reasoning":{"effort":"ultra"}, "service_tier":"priority", "input":[
                {"type":"agent_message", "author":"/root", "recipient":format!("/root/worker_{index}"), "content":[{"type":"input_text","text":"task"}], "id":format!("call_agent_{index}")},
                {"type":"agent_message", "author":format!("/root/worker_{index}"), "recipient":"/root", "content":[{"type":"input_text","text":"result"}]}
            ]});
            let (status, response) = send(&app, "POST", "/v1/responses", "Bearer fixture", body.clone()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(response["echo"], body);
        }
    });
    futures_util::future::join_all(jobs).await;
}
