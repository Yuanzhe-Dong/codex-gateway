//! 合并模型目录生成：DeepSeek（内置元数据）+ 官方（从 models_cache.json 抽取）。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// 内置 DeepSeek 模型元数据（字段与已验证可用的官方/工作配置一致）。
pub fn builtin_deepseek_models() -> Vec<Value> {
    vec![
        deepseek_entry("deepseek-flash", "DeepSeek V4.1 Flash (1017)", 1000),
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
        "input_modalities": if slug == "deepseek-flash" { json!(["text", "image"]) } else { json!(["text"]) },
        "max_context_window": 1048576,
        "priority": priority,
        "service_tiers": [],
        "shell_type": "shell_command",
        "apply_patch_tool_type": "freeform",
        "slug": slug,
        "support_verbosity": false,
        "supported_in_api": true,
        "supported_reasoning_levels": [
            { "description": "Disable Thinking", "effort": "none" },
            { "description": "Light Thinking", "effort": "low" },
            { "description": "Enabled Thinking", "effort": "high" },
            { "description": "Maximum Reasoning", "effort": "xhigh" }
        ],
        "supports_image_detail_original": slug == "deepseek-flash",
        "supports_parallel_tool_calls": true,
        "supports_reasoning_summaries": false,
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
    official_models_from_value(&data)
}

/// 在线响应和本地缓存使用相同过滤规则，保留上游提供的完整能力元数据。
pub fn official_models_from_value(data: &Value) -> Result<Vec<Value>> {
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
        // 保留隐藏的内部模型；菜单可见性由客户端根据 visibility 控制。
        if seen.insert(slug.to_string()) {
            out.push(m.clone());
        }
    }
    if out.is_empty() {
        bail!("模型缓存没有可见官方模型");
    }
    Ok(out)
}

/// 合并原生缓存与账号快照：较新的来源作为主目录，较旧来源补回其中缺失的
/// 模型；同一 slug 以较新来源的元数据为准。
///
/// Codex 的 `models_cache.json` 可能只保存当前“推荐集”，并不包含账号可用的
/// 全部前沿模型。若整体采用较新文件，刚在线获取的 GPT-6 会被这份较窄的缓存
/// 覆盖。两个来源都来自同一 Codex 登录态；刷新失败也不会删除已有目录。
pub fn latest_official_models(native: &Path, snapshot: &Path) -> Vec<Value> {
    let modified = |path: &Path| fs::metadata(path).and_then(|m| m.modified()).ok();
    let candidates = if modified(snapshot) > modified(native) {
        [snapshot, native]
    } else {
        [native, snapshot]
    };
    let mut merged = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in candidates {
        if path.exists() {
            match official_models_from_cache(path) {
                Ok(models) => {
                    for model in models {
                        let Some(slug) = model.get("slug").and_then(Value::as_str) else {
                            continue;
                        };
                        if seen.insert(slug.to_string()) {
                            merged.push(model);
                        }
                    }
                }
                Err(error) => tracing::warn!("官方模型缓存不可用：{error}"),
            }
        }
    }
    if !merged.is_empty() {
        return complete_official_models(merged);
    }
    tracing::warn!("未找到可用官方模型缓存，使用有限的内置清单；账号权限请以在线刷新为准");
    builtin_official_fallback()
}

/// 缓存有时仅含旧模型，补充已知的 6.x 选项。同名账号元数据（包括 hide）优先。
/// 菜单元数据不代表账号授权，上游仍决定实际可用性。
pub fn complete_official_models(mut official: Vec<Value>) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = official
        .iter()
        .filter_map(|m| m["slug"].as_str().map(str::to_owned))
        .collect();
    for model in builtin_official_fallback() {
        let slug = model["slug"].as_str().unwrap();
        if slug.starts_with("gpt-6") && seen.insert(slug.to_owned()) {
            official.push(model);
        }
    }
    official
}

