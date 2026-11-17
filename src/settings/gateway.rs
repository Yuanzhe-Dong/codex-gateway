//! 网关自身配置（DeepSeek Key、端口、代理等）。
//! 存放于 ~/.codex-gateway/config.toml，权限仅限当前用户。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

pub const DEFAULT_PORT: u16 = 17899;
pub const DEFAULT_DEEPSEEK_MODELS: [&str; 2] = ["deepseek-flash", "deepseek-v4-pro"];

#[derive(Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// DeepSeek API Key（仅发送到 DeepSeek，不发送到 ChatGPT）。
    pub deepseek_api_key: String,
    /// 本地代理监听端口。
    pub port: u16,
    /// 要在 Codex 菜单里暴露的 DeepSeek 模型。
    #[serde(default = "default_deepseek_models")]
    pub deepseek_models: Vec<String>,
    /// 手动指定上游代理，例如 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080。
    /// None 时是否自动检测由 auto_proxy 控制。
    #[serde(default)]
    pub proxy: Option<String>,
    /// 是否允许自动跟随系统代理 / 环境变量代理。
    #[serde(default = "default_true")]
    pub auto_proxy: bool,
}

fn default_deepseek_models() -> Vec<String> {
    DEFAULT_DEEPSEEK_MODELS
        .iter()
        .map(|s| s.to_string())
        .collect()
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
        let cfg = Self::load_for_setup()?;
        if let Some(cfg) = &cfg {
            cfg.validate()?;
        }
        Ok(cfg)
    }

    /// 向导可修正字段值错误；解析错误仍保留原文件并报告。
    pub fn load_for_setup() -> Result<Option<Self>> {
        let path = crate::runtime::paths::gateway_config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let mut cfg: GatewayConfig = toml::from_str(&raw).map_err(|_| {
            anyhow::anyhow!(
                "网关配置格式错误，请检查 {}（为避免泄漏密钥，省略原始内容）",
                path.display()
            )
        })?;
        // 保留旧配置的可用性，同时把已迁移的 Flash 别名更新到当前官方模型名。
        for model in &mut cfg.deepseek_models {
            *model = crate::proxy::routing::canonical_deepseek_model(model).to_owned();
        }
        let mut seen = std::collections::HashSet::new();
        cfg.deepseek_models
            .retain(|model| seen.insert(model.clone()));
        Ok(Some(cfg))
    }

    /// 保存到 ~/.codex-gateway/config.toml（创建目录并设置仅当前用户可读）。
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        let raw = toml::to_string_pretty(self).context("序列化网关配置失败")?;
        let path = crate::runtime::paths::gateway_config_path()?;
        crate::runtime::storage::private_write(&path, raw.as_bytes())
    }

    /// 验证手工编辑的配置，避免空模型数组越界和端口 0 随机绑定。
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("端口必须是 1-65535，不能为 0");
        }
        let key = self.deepseek_api_key.trim();
        if key.is_empty() || key.bytes().any(|b| b.is_ascii_control() || b == b' ') {
            bail!("DeepSeek API Key 不能为空或包含空白/控制字符");
        }
        if self.deepseek_models.is_empty() {
            bail!("至少选择一个 DeepSeek 模型");
        }
        let mut seen = std::collections::HashSet::new();
        for model in &self.deepseek_models {
            if !DEFAULT_DEEPSEEK_MODELS.contains(&model.as_str()) || !seen.insert(model) {
                bail!("DeepSeek 模型不受支持或重复，请重新运行 setup");
            }
        }
        if let Some(proxy) = &self.proxy {
            crate::runtime::network::validate_proxy(proxy)?;
        }
        Ok(())
    }

    /// 打码后的 Key，用于状态展示。
    pub fn masked_key(&self) -> String {
        mask_secret(&self.deepseek_api_key)
    }
}

/// 对密钥做打码：sk-abc...xyz。
pub fn mask_secret(s: &str) -> String {
    if s.trim().is_empty() {
        return "<未设置>".to_string();
    }
    let trimmed = s.trim();
    if trimmed.chars().count() <= 8 {
        return "***".to_string();
    }
    let head: String = trimmed.chars().take(4).collect();
    let tail: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}...{}", head, tail)
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
        assert!(c.deepseek_models.contains(&"deepseek-flash".to_string()));
        assert!(c.auto_proxy);
    }

    #[test]
    fn rejects_empty_models_zero_port_and_header_injection() {
        let mut cfg = GatewayConfig {
            deepseek_api_key: "sk-test".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        cfg.port = 0;
        assert!(cfg.validate().is_err());
        cfg.port = DEFAULT_PORT;
        cfg.deepseek_models.clear();
        assert!(cfg.validate().is_err());
        cfg.deepseek_models = default_deepseek_models();
        cfg.deepseek_api_key = "sk-test\r\nInjected: true".into();
        assert!(cfg.validate().is_err());
        assert_eq!(mask_secret("中文短密钥"), "***");
    }
}
