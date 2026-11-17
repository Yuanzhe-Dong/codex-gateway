//! 命令入口：启动成功以服务健康为准；所有配置变更串行执行。
use super::lifecycle::*;
use crate::runtime::paths;
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "codex-gateway",
    version,
    about = "在 Codex 中切换官方订阅与 DeepSeek 的本地网关"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 交互式配置向导
    Setup,
    /// 启动网关（--foreground 在前台运行）
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// 停止当前实例并恢复直连；保留官方/网关会话的 provider 身份
    Stop,
    /// 停止旧实例并用当前版本重新启动，保留配置和原始会话历史
    Restart,
    /// 查看本地状态及上游网络连通性
    Status {
        /// 跳过外网探测，只检查本地
        #[arg(long)]
        offline: bool,
    },
    /// 按当前登录账号在线刷新官方模型目录，不切换 provider
    RefreshModels,
    /// 直接测试 DeepSeek 文本和函数调用（两次小请求，正常计费）
    TestDeepseek {
        #[arg(long, default_value = "deepseek-flash")]
        model: String,
    },
    /// 删除网关配置、日志和原始配置备份；会话修复备份保留
    Uninstall,
    /// 检查旧会话中的不兼容 reasoning；默认只预览
    RepairSessions {
        /// 旧参数，仅用于明确拒绝改写历史
        #[arg(long, hide = true)]
        apply: bool,
    },
    /// 只读验证单条会话的官方/DS 请求副本，不访问上游
    CheckSession {
        /// 会话 JSONL 文件路径
        path: std::path::PathBuf,
    },
    #[command(hide = true)]
    Serve,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let logging = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false);
    if matches!(&cli.command, Command::Serve) {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths::log_path()?)?;
        logging
            .with_writer(std::sync::Mutex::new(file))
            .try_init()
            .ok();
    } else {
        logging.try_init().ok();
    }
    match cli.command {
        Command::Setup => {
            let _guard = control_lock()?;
            crate::app::wizard::run_setup()
        }
        Command::Start { foreground } => cmd_start(foreground),
        Command::Stop => {
            let _guard = control_lock()?;
            cmd_stop()
        }
        Command::Restart => cmd_restart(),
        Command::Status { offline } => crate::app::status::run_status(offline),
        Command::RefreshModels => {
            let _guard = control_lock()?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(crate::models::refresh::refresh_models())
        }
        Command::TestDeepseek { model } => {
            let cfg = require_configured()?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(crate::models::refresh::test_deepseek(&cfg, &model))
        }
        Command::Uninstall => {
            let _guard = control_lock()?;
            cmd_uninstall()
        }
        Command::RepairSessions { apply } => {
            anyhow::ensure!(
                !apply,
                "已停用会话历史改写；请启动网关，仅在官方请求副本中处理兼容性，保留 DS 原始历史。"
            );
            let report = crate::history::repair_sessions(apply)?;
            println!(
                "预览：{} 个文件，{} 个推理项，{} 个文件跳过",
                report.files, report.items, report.skipped
            );
            if !apply && report.items > 0 {
                println!(
                    "原始历史保持原样；启动网关后会自动适配官方请求副本，DS 明文推理继续保留。"
                );
            }
            anyhow::ensure!(report.skipped == 0, "部分会话无法处理，请关闭 Codex 后重试");
            Ok(())
        }
        Command::CheckSession { path } => {
            let report = crate::history::check_session(&path)?;
            println!(
                "历史 provider：{}；恢复会话时客户端的 provider 必须与它一致。",
                report.provider.as_deref().unwrap_or("未记录")
            );
            println!("原始有效历史：{} 项；官方请求副本过滤：{} 条第三方推理；DS 请求副本保留：{} 条明文推理。",
                report.items, report.official_removed, report.deepseek_reasoning);
            println!("只读验证通过；原始历史未修改，未调用上游 API。");
            Ok(())
        }
        Command::Serve => {
            let result = cmd_serve(false);
            if let Err(error) = &result {
                tracing::error!("代理启动/运行失败：{error:#}");
            }
            result
        }
    }
}
