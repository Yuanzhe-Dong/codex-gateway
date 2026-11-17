//! 只管理网关拥有的配置字段；恢复时合并备份，保留用户后来增加的设置。
use crate::runtime::paths;
use crate::runtime::storage;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{value, DocumentMut, Item, Table};

pub const PROVIDER: &str = "codex_gateway";
pub const CATALOG_FILE_NAME: &str = paths::CATALOG_FILE_NAME;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GatewayState {
    pub backup_id: Option<String>,
    pub managed_config: bool,
    pub installed_at: Option<String>,
    pub last_setup_at: Option<String>,
    pub codex_home: Option<PathBuf>,
}

pub struct RestoreReport {
    pub restored_config: bool,
    pub catalog_removed: bool,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    match fs::read_to_string(path) {
        Ok(raw) => raw.parse().map_err(|e: toml_edit::TomlError| {
            anyhow::anyhow!(
                "解析 {} 失败（字节位置 {:?}；不回显可能含密钥的配置内容）",
                path.display(),
                e.span()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e).with_context(|| format!("读取 {} 失败", path.display())),
    }
}

pub fn load_state() -> Result<GatewayState> {
    let path = paths::state_path()?;
    let state: GatewayState = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("解析网关状态失败")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GatewayState::default(),
        Err(e) => return Err(e).context("读取网关状态失败"),
    };
    if let Some(id) = &state.backup_id {
        validate_backup_id(id)?;
    }
    if let Some(home) = &state.codex_home {
        let current = paths::codex_home()?;
        let same = fs::canonicalize(home).unwrap_or_else(|_| home.clone())
            == fs::canonicalize(&current).unwrap_or(current);
        anyhow::ensure!(
            same,
            "CODEX_HOME 与原始备份所属目录不一致；请使用对应的 CODEX_GATEWAY_HOME"
        );
    }
    Ok(state)
}

fn validate_backup_id(id: &str) -> Result<()> {
    // 状态文件是可编辑的外部输入，备份名不得逃逸到目录之外。
    anyhow::ensure!(
        id.starts_with("config.toml.")
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
        "备份文件名不合法"
    );
    Ok(())
}

pub fn save_state(state: &GatewayState) -> Result<()> {
    let mut state = state.clone();
    state.codex_home.get_or_insert(paths::codex_home()?);
    storage::atomic_write(&paths::state_path()?, &serde_json::to_vec_pretty(&state)?)
}

fn own_catalog(doc: &DocumentMut) -> bool {
    doc.get("model_catalog_json")
        .and_then(Item::as_str)
        .is_some_and(|p| {
            Path::new(p).file_name().and_then(|s| s.to_str()) == Some(CATALOG_FILE_NAME)
        })
}

fn legacy_active(doc: &DocumentMut) -> bool {
    legacy_table_owned(doc) && doc.get("model_provider").and_then(Item::as_str) == Some("custom")
}

fn own_openai_route(doc: &DocumentMut) -> bool {
    let route = doc.get("openai_base_url").and_then(Item::as_str);
    let alias = doc.get("model_providers").and_then(|p| p.get(PROVIDER));
    alias.is_some_and(|p| {
        p.get("name").and_then(Item::as_str) == Some("codex-gateway")
            && route.is_some_and(|url| {
                url.starts_with("http://127.0.0.1:")
                    && p.get("base_url").and_then(Item::as_str) == Some(url)
            })
    })
}

fn gateway_active(doc: &DocumentMut) -> bool {
    // 停用后保留的官方直连别名不是接管状态，卸载后可据此重新安装。
    let direct_alias = doc
        .get("model_providers")
        .and_then(|p| p.get(PROVIDER))
        .is_some_and(|p| {
            p.get("name").and_then(Item::as_str) == Some("codex-gateway")
                && p.get("base_url").and_then(Item::as_str)
                    == Some(crate::proxy::routing::OFFICIAL_BASE)
        });
    (doc.get("model_provider").and_then(Item::as_str) == Some(PROVIDER) && !direct_alias)
        || legacy_active(doc)
        || own_openai_route(doc)
}

pub fn is_managed() -> Result<bool> {
    // 状态中的 managed_config 可能在 stop 后过时，以当前配置实际值为准。
    let doc = read_doc(&paths::codex_config_path()?)?;
    Ok(gateway_active(&doc))
}

pub fn ensure_pristine_backup() -> Result<Option<String>> {
    let mut state = load_state()?;
    if let Some(id) = &state.backup_id {
        if paths::backups_dir()?.join(id).is_file() {
            return Ok(None);
        }
        bail!("原始备份缺失；请恢复备份后再修改 Codex 配置");
    }
    let config = paths::codex_config_path()?;
    if gateway_active(&read_doc(&config)?) {
        bail!("当前配置已由网关接管，但原始备份丢失；不能把接管后的配置当作原始备份");
    }
    let original = match fs::read(&config) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).context("读取原始配置失败"),
    };
    let id = format!(
        "config.toml.{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%f")
    );
    // 原始配置也可能含第三方密钥，备份使用与 API Key 相同的私有权限。
    storage::private_write(&paths::backups_dir()?.join(&id), &original)?;
    state.backup_id = Some(id.clone());
    state.installed_at.get_or_insert_with(now_iso);
    save_state(&state)?;
    Ok(Some(id))
}

