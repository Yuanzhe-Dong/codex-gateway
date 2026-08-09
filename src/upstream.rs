//! 上游定义与模型名 → 上游的映射。
//! 双方均为原生 Responses API，网关只改 URL 与 Authorization，不做协议转换。

/// DeepSeek 官方 Responses API base URL。
pub const DEEPSEEK_BASE: &str = "https://api.deepseek.com";
/// ChatGPT 官方 Codex 反代 base URL。
pub const OFFICIAL_BASE: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    DeepSeek,
    Official,
}

/// 按模型名判断路由目标：deepseek-* 走 DeepSeek，其余走官方。
pub fn classify_model(model: &str) -> Upstream {
    if model.trim_start().starts_with("deepseek-") {
        Upstream::DeepSeek
    } else {
        Upstream::Official
    }
}

/// 去掉入站路径的 /v1 前缀（Codex 配置 base_url 以 /v1 结尾）。
pub fn strip_v1(path: &str) -> String {
    if path == "/v1" {
        return "/".to_string();
    }
    if let Some(rest) = path.strip_prefix("/v1") {
        if rest.is_empty() {
            "/".to_string()
        } else {
            rest.to_string()
        }
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_models() {
        assert_eq!(classify_model("deepseek-v4-flash"), Upstream::DeepSeek);
        assert_eq!(classify_model("deepseek-v4-pro"), Upstream::DeepSeek);
        assert_eq!(classify_model("gpt-5.6-sol"), Upstream::Official);
        assert_eq!(classify_model(""), Upstream::Official);
    }

    #[test]
    fn strip_v1_paths() {
        assert_eq!(strip_v1("/v1/responses"), "/responses");
        assert_eq!(strip_v1("/v1/models"), "/models");
        assert_eq!(strip_v1("/responses"), "/responses");
        assert_eq!(strip_v1("/v1"), "/");
        assert_eq!(strip_v1("/"), "/");
    }

}