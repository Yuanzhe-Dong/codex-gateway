//! 区分进程、服务健康、配置激活与外网可达；网络可达不代表凭证有效。
use crate::runtime::network::{build_client, display_proxy};
use crate::runtime::paths;
use crate::settings::gateway::GatewayConfig;
use anyhow::Result;
use std::time::Duration;

pub fn run_status(offline: bool) -> Result<()> {
    println!("========== Codex 网关状态 ==========");
    println!("控制程序版本：{}", env!("CARGO_PKG_VERSION"));
    let Some(cfg) = GatewayConfig::load()? else {
        println!("尚未配置，请先运行 codex-gateway setup");
        return Ok(());
    };
    println!(
        "端口：{}；DeepSeek 模型：{}",
        cfg.port,
        cfg.deepseek_models.join(", ")
    );
    println!("DeepSeek API Key：{}", cfg.masked_key());
    println!(
        "网络模式：{}",
        match (&cfg.proxy, cfg.auto_proxy) {
            (Some(p), _) => format!("手动代理 {}", display_proxy(p)),
            (None, true) => "自动检测环境/系统代理".into(),
            (None, false) => "强制直连".into(),
        }
    );
    let running = crate::runtime::process::is_running()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let health = rt
        .block_on(crate::runtime::process::health_info(cfg.port))
        .ok()
        .flatten();
    let health_pid = health.as_ref().map(|info| info.pid);
    if let Some(info) = &health {
        println!("实际服务版本：{}", info.version);
        if info.version != env!("CARGO_PKG_VERSION") {
            println!("服务仍在运行其它版本；请用 restart（控制台 5）应用更新。");
        }
    }
    let expected = crate::runtime::process::read_pid()?;
    let healthy = running && health_pid == expected;
    println!(
        "进程身份：{}；服务健康：{}",
        if running {
            "匹配"
        } else {
            "未运行或记录过时"
        },
        if healthy { "正常" } else { "未通过检查" }
    );
    if let Some(pid) = expected {
        println!("PID：{pid}");
    }
    let managed = crate::settings::codex::is_managed()?;
    println!("Codex 配置接管：{}", if managed { "是" } else { "否" });
    println!(
        "历史兼容：{}",
        if managed && healthy {
            "已启用；官方仅处理请求副本，DS 明文推理和原始历史保留"
        } else {
            "未生效；需要启动网关并重启 Codex"
        }
    );
    if managed && !healthy {
        println!("配置指向网关，但服务不可用；请运行 start 或 stop。");
    }
    if let Some(id) = crate::settings::codex::load_state()?.backup_id {
        println!("原始配置备份：{}", paths::backups_dir()?.join(id).display());
    }
    let catalog = paths::catalog_path()?;
    println!(
        "模型目录：{} ({})",
        catalog.display(),
        if catalog.exists() { "存在" } else { "缺失" }
    );
    if let Ok(raw) = std::fs::read(&catalog) {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) {
            if let Some(models) = value["models"].as_array() {
                let visible: Vec<_> = models
                    .iter()
                    .filter(|m| m["visibility"] != "hide")
                    .filter_map(|m| m["slug"].as_str())
                    .collect();
                println!("目录中的可见选项：{}", visible.join(", "));
            }
        }
    }
    if let Some(binary) = crate::models::client::installed_binary() {
        println!("检测到的桌面运行时：{}", binary.display());
    }
    if offline {
        return Ok(());
    }
    let resolved = crate::runtime::network::resolved_proxy(&cfg.proxy, cfg.auto_proxy);
    println!(
        "生效的出站代理：{}",
        resolved
            .as_deref()
            .map(display_proxy)
            .unwrap_or_else(|| "直连".into())
    );
    println!("外网探测（每个上游最多 5 秒，只检查 HTTP 可达性）：");
    rt.block_on(async {
        let client = build_client(resolved.as_deref())?;
        let (official, deepseek) = tokio::join!(
            probe(&client, "https://chatgpt.com/backend-api/codex/"),
            probe(&client, "https://api.deepseek.com/")
        );
        println!("  官方 ChatGPT：{official}");
        println!("  DeepSeek：{deepseek}");
        Ok::<_, anyhow::Error>(())
    })
}

async fn probe(client: &reqwest::Client, url: &str) -> String {
    match client.get(url).timeout(Duration::from_secs(5)).send().await {
        Ok(response) => format!("HTTP 可达（{}；未验证登录态或 API Key）", response.status()),
        Err(_) => "不可达，请检查网络或代理".into(),
    }
}