fn provider_table(port: u16) -> Item {
    let mut table = Table::new();
    table["name"] = value("codex-gateway");
    table["base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    table["wire_api"] = value("responses");
    table["requires_openai_auth"] = value(true);
    Item::Table(table)
}

fn legacy_table_owned(doc: &DocumentMut) -> bool {
    let custom = doc.get("model_providers").and_then(|p| p.get("custom"));
    own_catalog(doc)
        && custom.is_some_and(|c| {
            c.get("name").and_then(Item::as_str) == Some("deepseek")
                && c.get("base_url")
                    .and_then(Item::as_str)
                    .is_some_and(|url| url.starts_with("http://127.0.0.1:"))
        })
}

fn apply_doc(doc: &mut DocumentMut, port: u16, model: &str) -> Result<()> {
    anyhow::ensure!(
        port != 0 && !model.trim().is_empty(),
        "端口和默认模型不合法"
    );
    let legacy_owned = legacy_table_owned(doc);
    if let Some(existing) = doc.get("model_providers").and_then(|p| p.get(PROVIDER)) {
        anyhow::ensure!(
            existing.get("name").and_then(Item::as_str) == Some("codex-gateway"),
            "model_providers.codex_gateway 已被其他配置占用"
        );
    }
    if !doc.contains_key("model_providers") {
        doc["model_providers"] = Item::Table(Table::new());
    }
    let providers = doc["model_providers"]
        .as_table_mut()
        .context("model_providers 必须是表")?;
    if legacy_owned {
        providers.remove("custom");
    }
    providers.insert(PROVIDER, provider_table(port));
    // provider 是历史会话的身份，不是开关。官方用户保持 openai，
    // 已安装版本保持 codex_gateway；启停只改变路由，不修改历史索引。
    if !matches!(
        doc.get("model_provider").and_then(Item::as_str),
        None | Some("openai" | PROVIDER)
    ) {
        doc["model_provider"] = value(PROVIDER);
    }
    // 部分旧会话恢复时沿用 openai。使用客户端提供的专用 URL 设置，
    // 使其也经过同一请求适配；禁止重定义保留的 model_providers.openai。
    doc["openai_base_url"] = value(format!("http://127.0.0.1:{port}/v1"));
    doc["model"] = value(if model == "deepseek-v4-flash" {
        "deepseek-flash"
    } else {
        model
    });
    // Codex 接受 catalog 路径；绝对路径避免从不同工作目录启动时读错文件。
    doc["model_catalog_json"] = value(paths::catalog_path()?.to_string_lossy().as_ref());
    Ok(())
}

pub fn apply_gateway_to_codex_config(port: u16, default_model: &str) -> Result<()> {
    ensure_pristine_backup()?;
    let path = paths::codex_config_path()?;
    let mut doc = read_doc(&path)?;
    let previous = match fs::read(&path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("读取配置快照失败"),
    };
    apply_doc(&mut doc, port, default_model)?;
    storage::private_write(&path, doc.to_string().as_bytes())?;
    let mut state = load_state()?;
    state.managed_config = true;
    state.last_setup_at = Some(now_iso());
    if let Err(error) = save_state(&state) {
        match previous {
            Some(raw) => storage::private_write(&path, &raw)?,
            None => fs::remove_file(&path)?,
        }
        return Err(error).context("保存状态失败，Codex 配置已回滚");
    }
    Ok(())
}

pub fn ensure_custom_active(port: u16, default_model: &str) -> Result<bool> {
    let path = paths::codex_config_path()?;
    let doc = read_doc(&path)?;
    let provider = doc.get("model_providers").and_then(|p| p.get(PROVIDER));
    let expected = format!("http://127.0.0.1:{port}/v1");
    let expected_catalog = paths::catalog_path()?.to_string_lossy().into_owned();
    let already = gateway_active(&doc)
        && matches!(
            doc.get("model_provider").and_then(Item::as_str),
            None | Some("openai" | PROVIDER)
        )
        && doc.get("openai_base_url").and_then(Item::as_str) == Some(expected.as_str())
        && doc.get("model_catalog_json").and_then(Item::as_str) == Some(expected_catalog.as_str())
        && provider.is_some_and(|p| {
            p.get("base_url").and_then(Item::as_str) == Some(expected.as_str())
                && p.get("wire_api").and_then(Item::as_str) == Some("responses")
                && p.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
                && p.get("experimental_bearer_token").is_none()
                && p.get("env_key").is_none()
        });
    if !already {
        // 接管官方直连或修正网关配置时，保留用户当前选择的模型。
        let original_is_official = doc
            .get("model_provider")
            .and_then(Item::as_str)
            .unwrap_or("openai")
            == "openai";
        let selected = if gateway_active(&doc) || original_is_official {
            doc.get("model")
                .and_then(Item::as_str)
                .unwrap_or(default_model)
        } else {
            default_model
        };
        apply_gateway_to_codex_config(port, selected)?;
    }
    Ok(!already)
}

fn original_doc() -> Result<DocumentMut> {
    if let Some(id) = load_state()?.backup_id {
        let path = paths::backups_dir()?.join(id);
        anyhow::ensure!(path.is_file(), "原始备份缺失，停止恢复以避免覆盖用户配置");
        let original = read_doc(&path)?;
        anyhow::ensure!(
            !gateway_active(&original),
            "原始备份内容已被网关接管，拒绝恢复；请手工核对 {}",
            path.display()
        );
        return Ok(original);
    }
    Ok(DocumentMut::new())
}

fn restore_item(doc: &mut DocumentMut, original: &DocumentMut, key: &str) {
    if let Some(item) = original.get(key) {
        doc[key] = item.clone();
    } else {
        doc.remove(key);
    }
}

/// 只还原仍然属于网关的字段；第三方 provider、目录及后续新增设置均保留。
fn restore_doc(doc: &mut DocumentMut, original: &DocumentMut) {
    let active = gateway_active(doc);
    let legacy_owned = legacy_table_owned(doc);
    let owned_catalog = own_catalog(doc);
    let owned_openai_route = own_openai_route(doc);
    let restore_provider = active
        && (legacy_active(doc)
            || matches!(
                doc.get("model_provider").and_then(Item::as_str),
                Some(PROVIDER | "openai") | None
            ));
    if restore_provider {
        restore_item(doc, original, "model_provider");
        let original_provider = original
            .get("model_provider")
            .and_then(Item::as_str)
            .unwrap_or("openai");
        if original_provider != "openai"
            || doc
                .get("model")
                .and_then(Item::as_str)
                .is_some_and(|m| m.starts_with("deepseek-"))
        {
            restore_item(doc, original, "model");
        }
    }
    if owned_catalog {
        restore_item(doc, original, "model_catalog_json");
    }
    if owned_openai_route {
        restore_item(doc, original, "openai_base_url");
    }
    let original_custom = original
        .get("model_providers")
        .and_then(|p| p.get("custom"));
    let restore_missing_custom =
        restore_provider && doc.get("model_provider").and_then(Item::as_str) == Some("custom");
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        if providers
            .get(PROVIDER)
            .is_some_and(|p| p.get("name").and_then(Item::as_str) == Some("codex-gateway"))
        {
            providers.remove(PROVIDER);
        }
        if legacy_owned {
            // 老版本覆盖了 custom，优先恢复安装前的 custom 表。
            if let Some(custom) = original
                .get("model_providers")
                .and_then(|p| p.get("custom"))
            {
                providers.insert("custom", custom.clone());
            } else {
                providers.remove("custom");
            }
        } else if restore_missing_custom && providers.get("custom").is_none() {
            if let Some(custom) = original_custom {
                providers.insert("custom", custom.clone());
            }
        }
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }
}