pub fn current_catalog(include_deepseek: &[String]) -> Result<Value> {
    Ok(build_catalog(
        include_deepseek,
        latest_official_models(
            &crate::runtime::paths::codex_models_cache_path()?,
            &crate::runtime::paths::official_models_path()?,
        ),
    ))
}

/// 内置官方模型兜底清单（当 models_cache.json 缺失或为空时使用）。
pub fn builtin_official_fallback() -> Vec<Value> {
    let mut out = Vec::new();
    for (i, slug) in [
        "gpt-6.1-sol",
        "gpt-6.1-terra",
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
    ]
    .iter()
    .enumerate()
    {
        out.push(json!({
            "slug": slug,
            "display_name": slug,
            "visibility": "list",
            "supported_in_api": true,
            "priority": 100 - i as i64,
            // API 上限已核对官方文档；在线可用时使用账号目录给出的实际 Codex 限制。
            "context_window": if slug.starts_with("gpt-6") { 1050000 } else { 272000 },
            "max_context_window": if slug.starts_with("gpt-6") { 1050000 } else { 272000 },
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
            "web_search_tool_type": "text",
            "input_modalities": if slug.starts_with("gpt-6") { json!(["text", "image"]) } else { json!(["text"]) },
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
    for model in &mut out {
        if model["slug"]
            .as_str()
            .is_some_and(|s| s.starts_with("gpt-6"))
        {
            // Sol/Astra 不支持 none；Luna 支持，避免给整代模型套同一枚举。
            if model["slug"] == "gpt-6-luna"
                || model["slug"] == "gpt-6-sol"
                || model["slug"] == "gpt-6.1-terra"
            {
                model["supported_reasoning_levels"]
                    .as_array_mut()
                    .unwrap()
                    .insert(
                        0,
                        json!({"description":"Disable thinking", "effort":"none"}),
                    );
            }
            model["default_reasoning_level"] = json!("medium");
        }
    }
    out
}

/// 生成合并模型目录 Value：DeepSeek 条目在前，官方条目在后。
pub fn build_catalog(include_deepseek: &[String], official: Vec<Value>) -> Value {
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for slug in include_deepseek {
        let slug = crate::proxy::routing::canonical_deepseek_model(slug);
        if !seen.insert(slug.to_string()) {
            continue;
        }
        if let Some(entry) = builtin_deepseek_models()
            .into_iter()
            .find(|m| m.get("slug").and_then(|v| v.as_str()) == Some(slug))
        {
            models.push(entry);
        }
    }
    models.extend(official.into_iter().filter(|m| {
        m.get("slug")
            .and_then(Value::as_str)
            .is_some_and(|slug| seen.insert(slug.to_string()))
    }));
    for model in &mut models {
        normalize_reasoning_levels(model);
    }
    json!({ "models": models })
}

/// 补齐目录字段并去重；当前桌面客户端支持 max/ultra，不再降级官方档位。
fn normalize_reasoning_levels(model: &mut Value) {
    // 新版账号缓存可能省略旧客户端要求的字段；仅为输出目录补默认值。
    if let Some(object) = model.as_object_mut() {
        let name = object.get("slug").cloned().unwrap_or(json!(""));
        object.entry("description").or_insert(name.clone());
        object.entry("display_name").or_insert(name);
        // 部分目录只返回变更字段；单个条目缺少必需字段不能拖垮整个模型菜单。
        for (key, default) in json!({
            "shell_type":"shell_command", "visibility":"list", "priority":100,
            "supported_in_api":true, "support_verbosity":false,
            "default_reasoning_level":"high",
            "supported_reasoning_levels":[{"effort":"high","description":"High"}],
            "truncation_policy":{"mode":"bytes","limit":10000},
            "input_modalities":["text"], "additional_speed_tiers":[],
            "service_tiers":[], "experimental_supported_tools":[]
        })
        .as_object()
        .unwrap()
        {
            object.entry(key.clone()).or_insert_with(|| default.clone());
        }
        object
            .entry("base_instructions")
            .or_insert(json!("You are Codex, a coding agent."));
        object
            .entry("default_reasoning_summary")
            .or_insert(json!("none"));
        object
            .entry("supports_reasoning_summaries")
            .or_insert(json!(false));
    }
    if let Some(levels) = model
        .get_mut("supported_reasoning_levels")
        .and_then(Value::as_array_mut)
    {
        let mut seen = std::collections::HashSet::new();
        levels.retain_mut(|level| {
            let effort = level["effort"].as_str().unwrap_or("");
            !effort.trim().is_empty() && seen.insert(effort.to_owned())
        });
    }
}

/// 把合并目录写入 CODEX_HOME 下的 codex-gateway-model-catalog.json。
pub fn write_catalog_file(catalog: &Value, target: &Path) -> Result<()> {
    let raw = serde_json::to_string_pretty(catalog).context("序列化模型目录失败")?;
    crate::runtime::storage::atomic_write(target, raw.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_advertised_future_efforts_are_preserved_and_deduplicated() {
        let source = json!({"slug":"future-model", "default_reasoning_level":"future-effort", "supported_reasoning_levels":[
            {"effort":"future-effort","description":"Server supplied"},
            {"effort":"future-effort","description":"duplicate"}, {"effort":""}
        ]});
        let out = build_catalog(&[], vec![source.clone()]);
        assert_eq!(out["models"][0]["default_reasoning_level"], "future-effort");
        assert_eq!(
            out["models"][0]["supported_reasoning_levels"],
            json!([source["supported_reasoning_levels"][0]])
        );
    }

    #[test]
    fn catalog_preserves_advanced_efforts_without_changing_account_snapshot() {
        let source = json!({"slug":"gpt-6.1-terra", "default_reasoning_level":"max",
            "supported_reasoning_levels":[{"effort":"none"},{"effort":"xhigh"},
            {"effort":"max"},{"effort":"ultra"}], "account_metadata":"keep"});
        let catalog = build_catalog(&["deepseek-flash".into()], vec![source.clone()]);
        let terra = &catalog["models"][1];
        assert_eq!(
            terra["supported_reasoning_levels"],
            source["supported_reasoning_levels"]
        );
        assert_eq!(terra["default_reasoning_level"], "max");
        assert_eq!(terra["account_metadata"], "keep");
        assert_eq!(terra["supports_reasoning_summaries"], false);
        assert_eq!(source["default_reasoning_level"], "max");
        assert_eq!(
            source["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(catalog["models"][0]["slug"], "deepseek-flash");
    }

    #[test]
    fn narrow_cache_keeps_frontier_choices_without_overriding_hidden_account_entries() {
        let official = complete_official_models(vec![
            json!({"slug":"gpt-5.6-sol","visibility":"list"}),
            json!({"slug":"gpt-6-astra","visibility":"hide","priority":42}),
        ]);
        let catalog = build_catalog(
            &["deepseek-flash".into(), "deepseek-v4-pro".into()],
            official,
        );
        let models = catalog["models"].as_array().unwrap();
        for slug in [
            "deepseek-flash",
            "deepseek-v4-pro",
            "gpt-5.6-sol",
            "gpt-6.1-sol",
            "gpt-6.1-terra",
            "gpt-6-sol",
            "gpt-6-luna",
        ] {
            assert_eq!(models.iter().filter(|m| m["slug"] == slug).count(), 1);
        }
        let astra = models.iter().find(|m| m["slug"] == "gpt-6-astra").unwrap();
        assert_eq!(astra["visibility"], "hide");
        assert_eq!(astra["priority"], 42);
    }

    #[test]
    fn builtin_has_flash_and_pro() {
        let models = builtin_deepseek_models();
        let slugs: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("slug").and_then(|v| v.as_str()))
            .collect();
        assert!(slugs.contains(&"deepseek-flash"));
        assert!(slugs.contains(&"deepseek-v4-pro"));
    }

    #[test]
    fn deepseek_supports_none_high_xhigh() {
        let models = builtin_deepseek_models();
        for m in &models {
            let levels = m["supported_reasoning_levels"].as_array().unwrap();
            let efforts: Vec<&str> = levels
                .iter()
                .filter_map(|l| l.get("effort").and_then(|e| e.as_str()))
                .collect();
            // xhigh 在 UI 占位「最高档」，由网关映射为 DeepSeek 的 max
            assert_eq!(efforts, vec!["none", "low", "high", "xhigh"]);
        }
    }

    #[test]
    fn build_catalog_order_and_filter() {
        let official = builtin_official_fallback();
        let catalog = build_catalog(&["deepseek-v4-flash".to_string()], official.clone());
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 1 + official.len());
        assert_eq!(models[0]["slug"], "deepseek-flash");
        assert_eq!(models[1]["slug"], official[0]["slug"]);
    }

    #[test]
    fn fallback_has_official_slugs() {
        let f = builtin_official_fallback();
        assert!(f.iter().any(|m| m["slug"] == "gpt-5.6-sol"));
        for slug in [
            "gpt-6.1-sol",
            "gpt-6.1-terra",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
        ] {
            let m = f.iter().find(|m| m["slug"] == slug).unwrap();
            assert_eq!(m["context_window"], 1050000);
            assert_eq!(m["input_modalities"], json!(["text", "image"]));
            let efforts: Vec<&str> = m["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|level| level["effort"].as_str().unwrap())
                .collect();
            assert!(efforts.contains(&"xhigh"));
            assert!(!efforts.contains(&"max"));
            assert_eq!(
                efforts.contains(&"none"),
                matches!(slug, "gpt-6-sol" | "gpt-6-luna" | "gpt-6.1-terra")
            );
        }
    }

    #[test]
    fn visible_account_metadata_is_authoritative_and_legacy_flash_names_deduplicate() {
        let data = json!({"models":[
            {"slug":"gpt-6.1-sol","visibility":"list","context_window":272000,"custom":"keep"},
            {"slug":"not-entitled","visibility":"hide"},
            {"slug":"deepseek-flash","visibility":"list"}
        ]});
        let official = official_models_from_value(&data).unwrap();
        assert_eq!(official.len(), 2);
        let catalog = build_catalog(
            &[
                "deepseek-v4-flash".into(),
                "deepseek-v4-pro".into(),
                "deepseek-v4-flash-vision-exp".into(),
            ],
            official,
        );
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 4);
        assert_eq!(models[2]["context_window"], 272000);
        assert_eq!(models[2]["custom"], "keep");
        assert_eq!(models[0]["display_name"], "DeepSeek V4.1 Flash (1017)");
    }

    #[test]
    fn broken_newer_snapshot_keeps_valid_native_cache() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("native.json");
        let snapshot = dir.path().join("snapshot.json");
        fs::write(&native, r#"{"models":[{"slug":"native-account-model"}]}"#).unwrap();
        fs::write(&snapshot, "broken JSON").unwrap();
        assert_eq!(
            latest_official_models(&native, &snapshot)[0]["slug"],
            "native-account-model"
        );
    }

    #[test]
    fn merge_restores_frontier_model_missing_from_newer_recommended_cache() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("native.json");
        let snapshot = dir.path().join("snapshot.json");
        fs::write(
            &native,
            r#"{"models":[{"slug":"gpt-5.6-sol","display_name":"newer metadata"}]}"#,
        )
        .unwrap();
        fs::write(
            &snapshot,
            r#"{"models":[{"slug":"gpt-6.1-sol"},{"slug":"gpt-5.6-sol","display_name":"older metadata"},{"slug":"gpt-6-astra"}]}"#,
        )
        .unwrap();
        let file = fs::File::options().write(true).open(&native).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        fs::File::options()
            .write(true)
            .open(&snapshot)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        let models = latest_official_models(&native, &snapshot);
        assert_eq!(
            models
                .iter()
                .take(3)
                .map(|model| model["slug"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-6.1-sol", "gpt-6-astra"]
        );
        assert_eq!(models[0]["display_name"], "newer metadata");
    }
}
