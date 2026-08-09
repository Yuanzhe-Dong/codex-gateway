//! status 命令：展示配置、进程、Codex 接管状态与上游连通性。

use anyhow::{Context, Result};
use std::time::Duration;

use crate::config::GatewayConfig;
use crate::network::build_client;
use crate::paths;

pub fn run_status() -> Result<()> {
    println!("========== Codex 无缝模型切换网关 · 状态 ==========");

    // 1. 网关配置
    let cfg = GatewayConfig::load()?;
    match &cfg {
        Some(cfg) => {
            println!("网关配置:");
            println!("  端口: {}", cfg.port);
            println!("  DeepSeek 模型: {}", cfg.deepseek_models.join(", "));
            println!("  DeepSeek API Key: {}", cfg.masked_key());
            println!(
                "  网络模式: {}",
                match (&cfg.proxy, cfg.auto_proxy) {
                    (Some(p), _) => format!("手动代理 {p}"),
                    (None, true) => "自动（直连 / 跟随系统代理）".to_string(),
                    (None, false) => "强制直连".to_string(),
                }
            );
        }
        None => {
            println!("网关尚未配置，请先运行 `codex-gateway setup`");
            return Ok(());
        }
    }

    // 2. 进程
    let running = crate::process::is_running()?;
    println!();
    if running {
        println!("代理进程: 运行中 ✅（Codex 可正常连接）");
        if let Some(pid) = crate::process::read_pid()? {
            println!("  PID: {pid}");
        }
    } else {
        println!("代理进程: 未运行 ❌（Codex 将无法连接，请先运行 start）");
    }

    // 3. Codex 接管状态
    let managed = crate::codex_config::is_managed()?;
    println!();
    println!("Codex 配置接管: {}", if managed { "是" } else { "否" });
    if let Some(id) = crate::codex_config::load_state()?.backup_id {
        println!("  原生配置备份: ~/.codex-gateway/backups/{id}");
    }
    let cat = paths::catalog_path()?;
    println!(
        "  合并模型目录: {} ({})",
        cat.display(),
        if cat.exists() { "存在" } else { "缺失" }
    );

    // 4. 上游连通性
    // 读取配置中的手动代理 + 是否自动跟随系统/环境变量代理，而不是硬编码。
    let resolved = cfg
        .as_ref()
        .and_then(|c| crate::network::resolved_proxy(&c.proxy, c.auto_proxy));
    println!();
    println!(
        "上游出站代理（去往官方/DeepSeek 的流量走的代理）: {}",
        resolved.as_deref().unwrap_or("直连（未检测到代理）")
    );

    println!("上游连通性测试（超时 5s）...");
    let rt = tokio::runtime::Runtime::new().context("创建运行时失败")?;
    rt.block_on(check_upstream(resolved.as_deref()))
}

async fn check_upstream(proxy: Option<&str>) -> Result<()> {
    let client = build_client(proxy)?;
    for (name, url) in [
        ("官方 ChatGPT", "https://chatgpt.com/backend-api/codex/"),
        ("DeepSeek", "https://api.deepseek.com/"),
    ] {
        let ok = match client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("{url} -> {e}");
                false
            }
        };
        println!("  {name}: {}", if ok { "可达" } else { "不可达（检查网络/代理）" });
    }
    Ok(())
}