pub fn switch_to_native_for_stop() -> Result<bool> {
    let path = paths::codex_config_path()?;
    let mut doc = read_doc(&path)?;
    let original = original_doc()?;
    let before = doc.to_string();
    let keep_identity = doc.get("model_provider").and_then(Item::as_str) == Some(PROVIDER)
        && original
            .get("model_provider")
            .and_then(Item::as_str)
            .unwrap_or("openai")
            == "openai";
    let had_gateway_alias = doc
        .get("model_providers")
        .and_then(|p| p.get(PROVIDER))
        .is_some_and(|p| p.get("name").and_then(Item::as_str) == Some("codex-gateway"));
    restore_doc(&mut doc, &original);
    if had_gateway_alias {
        // JSONL 保留原 provider；停止服务后仍须能解析 codex_gateway 历史。
        // 兼容别名改指官方直连，不能留在已停止的本地端口。
        if !doc.contains_key("model_providers") {
            doc["model_providers"] = Item::Table(Table::new());
        }
        let mut alias = provider_table(1);
        alias["base_url"] = value(crate::proxy::routing::OFFICIAL_BASE);
        doc["model_providers"][PROVIDER] = alias;
        if keep_identity {
            doc["model_provider"] = value(PROVIDER);
        }
    }
    let after = doc.to_string();
    let changed = after != before;
    if changed {
        storage::private_write(&path, after.as_bytes())?;
    }
    let mut state = load_state()?;
    state.managed_config = false;
    save_state(&state)?;
    Ok(changed)
}

