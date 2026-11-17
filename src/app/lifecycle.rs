//! 生命周期事务：预检、锁、服务健康、配置激活及失败恢复。
use crate::runtime::paths;
use crate::runtime::storage;
use crate::settings::gateway::GatewayConfig;
use anyhow::{anyhow, Context, Result};

pub(crate) fn control_lock() -> Result<(std::fs::File, std::fs::File)> {
    let gateway = storage::lock(&paths::gateway_dir()?.join("control.lock"))?;
    // 不同网关数据目录也不能同时改写同一个 Codex 配置。
    let codex = storage::lock(&paths::codex_home()?.join("codex-gateway.lock"))?;
    Ok((gateway, codex))
}

pub(super) fn require_configured() -> Result<GatewayConfig> {
    GatewayConfig::load()?.ok_or_else(|| anyhow!("尚未配置，请先运行 codex-gateway setup"))
}

pub(super) fn refresh_catalog(cfg: &GatewayConfig) -> Result<()> {
    // 每天更新过期账号目录；失败时继续用本地目录，不伪造账号授权。
    let fresh = std::fs::metadata(paths::official_models_path()?)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(86400));
    if !fresh && paths::codex_home()?.join("auth.json").is_file() {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(crate::models::refresh::refresh_models());
        if let Err(error) = result {
            tracing::warn!("在线模型目录更新失败，使用本地目录：{error:#}");
        }
    }
    crate::models::catalog::write_catalog_file(
        &crate::models::catalog::current_catalog(&cfg.deepseek_models)?,
        &paths::catalog_path()?,
    )
}

pub(super) fn cmd_start(foreground: bool) -> Result<()> {
    let guard = control_lock()?;
    cmd_start_locked(foreground, guard)
}

pub(super) fn cmd_start_locked(
    foreground: bool,
    guard: (std::fs::File, std::fs::File),
) -> Result<()> {
    let cfg = require_configured()?;
    if crate::runtime::process::is_running()? {
        let pid = crate::runtime::process::read_pid()?.context("进程记录丢失")?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let health = rt
            .block_on(crate::runtime::process::health_info(cfg.port))
            .ok()
            .flatten();
        anyhow::ensure!(
            health.as_ref().map(|info| info.pid) == Some(pid),
            "进程存在但配置端口上的服务未通过健康检查；请使用 restart"
        );
        let running_version = health.unwrap().version;
        anyhow::ensure!(
            running_version == env!("CARGO_PKG_VERSION"),
            "运行中的网关为 {running_version}，当前程序为 {}；请用 restart（控制台 5）应用更新",
            env!("CARGO_PKG_VERSION")
        );
        refresh_catalog(&cfg)?;
        let switched =
            crate::settings::codex::ensure_custom_active(cfg.port, &cfg.deepseek_models[0])?;
        println!(
            "代理已在运行（PID {pid}），监听 http://127.0.0.1:{}",
            cfg.port
        );
        if switched {
            println!("已激活网关配置，请重启 Codex 载入模型菜单。");
        }
        return Ok(());
    }
    // 所有可提前发现的配置/网络错误都在创建子进程之前报告。
    crate::proxy::ProxyState::production(&cfg)?;
    crate::settings::codex::ensure_pristine_backup()?;
    refresh_catalog(&cfg)?;
    if foreground {
        drop(guard);
        return cmd_serve(true);
    }
    let exe = std::env::current_exe()?;
    let log = paths::log_path()?;
    let mut child = crate::runtime::process::spawn_background(&exe, &log)?;
    crate::runtime::process::wait_ready(&mut child, cfg.port)?;
    // 只有服务已经监听且身份匹配，才将 Codex 指向网关。
    if let Err(e) = crate::settings::codex::ensure_custom_active(cfg.port, &cfg.deepseek_models[0])
    {
        child.kill()?;
        child.wait()?;
        crate::runtime::process::remove_pid()?;
        return Err(e).context("激活配置失败，已停止新启动的代理");
    }
    println!(
        "✔ 代理已启动（PID {}），监听 http://127.0.0.1:{}",
        child.id(),
        cfg.port
    );
    println!("日志：{}", log.display());
    println!("首次使用或切换 provider 后，请完全退出并重新打开 Codex。");
    println!("历史兼容已启用：官方请求副本过滤第三方推理，DS 明文推理与原始历史保留。");
    Ok(())
}

pub(super) fn cmd_stop() -> Result<()> {
    // 先恢复配置，恢复失败时保留仍可工作的网关。
    let switched = crate::settings::codex::switch_to_native_for_stop()?;
    crate::runtime::process::stop()?;
    println!("✔ 当前网关实例已停止。");
    if switched {
        println!("已恢复直连配置并保留官方/网关会话身份，请重启 Codex。");
    }
    println!("会话历史文件保持原样；恢复直连后不再适配 DS 历史，跨模型旧会话请重新 start 后使用。");
    Ok(())
}

pub(super) fn cmd_serve(activate: bool) -> Result<()> {
    let cfg = require_configured()?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(crate::proxy::serve(cfg, activate))
}

pub(super) fn cmd_uninstall() -> Result<()> {
    let report = crate::settings::codex::restore_native()?;
    crate::runtime::process::stop()?;
    crate::settings::codex::purge_gateway_data()?;
    println!("✔ 当前实例已停止，网关配置、密钥、日志和原始配置备份已移除。");
    println!(
        "配置还原：{}；模型目录删除：{}",
        report.restored_config, report.catalog_removed
    );
    println!("会话历史及已有会话修复备份保留；重启 Codex 后生效。");
    Ok(())
}

pub(super) fn cmd_restart() -> Result<()> {
    let guard = control_lock()?;
    // 重启只替换进程，不在中途删除 provider 或来回迁移全部会话。
    let cfg = require_configured()?;
    crate::proxy::ProxyState::production(&cfg)?;
    crate::settings::codex::ensure_pristine_backup()?;
    refresh_catalog(&cfg)?;
    crate::runtime::process::stop()?;
    let result = cmd_start_locked(false, guard);
    if result.is_err() {
        // 启动失败时恢复直连，避免配置长期指向已停止的服务。
        let _guard = control_lock()?;
        crate::settings::codex::switch_to_native_for_stop()?;
    }
    result
}
