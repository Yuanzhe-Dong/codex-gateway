//! 本机关键路径集中管理（CODEX_HOME、网关数据目录等）。

use anyhow::{Context, Result};
use std::path::PathBuf;

/// 网关数据目录名（位于用户主目录下）。
pub const GATEWAY_DIR_NAME: &str = ".codex-gateway";
/// 我们生成的合并模型目录文件名（放在 CODEX_HOME 下，与 config.toml 同级）。
pub const CATALOG_FILE_NAME: &str = "codex-gateway-model-catalog.json";
pub const PID_FILE_NAME: &str = "gateway.pid";
pub const LOG_FILE_NAME: &str = "gateway.log";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const STATE_FILE_NAME: &str = "state.json";
pub const BACKUPS_DIR_NAME: &str = "backups";

/// Codex 主目录：优先 CODEX_HOME 环境变量，否则 ~/.codex。
pub fn codex_home() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("CODEX_HOME") {
        if !v.trim().is_empty() {
            return Ok(PathBuf::from(v.trim()));
        }
    }
    let home = dirs::home_dir().context("无法定位用户主目录")?;
    Ok(home.join(".codex"))
}

/// 网关数据目录：默认 ~/.codex-gateway；
/// 若设置 CODEX_GATEWAY_HOME 环境变量则直接使用（便携模式/测试用）。
pub fn gateway_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("CODEX_GATEWAY_HOME") {
        if !v.trim().is_empty() {
            return Ok(PathBuf::from(v.trim()));
        }
    }
    let home = dirs::home_dir().context("无法定位用户主目录")?;
    Ok(home.join(GATEWAY_DIR_NAME))
}

pub fn gateway_config_path() -> Result<PathBuf> {
    Ok(gateway_dir()?.join(CONFIG_FILE_NAME))
}

pub fn state_path() -> Result<PathBuf> {
    Ok(gateway_dir()?.join(STATE_FILE_NAME))
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(gateway_dir()?.join(PID_FILE_NAME))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(gateway_dir()?.join(LOG_FILE_NAME))
}

pub fn backups_dir() -> Result<PathBuf> {
    Ok(gateway_dir()?.join(BACKUPS_DIR_NAME))
}

/// Codex 的 config.toml。
pub fn codex_config_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("config.toml"))
}

/// Codex 官方模型缓存（合并目录时抽取官方模型用）。
pub fn codex_models_cache_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("models_cache.json"))
}

/// 我们生成/管理的合并模型目录文件。
pub fn catalog_path() -> Result<PathBuf> {
    Ok(codex_home()?.join(CATALOG_FILE_NAME))
}