pub fn restore_native() -> Result<RestoreReport> {
    let restored_config = switch_to_native_for_stop()?;
    let catalog = paths::catalog_path()?;
    let catalog_removed = catalog.is_file();
    if catalog_removed {
        fs::remove_file(catalog)?;
    }
    // 原始备份直到最后删除网关数据时才清除，失败后可重试。
    Ok(RestoreReport {
        restored_config,
        catalog_removed,
    })
}

/// 删除确认属于本工具的文件，不对用户指定的数据目录执行递归删除。
pub fn purge_gateway_data() -> Result<()> {
    let root = paths::gateway_dir()?;
    let codex = paths::codex_home()?;
    let same_dir = fs::canonicalize(&root)
        .ok()
        .zip(fs::canonicalize(&codex).ok())
        .is_some_and(|(a, b)| a == b);
    anyhow::ensure!(
        !same_dir && root != codex && root.parent().is_some(),
        "网关数据目录不能是 Codex 目录或磁盘根目录"
    );
    let snapshot = paths::official_models_path()?;
    if snapshot.is_file() {
        fs::remove_file(snapshot)?;
    }
    // 避免 CODEX_GATEWAY_HOME 被误设成项目/用户主目录时删除其中其它内容。
    let state = load_state()?;
    // 目录里没有可识别的网关配置/备份时，不删除任何同名文件。
    if state.backup_id.is_none() && crate::settings::gateway::GatewayConfig::load()?.is_none() {
        return Ok(());
    }
    if let Some(id) = state.backup_id {
        let backup = root.join(paths::BACKUPS_DIR_NAME).join(id);
        if backup.is_file() {
            fs::remove_file(backup)?;
        }
    }
    let backups = root.join(paths::BACKUPS_DIR_NAME);
    if backups.is_dir() && fs::read_dir(&backups)?.next().is_none() {
        fs::remove_dir(backups)?;
    }
    for name in [
        paths::CONFIG_FILE_NAME,
        paths::STATE_FILE_NAME,
        paths::LOG_FILE_NAME,
        paths::PID_FILE_NAME,
        "gateway.log.1",
    ] {
        let path = root.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(raw: &str) -> DocumentMut {
        raw.parse().unwrap()
    }
    fn installed(original: &DocumentMut) -> DocumentMut {
        let mut current = original.clone();
        current["model_provider"] = value(PROVIDER);
        current["model"] = value("deepseek-flash");
        current["model_catalog_json"] = value(CATALOG_FILE_NAME);
        if !current.contains_key("model_providers") {
            current["model_providers"] = Item::Table(Table::new());
        }
        current["model_providers"][PROVIDER] = provider_table(17899);
        current
    }

    #[test]
    fn native_history_route_is_intercepted_and_previous_url_restored() {
        let original = doc("model='gpt-6.1-sol'\nmodel_provider='openai'\nopenai_base_url='https://example.test/v1'\n");
        let mut current = original.clone();
        apply_doc(&mut current, 17899, "gpt-6.1-sol").unwrap();
        assert_eq!(
            current["openai_base_url"].as_str(),
            Some("http://127.0.0.1:17899/v1")
        );
        assert!(current["model_providers"].get("openai").is_none());
        restore_doc(&mut current, &original);
        assert_eq!(
            current["openai_base_url"].as_str(),
            Some("https://example.test/v1")
        );
        assert_eq!(current["model_provider"].as_str(), Some("openai"));
    }

    #[test]
    fn restore_preserves_new_settings_and_original_custom_provider() {
        let original = doc("# 原始\nmodel = \"gpt-original\"\nmodel_provider = \"custom\"\nmodel_catalog_json = \"my-models.json\"\n[model_providers.custom]\nbase_url = \"https://example.test/v1\"\n");
        let mut current = installed(&original);
        current["new_setting"] = value("keep");
        restore_doc(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("custom"));
        assert_eq!(current["model"].as_str(), Some("gpt-original"));
        assert_eq!(
            current["model_catalog_json"].as_str(),
            Some("my-models.json")
        );
        assert_eq!(current["new_setting"].as_str(), Some("keep"));
        assert!(current["model_providers"].get("custom").is_some());
        assert!(current["model_providers"].get(PROVIDER).is_none());
        assert!(current.to_string().contains("# 原始"));
    }

    #[test]
    fn restores_private_model_when_original_provider_is_custom() {
        let original = doc("model_provider = \"custom\"\nmodel = \"private-model\"\n");
        let mut current = installed(&original);
        current["model"] = value("gpt-selected-in-gateway");
        restore_doc(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("custom"));
        assert_eq!(current["model"].as_str(), Some("private-model"));
    }

    #[test]
    fn restore_keeps_later_provider_and_model_selection() {
        let original = doc("model = \"gpt-original\"\n");
        let mut current = installed(&original);
        current["model_provider"] = value("another");
        current["model"] = value("new-model");
        current["model_catalog_json"] = value("third-party.json");
        restore_doc(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("another"));
        assert_eq!(current["model"].as_str(), Some("new-model"));
        assert_eq!(
            current["model_catalog_json"].as_str(),
            Some("third-party.json")
        );
    }

    #[test]
    fn native_roundtrip_and_stop_are_idempotent() {
        let original = doc("# comment\nmodel = \"gpt-original\"\nmodel_provider = \"openai\"\n[projects.test]\ntrust_level = \"trusted\"\n");
        let mut current = installed(&original);
        restore_doc(&mut current, &original);
        assert_eq!(current.to_string(), original.to_string());
        let before = current.to_string();
        restore_doc(&mut current, &original);
        assert_eq!(current.to_string(), before);
    }

    #[test]
    fn restore_without_backup_only_removes_our_fields() {
        let mut current = installed(&DocumentMut::new());
        current["extra"] = value(true);
        restore_doc(&mut current, &DocumentMut::new());
        assert!(current.get("model_provider").is_none());
        assert!(current.get("model").is_none());
        assert!(current.get("model_catalog_json").is_none());
        assert_eq!(current["extra"].as_bool(), Some(true));
    }

    #[test]
    fn does_not_claim_foreign_custom_or_comment_marker() {
        let foreign = doc("# codex-gateway-model-catalog.json\nmodel_provider = \"custom\"\nmodel_catalog_json = \"my-models.json\"\n");
        assert!(!gateway_active(&foreign));
        assert!(!gateway_active(&doc("model_provider = \"custom\"\nmodel_catalog_json = \"codex-gateway-model-catalog.json\"\n")));
    }

    #[test]
    fn restore_still_works_after_catalog_key_is_removed() {
        let original = doc("model = \"gpt-original\"\nmodel_provider = \"openai\"\n");
        let mut current = installed(&original);
        current.remove("model_catalog_json");
        assert!(gateway_active(&current));
        restore_doc(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("openai"));
        assert_eq!(current["model"].as_str(), Some("gpt-original"));
        assert!(current.get("model_providers").is_none());
    }

    #[test]
    fn rejects_backup_path_traversal() {
        assert!(validate_backup_id("config.toml.20261002-001").is_ok());
        for bad in ["../config.toml", "config.toml.../secret", "C:\\config.toml"] {
            assert!(validate_backup_id(bad).is_err());
        }
    }

    #[test]
    fn old_custom_table_is_restored_from_original() {
        let original = doc("model = \"gpt-old\"\n[model_providers.custom]\nbase_url = \"https://my-api.test/v1\"\n");
        let mut current = doc("model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\nmodel_catalog_json = \"codex-gateway-model-catalog.json\"\n[model_providers.custom]\nname = \"deepseek\"\nbase_url = \"http://127.0.0.1:17899/v1\"\n");
        restore_doc(&mut current, &original);
        assert_eq!(
            current["model_providers"]["custom"]["base_url"].as_str(),
            Some("https://my-api.test/v1")
        );
        assert_eq!(current["model"].as_str(), Some("gpt-old"));
    }

    #[test]
    fn legacy_custom_survives_apply_then_restore() {
        let original = doc("model='gpt-old'\nmodel_provider='custom'\n[model_providers.custom]\nbase_url='https://my-api.test/v1'\n");
        let mut current = doc("model_provider='custom'\nmodel='deepseek-v4-flash'\n[model_providers.custom]\nname='deepseek'\nbase_url='http://127.0.0.1:17899/v1'\n");
        apply_doc(&mut current, 17899, "deepseek-flash").unwrap();
        restore_doc(&mut current, &original);
        assert_eq!(current["model_provider"].as_str(), Some("custom"));
        assert_eq!(
            current["model_providers"]["custom"]["base_url"].as_str(),
            Some("https://my-api.test/v1")
        );
    }
}
