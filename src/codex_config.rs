//! Codex config.toml 的管理：无损备份、写入网关配置、一键恢复原生设置。
//!
//! 原则：
//! - 首次接管时把原始 config.toml 完整备份到 ~/.codex-gateway/backups/，之后不再覆盖该备份；
//! - 修改使用 toml_edit 原地编辑，保留原文件其余段落与注释；
//! - restore/uninstall 优先还原未被污染的备份，否则主动把 config 改回原生 OpenAI，并删除模型目录；

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use toml_edit::{value, DocumentMut, Item, Table};

use crate::paths;

/// 我们写入 config.toml 的 model_catalog_json 文件名。
pub const CATALOG_FILE_NAME: &str = paths::CATALOG_FILE_NAME;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayState {
    /// 指向 backups/ 下原始 config.toml 备份文件名。
    pub backup_id: Option<String>,
    /// 当前 config.toml 是否由我们接管。
    pub managed_config: bool,
    pub installed_at: Option<String>,
    pub last_setup_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub restored_config: bool,
    pub catalog_removed: bool,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------- state ----------

pub fn load_state() -> Result<GatewayState> {
    let p = paths::state_path()?;
    if !p.exists() {
        return Ok(GatewayState::default());
    }
    let raw = fs::read_to_string(&p).context("读取网关状态失败")?;
    let s: GatewayState = serde_json::from_str(&raw).context("解析网关状态失败")?;
    Ok(s)
}

pub fn save_state(state: &GatewayState) -> Result<()> {
    let dir = paths::gateway_dir()?;
    fs::create_dir_all(&dir)?;
    let raw = serde_json::to_string_pretty(state).context("序列化网关状态失败")?;
    fs::write(paths::state_path()?, raw).context("写入网关状态失败")?;
    Ok(())
}

/// 是否正在接管 Codex 配置（以状态为准，兼容状态丢失时按目录文件名判断）。
pub fn is_managed() -> Result<bool> {
    let st = load_state()?;
    if st.managed_config {
        return Ok(true);
    }
    // 兜底：config.toml 里 model_catalog_json 指向我们的文件
    let cp = paths::codex_config_path()?;
    if cp.exists() {
        if let Ok(raw) = fs::read_to_string(&cp) {
            if raw.contains(CATALOG_FILE_NAME) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ---------- backup ----------

/// 确保存在一份"接管前"的原始备份；已有则不再覆盖。
pub fn ensure_pristine_backup() -> Result<Option<String>> {
    let mut state = load_state()?;
    if let Some(id) = &state.backup_id {
        let backup = paths::backups_dir()?.join(id);
        if backup.exists() {
            return Ok(None);
        }
    }
    let config_path = paths::codex_config_path()?;
    if !config_path.exists() {
        bail!("未找到 {}，无法备份", config_path.display());
    }
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let id = format!("config.toml.{ts}");
    let dir = paths::backups_dir()?;
    fs::create_dir_all(&dir).context("创建备份目录失败")?;
    let dest = dir.join(&id);
    fs::copy(&config_path, &dest).with_context(|| format!("备份 {} 失败", config_path.display()))?;
    state.backup_id = Some(id.clone());
    state.managed_config = true;
    state.installed_at.get_or_insert_with(now_iso);
    state.last_setup_at = Some(now_iso());
    save_state(&state)?;
    Ok(Some(id))
}

// ---------- apply ----------

/// 把网关配置写入 Codex config.toml（原地编辑，保留其余内容）。
pub fn apply_gateway_to_codex_config(port: u16, default_model: &str) -> Result<()> {
    let path = paths::codex_config_path()?;
    let mut doc = if path.exists() {
        let raw = fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
        raw.parse::<DocumentMut>().with_context(|| format!("解析 {} 失败", path.display()))?
    } else {
        DocumentMut::new()
    };

    // 顶层键
    doc["model_provider"] = value("custom");
    doc["model"] = value(default_model);
    doc["model_catalog_json"] = value(CATALOG_FILE_NAME);

    // [model_providers.custom]
    if !doc.contains_key("model_providers") {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let providers = doc["model_providers"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml 中 model_providers 不是表"))?;
    if !providers.contains_key("custom") {
        providers.insert("custom", Item::Table(Table::new()));
    }
    let custom = providers["custom"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml 中 model_providers.custom 不是表"))?;
    custom["name"] = value("deepseek");
    custom["base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    custom["wire_api"] = value("responses");
    custom["requires_openai_auth"] = value(true);
    // 移除可能干扰官方 OAuth 透传的固定 token
    custom.remove("experimental_bearer_token");

    fs::write(&path, doc.to_string()).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

// ---------- restore ----------

/// 一键恢复原生配置：优先还原「未被污染的」原始备份，否则主动把 config 改回原生 OpenAI。
///
/// 备份若本身已被第三方（如 CC Switch）污染，原样还原会把 config 再次指向失效的代理，
/// 因此这类备份会被跳过，改由 `restore_to_native_openai` 主动清理当前 config。
pub fn restore_native() -> Result<RestoreReport> {
    let mut state = load_state()?;
    let config_path = paths::codex_config_path()?;
    let mut restored_config = false;

    // 1. 若存在备份且备份本身是原生配置（未被污染），原样还原以精确恢复用户原始内容。
    if let Some(id) = &state.backup_id {
        let backup = paths::backups_dir()?.join(id);
        if backup.exists() {
            if backup_is_native(&backup)? {
                fs::copy(&backup, &config_path).with_context(|| {
                    format!("还原 {} <- {} 失败", config_path.display(), backup.display())
                })?;
                restored_config = true;
            } else {
                tracing::warn!(
                    "备份 {} 已被污染（非原生 OpenAI 配置），跳过还原，改用主动清理",
                    backup.display()
                );
            }
        } else {
            tracing::warn!("备份文件缺失: {}", backup.display());
        }
    }

    // 2. 主动清理：无论是否还原备份，都确保 config 不残留网关注入的键。
    //    对已是原生的 config 是 no-op（不改文件格式）；对污染/无备份场景则改回可用原生状态。
    //    从备份中提取用户原始模型名，用于恢复被网关改成 deepseek-* 的 model 字段。
    if config_path.exists() {
        let fallback_model = extract_backup_model()?;
        restore_to_native_openai(&config_path, fallback_model.as_deref())?;
        restored_config = true;
    }

    // 2.5 清洗会话历史 reasoning content + 同步会话 provider，让 restore 后 GPT 直连可用且会话不丢
    if let Err(e) = crate::session_sync::clean_session_reasoning() {
        tracing::warn!("清洗会话 reasoning 失败（不阻断）: {e}");
    }
    if let Err(e) = crate::session_sync::sync_session_provider("openai") {
        tracing::warn!("同步会话 provider 失败（不阻断）: {e}");
    }

    // 3. 删除我们生成的模型目录。
    let cat = paths::catalog_path()?;
    let catalog_removed = if cat.exists() {
        fs::remove_file(&cat).context("删除生成的模型目录失败")?;
        true
    } else {
        false
    };

    state.managed_config = false;
    state.backup_id = None;
    state.last_setup_at = None;
    save_state(&state)?;

    Ok(RestoreReport {
        restored_config,
        catalog_removed,
    })
}

/// 删除整个网关数据目录（`~/.codex-gateway/`）：API Key 配置、备份、状态、日志、PID。
///
/// 这是 `setup` 的逆操作——回到「从未装过 codex-gateway」的状态。
/// **仅**在 `restore_native` 成功后调用：config 已还原、会话已清洗/同步，才可安全删除。
/// 不会触碰 `~/.codex/`（会话历史、SQLite、config.toml 均保留）。
pub fn purge_gateway_data() -> Result<()> {
    let gw_dir = paths::gateway_dir()?;
    if gw_dir.exists() {
        fs::remove_dir_all(&gw_dir)
            .with_context(|| format!("删除 {} 失败", gw_dir.display()))?;
    }
    Ok(())
}

/// `stop` 时：把 config 切回原生 OpenAI，但**保留 catalog 文件与 state**（轻量切换，非卸载）。
///
/// 与 `restore_native` 的区别：不删除 catalog 文件、不清空 state，便于 `start` 时一键切回。
/// 若 config 已是原生则 no-op。返回是否实际切换。
pub fn switch_to_native_for_stop() -> Result<bool> {
    let path = paths::codex_config_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let doc = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("解析 {} 失败", path.display()))?;
    let already_native = is_native_config(&doc);
    if !already_native {
        let fallback_model = extract_backup_model().unwrap_or(None);
        restore_to_native_openai(&path, fallback_model.as_deref())?;
    }
    // 清洗会话历史里 reasoning content 非空项，避免关网关后 GPT 直连官方报 array too long
    if let Err(e) = crate::session_sync::clean_session_reasoning() {
        tracing::warn!("清洗会话 reasoning 失败（不阻断）: {e}");
    }
    // 总是同步 SQLite 会话 provider：即使 config 已是 native，会话可能不匹配
    // （例如上次 stop 时 Codex 在运行锁库导致 sync 失败），重跑 stop 会补上。
    if let Err(e) = crate::session_sync::sync_session_provider("openai") {
        tracing::warn!("同步会话 provider 失败（不阻断，Codex 可能正在运行锁库；关闭 Codex 后重跑 stop 可补上）: {e}");
    }
    Ok(!already_native)
}

/// `start` 时：确保 config 指向网关（可能被 `stop` 切回 native）。返回是否实际切换。
///
/// 若 config 已是 `custom` 且含 `model_catalog_json`，视为已接管，no-op；否则重新写入网关配置。
pub fn ensure_custom_active(port: u16, default_model: &str) -> Result<bool> {
    let path = paths::codex_config_path()?;
    let already = if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        let doc = raw
            .parse::<DocumentMut>()
            .with_context(|| format!("解析 {} 失败", path.display()))?;
        doc.get("model_provider").and_then(|v| v.as_str()) == Some("custom")
            && doc.get("model_catalog_json").is_some()
    } else {
        false
    };
    if !already {
        // 改 config 前先确保有原始备份（no-op if 已有）。
        // 关键场景：uninstall 清空了 state.backup_id 后直接 start，若不重建备份，
        // 后续 stop 时 extract_backup_model 返回 None → model 字段被删除 → Codex 无法启动。
        ensure_pristine_backup()?;
        apply_gateway_to_codex_config(port, default_model)?;
    }
    // 总是同步 SQLite 会话 provider：即使 config 已是 custom，会话可能不匹配
    // （例如上次 start 时 Codex 在运行锁库导致 sync 失败），重跑 start 会补上。
    if let Err(e) = crate::session_sync::sync_session_provider("custom") {
        tracing::warn!("同步会话 provider 失败（不阻断，Codex 可能正在运行锁库；关闭 Codex 后重跑 start 可补上）: {e}");
    }
    Ok(!already)
}

/// 判断一份 config.toml 备份是否为「原生 OpenAI 配置」（未被网关或第三方接管）。
/// 判据：`model_provider` 为 `openai` 或缺失，且不含我们注入的 `model_catalog_json`。
fn backup_is_native(path: &Path) -> Result<bool> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取备份 {} 失败", path.display()))?;
    let doc = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("解析备份 {} 失败", path.display()))?;
    Ok(is_native_config(&doc))
}

/// config 是否为原生：`model_provider` 是 `openai` 或缺失，且无 `model_catalog_json`。
fn is_native_config(doc: &DocumentMut) -> bool {
    let provider = doc.get("model_provider").and_then(|v| v.as_str());
    let provider_ok = matches!(provider, None | Some("openai"));
    let no_catalog = doc.get("model_catalog_json").is_none();
    provider_ok && no_catalog
}

/// 从备份中提取原始 `model` 字段（仅当非 deepseek 模型时返回），用于 restore/stop 时
/// 恢复用户原始模型而非直接删除。
///
/// 备份可能被第三方（如 CC Switch）污染了 `model_provider`，但 `model` 字段本身可能
/// 仍是有效的官方模型名（如 `gpt-5.6-luna`），应予以恢复。
fn extract_backup_model() -> Result<Option<String>> {
    let state = load_state()?;
    if let Some(id) = &state.backup_id {
        let backup = paths::backups_dir()?.join(id);
        if backup.exists() {
            let raw = fs::read_to_string(&backup)
                .with_context(|| format!("读取备份 {} 失败", backup.display()))?;
            let doc = raw
                .parse::<DocumentMut>()
                .with_context(|| format!("解析备份 {} 失败", backup.display()))?;
            if let Some(m) = doc.get("model").and_then(|v| v.as_str()) {
                if !m.starts_with("deepseek-") {
                    return Ok(Some(m.to_string()));
                }
            }
        }
    }
    Ok(None)
}

/// 主动把 config.toml 改回原生 OpenAI 状态：移除网关注入的键与 `[model_providers.custom]` 段。
///
/// 对已是原生的 config 是 no-op（直接返回，不重写文件，避免 toml_edit 重序列化造成格式漂移）；
/// 保留其余所有段落与注释。
///
/// `fallback_model`：从备份中提取的用户原始模型名。当当前 model 是网关注入的 deepseek-*
/// 时，优先用 fallback 恢复；无 fallback 则移除 model 字段（让 Codex 用默认值）。
fn restore_to_native_openai(path: &Path, fallback_model: Option<&str>) -> Result<()> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut doc = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("解析 {} 失败", path.display()))?;

    // 已是原生则不动（保持原文件格式）
    let needs_fix = doc.get("model_catalog_json").is_some()
        || doc.get("model_provider").and_then(|v| v.as_str()) == Some("custom")
        || doc
            .get("model")
            .and_then(|v| v.as_str())
            .map(|m| m.starts_with("deepseek-"))
            .unwrap_or(false)
        || doc
            .get("model_providers")
            .and_then(|t| t.as_table())
            .map(|t| t.contains_key("custom"))
            .unwrap_or(false);
    if !needs_fix {
        return Ok(());
    }

    doc.remove("model_catalog_json");

    // model_provider：custom 是网关用的，改回 openai（原生）；其余值不动。
    if doc.get("model_provider").and_then(|v| v.as_str()) == Some("custom") {
        doc["model_provider"] = value("openai");
    }

    // model：若指向 deepseek（网关默认模型），优先用 fallback 恢复用户原始模型；
    // 无 fallback 则移除（让 Codex 用默认值）。其余不动。
    if let Some(m) = doc.get("model").and_then(|v| v.as_str()) {
        if m.starts_with("deepseek-") {
            if let Some(fb) = fallback_model {
                doc["model"] = value(fb);
            } else {
                doc.remove("model");
            }
        }
    }

    // 移除 [model_providers.custom] 段；若 providers 表因此变空，移除整个表。
    if let Some(providers) = doc.get_mut("model_providers").and_then(|t| t.as_table_mut()) {
        providers.remove("custom");
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }

    fs::write(path, doc.to_string())
        .with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // 环境变量是进程级的，串行化避免并行测试互相干扰。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_paths(dir: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let codex = dir.path().join(".codex");
        let gw = dir.path().join(".codex-gateway");
        let config = codex.join("config.toml");
        std::env::set_var("CODEX_HOME", &codex);
        std::env::set_var("CODEX_GATEWAY_HOME", &gw);
        (codex, gw, config)
    }

    #[test]
    fn apply_preserves_other_sections_and_comment() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, _gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "# 我的配置\nmodel = \"old\"\n\n[mcp_servers]\nfoo = 1\n",
        )
        .unwrap();

        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();

        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("# 我的配置"), "注释应保留");
        assert!(raw.contains("[mcp_servers]"), "其余段落应保留");
        assert!(raw.contains("foo = 1"));
        assert!(raw.contains("model_catalog_json = \"codex-gateway-model-catalog.json\""));
        assert!(raw.contains("base_url = \"http://127.0.0.1:17899/v1\""));
        assert!(raw.contains("requires_openai_auth = true"));
        assert!(raw.contains("wire_api = \"responses\""));
    }

    #[test]
    fn apply_removes_stale_experimental_token() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_c, _g2, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "[model_providers.custom]\nexperimental_bearer_token = \"PROXY_MANAGED\"\n",
        )
        .unwrap();
        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();
        let raw = fs::read_to_string(&config).unwrap();
        assert!(!raw.contains("experimental_bearer_token"));
    }

    #[test]
    fn backup_restore_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "# 原始配置\nmodel = \"gpt-5.6-sol\"\nmodel_provider = \"openai\"\n";
        fs::write(&config, original).unwrap();

        let backup_id = ensure_pristine_backup().unwrap().unwrap();
        assert!(gw.join("backups").join(&backup_id).exists());
        assert!(ensure_pristine_backup().unwrap().is_none());

        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();
        assert!(is_managed().unwrap());

        let report = restore_native().unwrap();
        assert!(report.restored_config);
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert!(!is_managed().unwrap());
    }

    #[test]
    fn restore_skips_polluted_backup_and_cleans() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, _gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        // 模拟「首次 setup 时 config 已被 CC Switch 接管」的污染状态
        fs::write(
            &config,
            "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:15721/v1\"\nwire_api = \"responses\"\n",
        )
        .unwrap();

        // 备份的是污染状态
        ensure_pristine_backup().unwrap();
        // 再 apply 网关配置（覆盖成指向 17899）
        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();

        let report = restore_native().unwrap();
        assert!(report.restored_config);
        let raw = fs::read_to_string(&config).unwrap();
        // 不应残留任何指向本地代理的 base_url
        assert!(!raw.contains("127.0.0.1:15721"), "不应残留 CC Switch 的 base_url");
        assert!(!raw.contains("127.0.0.1:17899"), "不应残留网关的 base_url");
        assert!(!raw.contains("model_catalog_json"), "应移除 model_catalog_json");
        assert!(!raw.contains("[model_providers.custom]"), "应移除 custom provider 段");
        assert!(!raw.contains("deepseek-v4-flash"), "应移除 deepseek 模型");
        // 应回到原生 openai
    assert!(raw.contains("model_provider = \"openai\""));
    assert!(!is_managed().unwrap());
}

    #[test]
    fn restore_recovers_model_from_polluted_backup() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, _gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        // 模拟「首次 setup 时 config 已被 CC Switch 接管，但 model 是有效官方模型」
        // CC Switch 把 model_provider 改成 custom，但用户的 model = "gpt-5.6-luna" 仍有效
        fs::write(
            &config,
            "model_provider = \"custom\"\nmodel = \"gpt-5.6-luna\"\n\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:15721/v1\"\nwire_api = \"responses\"\n",
        )
        .unwrap();

        // 备份的是污染状态（provider=custom，但 model 有效）
        ensure_pristine_backup().unwrap();
        // 再 apply 网关配置（model 被改成 deepseek-v4-flash）
        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();

        let report = restore_native().unwrap();
        assert!(report.restored_config);
        let raw = fs::read_to_string(&config).unwrap();
        // 关键：model 应从备份恢复为 gpt-5.6-luna，而非被删除
        assert!(raw.contains("model = \"gpt-5.6-luna\""), "应从备份恢复原始模型");
        assert!(!raw.contains("deepseek-v4-flash"), "不应残留 deepseek 模型");
        assert!(raw.contains("model_provider = \"openai\""));
        assert!(!is_managed().unwrap());
    }

    #[test]
    fn stop_start_roundtrip_switches_config() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, _gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "# 原生\nmodel = \"gpt-5.6-luna\"\nmodel_provider = \"openai\"\n",
        )
        .unwrap();

        ensure_pristine_backup().unwrap();
        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();

        // 已是 custom + catalog 键：ensure_custom_active 应 no-op
        assert!(!ensure_custom_active(17899, "deepseek-v4-flash").unwrap());

        // stop 切回 native
        let switched = switch_to_native_for_stop().unwrap();
        assert!(switched, "应从 custom 切回 native");
        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("model_provider = \"openai\""));
        assert!(!raw.contains("model_catalog_json"));
        assert!(!raw.contains("[model_providers.custom]"));

        // 再次 stop 是 no-op
        assert!(!switch_to_native_for_stop().unwrap());

        // start 切回 custom
        let switched = ensure_custom_active(17899, "deepseek-v4-flash").unwrap();
        assert!(switched, "应从 native 切回 custom");
        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("model_provider = \"custom\""));
        assert!(raw.contains("model_catalog_json"));
        assert!(raw.contains("base_url = \"http://127.0.0.1:17899/v1\""));
    }

    #[test]
    fn uninstall_start_stop_preserves_model() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (_codex, _gw, config) = fake_paths(&dir);
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        // 原生配置，model = gpt-5.6-luna
        fs::write(
            &config,
            "model = \"gpt-5.6-luna\"\nmodel_provider = \"openai\"\n",
        )
        .unwrap();

        // setup: 建备份 + 接管
        ensure_pristine_backup().unwrap();
        apply_gateway_to_codex_config(17899, "deepseek-v4-flash").unwrap();

        // uninstall: 清空 state（backup_id = None），config 改回 native
        restore_native().unwrap();

        // 模拟「uninstall 后直接 start，不跑 setup」
        ensure_custom_active(17899, "deepseek-v4-flash").unwrap();
        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("deepseek-v4-flash"), "start 后 model 应为 deepseek");

        // 关键验证：stop 后 model 应从 start 重建的备份中恢复，而非被删除
        switch_to_native_for_stop().unwrap();
        let raw = fs::read_to_string(&config).unwrap();
        assert!(
            raw.contains("model = \"gpt-5.6-luna\""),
            "uninstall→start→stop 后 model 应恢复为 gpt-5.6-luna，实际: {raw}"
        );
    }

    #[test]
    fn purge_gateway_data_removes_directory_but_not_codex() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let (codex, gw, _config) = fake_paths(&dir);
        // 网关目录里有各种文件
        fs::create_dir_all(gw.join("backups")).unwrap();
        fs::write(gw.join("config.json"), r#"{"deepseek_api_key":"sk-xxx"}"#).unwrap();
        fs::write(gw.join("state.json"), "{}").unwrap();
        fs::write(gw.join("gateway.log"), "log").unwrap();
        // Codex 目录里有会话数据
        fs::create_dir_all(codex.join("sessions")).unwrap();
        fs::write(codex.join("sessions/keep.jsonl"), "[]").unwrap();
        fs::write(codex.join("state_5.sqlite"), "sqlite data").unwrap();

        purge_gateway_data().unwrap();

        assert!(!gw.exists(), "网关目录应被删除");
        assert!(codex.exists(), "Codex 目录不应被删除");
        assert!(codex.join("sessions/keep.jsonl").exists(), "会话历史不应被删除");
        assert!(codex.join("state_5.sqlite").exists(), "SQLite 不应被删除");
    }
}
