//! 合并模型目录生成：DeepSeek（内置元数据）+ 官方（从 models_cache.json 抽取）。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// 内置 DeepSeek 模型元数据（字段与已验证可用的官方/工作配置一致）。
pub fn builtin_deepseek_models() -> Vec<Value> {
    vec![
        deepseek_entry("deepseek-v4-flash", "DeepSeek V4 Flash", 1000),
        deepseek_entry("deepseek-v4-pro", "DeepSeek V4 Pro", 1001),
    ]
}

fn deepseek_entry(slug: &str, display_name: &str, priority: i64) -> Value {
    json!({
        "additional_speed_tiers": [],
        "availability_nux": null,
        "base_instructions": "You are Codex, a coding agent. You and the user share the same workspace and collaborate to achieve the user's goals.",
        "context_window": 1048576,
        "default_reasoning_level": "high",
        "default_reasoning_summary": "none",
        "description": display_name,
        "display_name": display_name,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text"],
        "max_context_window": 1048576,
        "priority": priority,
        "service_tiers": [],
        "shell_type": "shell_command",
        "slug": slug,
        "support_verbosity": false,
        "supported_in_api": true,
        "supported_reasoning_levels": [
            { "description": "Disable Thinking", "effort": "none" },
            { "description": "Enabled Thinking", "effort": "high" }
        ],
        "supports_image_detail_original": false,
        "supports_parallel_tool_calls": false,
        "supports_reasoning_summaries": true,
        "supports_search_tool": false,
        "truncation_policy": { "limit": 10000, "mode": "bytes" },
        "upgrade": null,
        "visibility": "list"
    })
}

/// 从 Codex 官方模型缓存抽取可见官方模型（跳过 deepseek-*）。
pub fn official_models_from_cache(cache_path: &Path) -> Result<Vec<Value>> {
    if !cache_path.exists() {
        bail!("模型缓存不存在: {}", cache_path.display());
    }
    let raw = fs::read_to_string(cache_path).context("读取 models_cache.json 失败")?;
    let data: Value = serde_json::from_str(&raw).context("解析 models_cache.json 失败")?;
    let models = data
        .get("models")
        .and_then(|v| v.as_array())
        .context("models_cache.json 缺少 models 数组")?;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in models {
        let Some(slug) = m.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        if slug.starts_with("deepseek-") {
            continue;
        }
        // 只暴露 list 或未标隐藏的模型
        let vis = m.get("visibility").and_then(|v| v.as_str()).unwrap_or("list");
        if vis == "hide" {
            continue;
        }
        if seen.insert(slug.to_string()) {
            out.push(m.clone());
        }
    }
    Ok(out)
}

/// 内置官方模型兜底清单（当 models_cache.json 缺失或为空时使用）。
pub fn builtin_official_fallback() -> Vec<Value> {
    let mut out = Vec::new();
    for (i, slug) in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
        .iter()
        .enumerate()
    {
        out.push(json!({
            "slug": slug,
            "display_name": slug,
            "visibility": "list",
            "supported_in_api": true,
            "priority": 100 - i as i64,
            "context_window": 272000,
            "max_context_window": 272000,
            "effective_context_window_percent": 95,
            "default_reasoning_level": "high",
            "supported_reasoning_levels": [
                { "description": "Fast responses with lighter reasoning", "effort": "low" },
                { "description": "Balances speed and reasoning depth for everyday tasks", "effort": "medium" },
                { "description": "Greater reasoning depth for complex problems", "effort": "high" },
                { "description": "Extra high reasoning depth for complex problems", "effort": "xhigh" }
            ],
            "shell_type": "shell_command",
            "supports_search_tool": true,
            "apply_patch_tool_type": "freeform",
            "web_search_tool_type": "freeform",
            "input_modalities": ["text"],
            "service_tiers": [],
            "additional_speed_tiers": [],
            "availability_nux": null,
            "upgrade": null,
            "experimental_supported_tools": [],
            "support_verbosity": false,
            "supports_image_detail_original": false,
            "supports_parallel_tool_calls": true,
            "truncation_policy": { "limit": 30000, "mode": "bytes" }
        }));
    }
    out
}

/// 生成合并模型目录 Value：DeepSeek 条目在前，官方条目在后。
pub fn build_catalog(include_deepseek: &[String], official: Vec<Value>) -> Value {
    let mut models = Vec::new();
    for slug in include_deepseek {
        if let Some(entry) = builtin_deepseek_models().into_iter().find(|m| {
            m.get("slug").and_then(|v| v.as_str()) == Some(slug.as_str())
        }) {
            models.push(entry);
        }
    }
    models.extend(official);
    json!({ "models": models })
}

/// 把合并目录写入 CODEX_HOME 下的 codex-gateway-model-catalog.json。
pub fn write_catalog_file(catalog: &Value, target: &Path) -> Result<()> {
    let raw = serde_json::to_string_pretty(catalog).context("序列化模型目录失败")?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(target, raw).with_context(|| format!("写入 {} 失败", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_flash_and_pro() {
        let models = builtin_deepseek_models();
        let slugs: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("slug").and_then(|v| v.as_str()))
            .collect();
        assert!(slugs.contains(&"deepseek-v4-flash"));
        assert!(slugs.contains(&"deepseek-v4-pro"));
    }

    #[test]
    fn build_catalog_order_and_filter() {
        let official = builtin_official_fallback();
        let catalog = build_catalog(&["deepseek-v4-flash".to_string()], official.clone());
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 1 + official.len());
        assert_eq!(models[0]["slug"], "deepseek-v4-flash");
        assert_eq!(models[1]["slug"], official[0]["slug"]);
    }

    #[test]
    fn fallback_has_official_slugs() {
        let f = builtin_official_fallback();
        assert!(f.iter().any(|m| m["slug"] == "gpt-5.6-sol"));
    }
}