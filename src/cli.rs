//! CLI 入口：子命令分发。

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::GatewayConfig;
use crate::paths;

#[derive(Parser)]
#[command(
    name = "codex-gateway",
    version,
    about = "Codex 无缝模型切换网关：官方订阅与 DeepSeek 在一个菜单里点一下即切换，无需重启任务"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 交互式配置向导（首次使用从这里开始）
    Setup,
    /// 启动本地代理（默认后台运行，加 --foreground 前台运行）
    Start {
        /// 前台运行（调试用，Ctrl+C 停止）
        #[arg(long)]
        foreground: bool,
    },
    /// 停止本地代理并切回原生 OpenAI（会话历史不丢）
    Stop,
    /// 查看运行状态与上游连通性
    Status,
    /// 彻底卸载：停止代理 + 还原原生配置 + 删除网关数据
    Uninstall,
    /// 前台运行代理服务（内部命令）
    #[command(hide = true)]
    Serve,
}

pub fn run() -> Result<()> {
    // 所有子命令都初始化 tracing，确保 sync_session_provider 的 warn 可见
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .try_init();
    let cli = Cli::parse();
    match cli.command {
        Command::Setup => crate::wizard::run_setup(),
        Command::Start { foreground } => cmd_start(foreground),
        Command::Stop => cmd_stop(),
        Command::Status => crate::status::run_status(),
        Command::Uninstall => cmd_uninstall(),
        Command::Serve => cmd_serve(),
    }
}

fn require_configured() -> Result<GatewayConfig> {
    let cfg = GatewayConfig::load()?.ok_or_else(|| {
        anyhow!("尚未配置。请先运行 `codex-gateway setup` 完成一次性配置。")
    })?;
    if cfg.deepseek_api_key.trim().is_empty() {
        bail!("DeepSeek API Key 未设置，请先运行 `codex-gateway setup`。");
    }
    Ok(cfg)
}

fn cmd_start(foreground: bool) -> Result<()> {
    let cfg = require_configured()?;
    if foreground {
        return cmd_serve();
    }
    // 启动前确保 config 指向网关（可能被 stop 切回 native）
    let switched = crate::codex_config::ensure_custom_active(cfg.port, &cfg.deepseek_models[0])?;
    // catalog 文件若丢失则重新生成，避免菜单缺 deepseek 模型
    let cat = paths::catalog_path()?;
    if !cat.exists() {
        let official = crate::catalog::official_models_from_cache(&paths::codex_models_cache_path()?)
            .unwrap_or_else(|_| crate::catalog::builtin_official_fallback());
        let catalog = crate::catalog::build_catalog(&cfg.deepseek_models, official);
        crate::catalog::write_catalog_file(&catalog, &cat)?;
        println!("✔ 模型目录缺失，已重新生成。");
    }
    if crate::process::is_running()? {
        println!("代理已在运行（PID {}），监听 http://127.0.0.1:{}", crate::process::read_pid()?.unwrap_or(0), cfg.port);
        if switched {
            println!("  提示：已切回网关配置，重启 Codex 让 DeepSeek 模型重新出现在菜单。");
        }
        return Ok(());
    }
    let exe = std::env::current_exe().context("获取当前程序路径失败")?;
    let log = paths::log_path()?;
    let pid = crate::process::spawn_background(&exe, &log)?;
    crate::process::write_pid(pid)?;
    std::thread::sleep(std::time::Duration::from_millis(900));
    if !crate::process::pid_alive(pid) {
        println!("⚠ 代理启动后很快退出，请查看日志: {}", log.display());
        return Ok(());
    }
    println!("✔ 代理已启动（PID {pid}），监听 http://127.0.0.1:{}", cfg.port);
    println!("  日志: {}", log.display());
    if switched {
        println!("  提示：已切回网关配置，重启 Codex 让 DeepSeek 模型重新出现在菜单。");
    } else {
        println!("  提示：若 Codex 尚未重启，请完全退出并重新打开 Codex 一次。");
    }
    Ok(())
}

fn cmd_stop() -> Result<()> {
    // 总是调 stop()：即使 PID 文件缺失，stop() 内部的 kill_by_name 兜底也能杀掉残留进程。
    // 若用 is_running() 守卫，PID 文件丢失时会跳过 kill_by_name，留下孤儿网关进程。
    crate::process::stop()?;
    // 总是执行 switch：即使网关没运行，config 可能仍是 custom（需要切回 openai），
    // 会话历史可能需要清洗 reasoning、会话 provider 可能需要同步。
    let switched = crate::codex_config::switch_to_native_for_stop()?;
    println!("✔ 代理已停止。");
    if switched {
        println!("  已切回原生 OpenAI 配置（catalog 与备份保留）。");
        println!("  已清洗会话历史 reasoning + 同步会话 provider。");
        println!("  提示：重启 Codex 后 GPT 可直连官方，会话历史不丢失。");
    }
    Ok(())
}

fn cmd_serve() -> Result<()> {
    let cfg = require_configured()?;
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .try_init();
    let rt = tokio::runtime::Runtime::new().context("创建运行时失败")?;
    rt.block_on(crate::proxy::serve(cfg))
}

fn cmd_uninstall() -> Result<()> {
    println!("正在卸载 codex-gateway...");
    let _ = crate::process::stop();
    let report = crate::codex_config::restore_native()?;
    crate::codex_config::purge_gateway_data()?;
    println!("✔ 已停止代理");
    println!(
        "  config.toml 还原: {}",
        if report.restored_config { "是" } else { "无备份可还原（已跳过）" }
    );
    println!(
        "  删除生成的模型目录: {}",
        if report.catalog_removed { "是" } else { "无" }
    );
    println!("  已清洗会话历史 reasoning + 同步会话 provider。");
    println!("  已删除网关数据目录 ~/.codex-gateway/（API Key、备份、日志）。");
    println!("提示：重启 Codex 后 GPT 直连官方可用，会话历史不丢失。");
    println!("  如需再次使用，请运行 `codex-gateway setup` 重新配置。");
    Ok(())
}