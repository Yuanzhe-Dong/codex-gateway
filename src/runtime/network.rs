//! 网络适配：直连 / 环境变量代理 / Windows 系统代理 / 手动代理，自动降级重试。

use anyhow::Result;
use std::time::Duration;

/// 连接超时：网络切换时快速失败并自动重试。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct UpstreamClients {
    /// 主客户端（按解析出的代理模式构建）。
    pub primary: reqwest::Client,
    /// 兜底客户端（直连），主客户端连接失败时自动降级使用。
    pub fallback: Option<reqwest::Client>,
}

/// 读取环境变量代理。
pub fn env_proxy() -> Option<String> {
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 读取 Windows 系统代理（IE/WinINET 设置）。
#[cfg(windows)]
pub fn system_proxy() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    let server = server.trim().to_string();
    if server.is_empty() {
        return None;
    }
    system_proxy_server(&server)
}

/// WinINET 可保存为 http=host:port;https=host:port，出站 HTTPS 优先选 https。
pub fn system_proxy_server(server: &str) -> Option<String> {
    if !server.contains('=') {
        return (!server.trim().is_empty()).then(|| normalize_proxy(server));
    }
    for protocol in ["https", "http", "socks"] {
        for part in server.split(';') {
            if let Some((name, address)) = part.split_once('=') {
                if name.trim().eq_ignore_ascii_case(protocol) && !address.trim().is_empty() {
                    return Some(if protocol == "socks" && !address.contains("://") {
                        format!("socks5://{}", address.trim())
                    } else {
                        normalize_proxy(address)
                    });
                }
            }
        }
    }
    None
}

#[cfg(not(windows))]
pub fn system_proxy() -> Option<String> {
    None
}

/// 补全协议前缀：127.0.0.1:7890 → http://127.0.0.1:7890。
pub fn normalize_proxy(p: &str) -> String {
    let p = p.trim();
    if p.contains("://") {
        p.to_string()
    } else {
        format!("http://{p}")
    }
}

/// 解析最终生效的代理：
/// 手动指定 > 环境变量 > Windows 系统代理（auto_proxy 开启时）> 直连。
pub fn resolved_proxy(manual: &Option<String>, auto_proxy: bool) -> Option<String> {
    if let Some(p) = manual {
        let p = p.trim();
        if !p.is_empty() {
            return Some(normalize_proxy(p));
        }
    }
    if auto_proxy {
        if let Some(p) = env_proxy() {
            return Some(p);
        }
        if let Some(p) = system_proxy() {
            return Some(p);
        }
    }
    None
}

pub fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        // 不允许 reqwest 在「强制直连」或直连兜底时再次读取环境代理。
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(Duration::from_secs(600))
        .tcp_keepalive(Duration::from_secs(30));
    if let Some(p) = proxy {
        let prx = reqwest::Proxy::all(p)?;
        builder = builder.proxy(prx);
    }
    Ok(builder.build()?)
}

/// 构建主/兜底两个客户端：主 = 按配置代理，兜底 = 直连。
pub fn build_clients(manual: &Option<String>, auto_proxy: bool) -> Result<UpstreamClients> {
    let resolved = resolved_proxy(manual, auto_proxy);
    let primary = build_client(resolved.as_deref())?;
    // 手动代理是用户指定的网络边界，不自动绕过；自动模式才允许直连兜底。
    let fallback = if auto_proxy && manual.is_none() && resolved.is_some() {
        Some(build_client(None)?)
    } else {
        None
    };
    Ok(UpstreamClients { primary, fallback })
}

/// 判断错误是否属于"连接类"错误（连接失败/超时），可安全重试（请求未到达上游）。
pub fn is_retryable(err: &reqwest::Error) -> bool {
    // 读取响应超时可能已触发生成/计费，不可重发 POST。
    err.is_connect()
}

pub fn validate_proxy(proxy: &str) -> Result<()> {
    let url = reqwest::Url::parse(&normalize_proxy(proxy))
        .map_err(|_| anyhow::anyhow!("代理地址格式错误"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            && url.host_str().is_some()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "代理地址应为 http(s)://主机:端口或 socks5(h)://主机:端口"
    );
    Ok(())
}

/// 展示地址时删除用户名与密码，避免状态输出泄漏代理凭证。
pub fn display_proxy(proxy: &str) -> String {
    match reqwest::Url::parse(&normalize_proxy(proxy)) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Err(_) => "<代理地址格式错误>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_proxy_scheme() {
        assert_eq!(normalize_proxy("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            normalize_proxy("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
    }

    #[test]
    fn proxy_precedence_manual_over_env() {
        // 手动代理优先于环境变量
        let manual = Some("http://127.0.0.1:9999".to_string());
        let r = resolved_proxy(&manual, true);
        assert_eq!(r.as_deref(), Some("http://127.0.0.1:9999"));
    }

    #[test]
    fn proxy_none_when_disabled() {
        let r = resolved_proxy(&None, false);
        assert!(r.is_none());
    }

    #[test]
    fn parses_wininet_and_redacts_proxy_credentials() {
        assert_eq!(
            system_proxy_server("http=127.0.0.1:1;https=127.0.0.1:2").as_deref(),
            Some("http://127.0.0.1:2")
        );
        assert_eq!(
            system_proxy_server("socks=127.0.0.1:3").as_deref(),
            Some("socks5://127.0.0.1:3")
        );
        assert!(!display_proxy("http://user:secret@localhost:7890").contains("secret"));
        assert!(validate_proxy("file:///tmp/proxy").is_err());
        assert!(validate_proxy("").is_err());
    }

    #[tokio::test]
    async fn never_follows_redirects_with_credentials() {
        let app = axum::Router::new().fallback(|| async {
            axum::http::Response::builder()
                .status(307)
                .header("location", "http://127.0.0.1:1/secret")
                .body(axum::body::Body::empty())
                .unwrap()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = build_client(None)
            .unwrap()
            .post(format!("http://{addr}"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 307);
        server.abort();
    }

    #[test]
    fn manual_proxy_has_no_direct_fallback() {
        let clients = build_clients(&Some("http://127.0.0.1:7890".into()), true).unwrap();
        assert!(clients.fallback.is_none());
        assert!(build_clients(&None, false).unwrap().fallback.is_none());
    }
}
