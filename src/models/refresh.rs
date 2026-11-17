//! 按账号刷新官方模型目录，以及不切换 provider 的 DeepSeek 小请求测试。
use anyhow::{anyhow, ensure, Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};

use crate::models::catalog;
use crate::proxy::routing as upstream;
use crate::runtime::network;
use crate::runtime::paths;
use crate::settings::gateway::GatewayConfig;

// 刷新/测试都有限时；不影响长任务 SSE 使用的独立超时策略。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct OfficialAuth {
    token: String,
    account: String,
}

fn official_auth(path: &Path) -> Result<OfficialAuth> {
    let raw = fs::read_to_string(path).context("无法读取 Codex 登录信息，请先在 Codex 登录")?;
    let data: Value = serde_json::from_str(&raw).context("Codex 登录信息格式错误")?;
    // 订阅目录只接受 OAuth；不能把 OPENAI_API_KEY 当成 ChatGPT 登录令牌发送。
    let token = data["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("未找到 Codex OAuth 登录态，请先使用 ChatGPT 账号登录 Codex")?;
    let account = data["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .context("Codex 登录态缺少 account_id，请重新登录 Codex")?;
    Ok(OfficialAuth {
        token: token.into(),
        account: account.into(),
    })
}

async fn send(primary: RequestBuilder, fallback: Option<RequestBuilder>) -> Result<Response> {
    match primary.send().await {
        Ok(response) => Ok(response),
        Err(error) if network::is_retryable(&error) && fallback.is_some() => fallback
            .unwrap()
            .send()
            .await
            .map_err(|e| anyhow!("直连请求失败：{}", e.without_url())),
        Err(error) => Err(anyhow!("请求失败：{}", error.without_url())),
    }
}

async fn decode(response: Response, label: &str) -> Result<Value> {
    let status = response.status();
    // 不回显原始服务端错误内容，避免登录令牌或代理信息进入控制台/日志。
    ensure!(
        status.is_success(),
        "{label}返回 HTTP {status}；401/403 请检查登录或密钥，402 请检查余额，429 请稍后重试"
    );
    let body = response
        .bytes()
        .await
        .map_err(|e| anyhow!("读取{label}响应失败：{}", e.without_url()))?;
    serde_json::from_slice(&body).context("上游响应不是有效 JSON")
}

async fn fetch_official(
    clients: &network::UpstreamClients,
    auth: &OfficialAuth,
    client_version: Option<&str>,
    base: &str,
) -> Result<Value> {
    let request = |client: &Client| {
        let mut request = client
            .get(format!("{base}/models"))
            .bearer_auth(&auth.token)
            .header("chatgpt-account-id", &auth.account)
            .header("accept", "application/json")
            .header("accept-encoding", "identity")
            .header("originator", "codex_cli_rs")
            .header(
                "user-agent",
                format!(
                    "codex_cli_rs/{}",
                    client_version.unwrap_or(env!("CARGO_PKG_VERSION"))
                ),
            )
            .timeout(REQUEST_TIMEOUT);
        if let Some(version) = client_version {
            request = request.query(&[("client_version", version)]);
        }
        request
    };
    let data = decode(
        send(
            request(&clients.primary),
            clients.fallback.as_ref().map(request),
        )
        .await?,
        "官方模型目录",
    )
    .await?;
    // 空目录或错误结构不能覆盖当前仍可用的快照。
    catalog::official_models_from_value(&data)?;
    Ok(data)
}

/// 只刷新网关自己的目录，不修改用户选定的模型、provider、登录信息或原生缓存。
pub async fn refresh_models() -> Result<()> {
    let cfg = GatewayConfig::load()?.unwrap_or_default();
    let clients = network::build_clients(&cfg.proxy, cfg.auto_proxy)?;
    let auth = official_auth(&paths::codex_home()?.join("auth.json"))?;
    let native = paths::codex_models_cache_path()?;
    let cached: Option<Value> = if native.exists() {
        Some(serde_json::from_slice(&fs::read(&native)?).context("官方模型缓存格式错误")?)
    } else {
        None
    };
    let installed = crate::models::client::installed_version();
    let client_version = installed
        .as_deref()
        .or_else(|| cached.as_ref().and_then(|v| v["client_version"].as_str()));
    let data = fetch_official(&clients, &auth, client_version, upstream::OFFICIAL_BASE).await?;
    let official = catalog::official_models_from_value(&data)?;
    println!(
        "官方接口返回的模型元数据（{} 个，隐藏条目不在菜单展示）：",
        official.len()
    );
    for model in &official {
        println!("  {}", model["slug"].as_str().unwrap());
    }
    catalog::write_catalog_file(&data, &paths::official_models_path()?)?;
    catalog::write_catalog_file(
        &catalog::current_catalog(&cfg.deepseek_models)?,
        &paths::catalog_path()?,
    )?;
    println!("目录补充已知的 6.x 选项；菜单出现不等于账号已获授权，实际可用性由上游决定。");
    println!(
        "已刷新合并目录；deepseek-flash 对应当前 V4.1 Flash，DeepSeek Key 由 test-deepseek 验证。"
    );
    println!("重新打开 Codex 后载入菜单；当前 provider 和模型选择保持不变。");
    Ok(())
}

fn text_request(model: &str) -> Value {
    json!({
        "model":model, "input":"Reply with exactly CODEX_GATEWAY_OK and nothing else.",
        "reasoning":{"effort":"none"}, "max_output_tokens":64, "stream":false
    })
}

fn tool_request(model: &str) -> Value {
    json!({
        "model":model, "input":"Call gateway_probe with status ok.",
        "reasoning":{"effort":"none"}, "max_output_tokens":128, "stream":false,
        "tools":[{"type":"function", "name":"gateway_probe", "description":"Report probe status",
            "parameters":{"type":"object","properties":{"status":{"type":"string","enum":["ok"]}},
                "required":["status"],"additionalProperties":false}}],
        "tool_choice":{"type":"function","name":"gateway_probe"}
    })
}

fn check_result(data: &Value, tools: bool) -> Result<()> {
    ensure!(
        data["status"] == "completed",
        "响应未完成，测试未通过（请检查余额、输出限制或重试）"
    );
    let output = data["output"].as_array().context("响应缺少 output")?;
    let passed = if tools {
        output.iter().any(|item| {
            item["type"] == "function_call"
                && item["name"] == "gateway_probe"
                && item["arguments"].as_str().is_some_and(|args| {
                    serde_json::from_str::<Value>(args).is_ok_and(|v| v["status"] == "ok")
                })
        })
    } else {
        let text: String = output
            .iter()
            .filter(|item| item["type"] == "message")
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .filter(|part| part["type"] == "output_text")
            .filter_map(|part| part["text"].as_str())
            .collect();
        text.trim() == "CODEX_GATEWAY_OK"
    };
    ensure!(passed, "模型返回与测试要求不符；未执行任何工具或修改工作区");
    Ok(())
}

/// 两个有输出上限的小请求：文本和函数调用。只检查返回值，不执行模型生成的工具。
pub async fn test_deepseek(cfg: &GatewayConfig, model: &str) -> Result<()> {
    let model = upstream::canonical_deepseek_model(model);
    ensure!(
        crate::settings::gateway::DEFAULT_DEEPSEEK_MODELS.contains(&model),
        "测试仅支持当前 DeepSeek 模型"
    );
    let clients = network::build_clients(&cfg.proxy, cfg.auto_proxy)?;
    println!("测试 {model}：发送两次小请求，按 DeepSeek API 正常计费。");
    for tools in [false, true] {
        let body = if tools {
            tool_request(model)
        } else {
            text_request(model)
        };
        let request = |client: &Client| {
            client
                .post(format!("{}/responses", upstream::DEEPSEEK_BASE))
                .bearer_auth(cfg.deepseek_api_key.trim())
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
        };
        let started = std::time::Instant::now();
        let data = decode(
            send(
                request(&clients.primary),
                clients.fallback.as_ref().map(request),
            )
            .await?,
            "DeepSeek",
        )
        .await?;
        check_result(&data, tools)?;
        println!(
            "{}通过；返回模型 {}；用时 {:.2}s；总 token {}",
            if tools {
                "函数调用"
            } else {
                "文本响应"
            },
            data["model"].as_str().unwrap_or("未提供"),
            started.elapsed().as_secs_f64(),
            data["usage"]["total_tokens"]
        );
    }
    println!("基础接口测试完成；这不代表已验证桌面端长任务或基准能力。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_api_key_only_auth_without_exposing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        fs::write(&path, r#"{"OPENAI_API_KEY":"sk-private-test"}"#).unwrap();
        let error = official_auth(&path).err().unwrap().to_string();
        assert!(error.contains("OAuth"));
        assert!(!error.contains("sk-private-test"));
    }

    #[test]
    fn probe_never_treats_reasoning_text_or_incomplete_as_success() {
        let response = json!({"status":"completed","output":[
            {"type":"reasoning","content":[{"type":"reasoning_text","text":"CODEX_GATEWAY_OK"}]}
        ]});
        assert!(check_result(&response, false).is_err());
        let mut response = json!({"status":"completed","output":[
            {"type":"message","content":[{"type":"output_text","text":"CODEX_GATEWAY_OK"}]}
        ]});
        assert!(check_result(&response, false).is_ok());
        response["status"] = json!("incomplete");
        assert!(check_result(&response, false).is_err());
    }

    #[test]
    fn probe_checks_function_name_and_parsed_arguments() {
        let mut response = json!({"status":"completed","output":[
            {"type":"function_call","name":"gateway_probe","arguments":"{\"status\":\"ok\"}"}
        ]});
        assert!(check_result(&response, true).is_ok());
        response["output"][0]["arguments"] = json!("bad json");
        assert!(check_result(&response, true).is_err());
    }

    #[tokio::test]
    async fn refresh_uses_account_and_version_and_preserves_metadata() {
        use axum::{extract::Request, routing::get, Json, Router};
        let app = Router::new().route("/models", get(|request: Request| async move {
            assert_eq!(request.headers()["authorization"], "Bearer fake-oauth");
            assert_eq!(request.headers()["chatgpt-account-id"], "fake-account");
                    assert_eq!(request.headers()["accept-encoding"], "identity");
                    assert_eq!(request.headers()["originator"], "codex_cli_rs");
            assert_eq!(request.uri().query(), Some("client_version=0.147.0"));
            Json(json!({"models":[{"slug":"gpt-6.1-sol","visibility":"list","custom_metadata":"keep"}]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let clients = network::build_clients(&None, false).unwrap();
        let auth = OfficialAuth {
            token: "fake-oauth".into(),
            account: "fake-account".into(),
        };
        let data = fetch_official(
            &clients,
            &auth,
            Some("0.147.0"),
            &format!("http://{address}"),
        )
        .await
        .unwrap();
        assert_eq!(data["models"][0]["custom_metadata"], "keep");
        server.abort();
    }
}
