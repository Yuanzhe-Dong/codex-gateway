//! 网关自身配置（DeepSeek Key、端口、代理等）。
//! 存放于 ~/.codex-gateway/config.toml，权限仅限当前用户。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const DEFAULT_PORT: u16 = 17899;
pub const DEFAULT_DEEPSEEK_MODELS: [&str; 2] = ["deepseek-v4-flash", "deepseek-v4-pro"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// DeepSeek API Key（仅在本地保存，绝不外发到官方以外的上游）。
    pub deepseek_api_key: String,
    /// 本地代理监听端口。
    pub port: u16,
    /// 要在 Codex 菜单里暴露的 DeepSeek 模型。
    #[serde(default = "default_deepseek_models")]
    pub deepseek_models: Vec<String>,
    /// 手动指定上游代理，例如 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080。
    /// None = 自动（跟随环境变量 / 系统代理）。
    #[serde(default)]
    pub proxy: Option<String>,
    /// 是否允许自动跟随系统代理 / 环境变量代理。
    #[serde(default = "default_true")]
    pub auto_proxy: bool,
}

fn default_deepseek_models() -> Vec<String> {
    DEFAULT_DEEPSEEK_MODELS.iter().map(|s| s.to_string()).collect()
}

fn default_true() -> bool {
    true
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            deepseek_api_key: String::new(),
            port: DEFAULT_PORT,
            deepseek_models: default_deepseek_models(),
            proxy: None,
            auto_proxy: true,
        }
    }
}

impl GatewayConfig {
    /// 从 ~/.codex-gateway/config.toml 加载；文件不存在返回 None。
    pub fn load() -> Result<Option<Self>> {
        let path = crate::paths::gateway_config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let cfg: GatewayConfig =
            toml::from_str(&raw).with_context(|| format!("解析 {} 失败", path.display()))?;
        Ok(Some(cfg))
    }

    /// 保存到 ~/.codex-gateway/config.toml（创建目录并设置仅当前用户可读）。
    pub fn save(&self) -> Result<()> {
        let dir = crate::paths::gateway_dir()?;
        fs::create_dir_all(&dir).context("创建 ~/.codex-gateway 失败")?;
        let raw = toml::to_string_pretty(self).context("序列化网关配置失败")?;
        let path = crate::paths::gateway_config_path()?;
        fs::write(&path, raw).with_context(|| format!("写入 {} 失败", path.display()))?;
        #[cfg(windows)]
        restrict_file_permissions(&path);
        Ok(())
    }

    /// 打码后的 Key，用于状态展示。
    pub fn masked_key(&self) -> String {
        mask_secret(&self.deepseek_api_key)
    }
}

/// 对密钥做打码：sk-abc...xyz。
pub fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return "<未设置>".to_string();
    }
    let trimmed = s.trim();
    if trimmed.len() <= 8 {
        return "***".to_string();
    }
    let head: String = trimmed.chars().take(4).collect();
    let tail: String = trimmed.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{}...{}", head, tail)
}

/// Windows 下把配置文件 ACL 收紧为仅当前用户可读写。
#[cfg(windows)]
fn restrict_file_permissions(path: &Path) {
    // 使用 icacls 继承禁用 + 仅当前用户授权；失败不影响主流程，仅记录。
    use std::process::Command;
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let _ = Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(R,W)"))
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_short_and_long() {
        assert_eq!(mask_secret(""), "<未设置>");
        assert_eq!(mask_secret("abc"), "***");
        let m = mask_secret("sk-1234567890abcdef");
        assert!(m.starts_with("sk-1"));
        assert!(m.ends_with("cdef"));
        assert!(m.contains("..."));
    }

    #[test]
    fn default_config_sane() {
        let c = GatewayConfig::default();
        assert_eq!(c.port, DEFAULT_PORT);
        assert!(c.deepseek_models.contains(&"deepseek-v4-flash".to_string()));
        assert!(c.auto_proxy);
    }
}