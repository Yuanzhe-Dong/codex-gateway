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
    pub fallback: reqwest::Client,
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
    Some(normalize_proxy(&server))
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
        .connect_timeout(CONNECT_TIMEOUT)
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
    let fallback = build_client(None)?;
    Ok(UpstreamClients { primary, fallback })
}

/// 判断错误是否属于"连接类"错误（连接失败/超时），可安全重试（请求未到达上游）。
pub fn is_retryable(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
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
        std::env::remove_var("HTTPS_PROXY");
        let manual = Some("http://127.0.0.1:9999".to_string());
        let r = resolved_proxy(&manual, true);
        assert_eq!(r.as_deref(), Some("http://127.0.0.1:9999"));
    }

    #[test]
    fn proxy_none_when_disabled() {
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("ALL_PROXY");
        let r = resolved_proxy(&None, false);
        assert!(r.is_none());
    }
}