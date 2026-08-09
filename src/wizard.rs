//! 交互式配置向导（setup）：输入 DeepSeek Key、选模型、端口、代理。

use anyhow::{bail, Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, MultiSelect, Password, Select};

use crate::config::{GatewayConfig, DEFAULT_DEEPSEEK_MODELS, DEFAULT_PORT};

pub fn run_setup() -> Result<()> {
    let existing = GatewayConfig::load()?;

    println!();
    println!("==========================================================");
    println!("  Codex 无缝模型切换网关 · 配置向导");
    println!("==========================================================");
    println!("本向导会：");
    println!("  1. 自动备份你的原生 Codex 配置（~/.codex/config.toml）");
    println!("  2. 写入网关配置，生成合并模型目录（官方 + DeepSeek）");
    println!("  3. 之后随时可用 `codex-gateway stop` 切回原生，`uninstall` 彻底卸载");
    println!();

    // 1. DeepSeek API Key
    let mut key = String::new();
    if let Some(cfg) = &existing {
        if !cfg.deepseek_api_key.trim().is_empty() {
            println!("当前 DeepSeek API Key: {}（直接回车保持不变）", cfg.masked_key());
        }
    }
    let input: String = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入 DeepSeek API Key（platform.deepseek.com 获取，以 sk- 开头；输入不回显）")
        .allow_empty_password(true)
        .interact()?;
    if !input.trim().is_empty() {
        key = input.trim().to_string();
    } else if let Some(cfg) = &existing {
        key = cfg.deepseek_api_key.clone();
    }
    if key.trim().is_empty() {
        bail!("DeepSeek API Key 不能为空，配置已终止（未改动任何配置）。");
    }

    // 2. DeepSeek 模型（多选；末项为「全选」）
    let all_models: Vec<String> = DEFAULT_DEEPSEEK_MODELS.iter().map(|s| s.to_string()).collect();
    let mut items: Vec<String> = all_models.clone();
    items.push("全选（勾选全部 DeepSeek 模型）".to_string());
    let select_all_idx = items.len() - 1;
    let defaults: Vec<bool> = items
        .iter()
        .enumerate()
        .map(|(i, m)| match existing.as_ref() {
            // 末项「全选」默认不勾：勾上即全部选中；每个模型可单独空格控制
            Some(_) if i == select_all_idx => false,
            Some(c) => c.deepseek_models.iter().any(|x| x == m),
            // 首次默认全部模型已勾选，直接回车即全选
            None => i < select_all_idx,
        })
        .collect();
    let chosen = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("选择要显示的 DeepSeek 模型（↑↓ 移动，空格 选中/取消，回车 确认；默认全选，末项为全选）")
        .items(&items)
        .defaults(&defaults)
        .interact()?;
    // 勾了「全选」→ 全部模型；否则按逐个勾选的模型
    let models: Vec<String> = if chosen.contains(&select_all_idx) {
        all_models
    } else {
        chosen
            .iter()
            .filter(|&&i| i < select_all_idx)
            .map(|&i| items[i].clone())
            .collect()
    };
    if models.is_empty() {
        bail!("至少选择一个 DeepSeek 模型，配置已终止。");
    }

    // 3. 端口
    let default_port = existing.as_ref().map(|c| c.port).unwrap_or(DEFAULT_PORT);
    let port_str: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("本地代理端口（默认 17899，避开 CC Switch 等已有端口）")
        .default(default_port.to_string())
        .allow_empty(false)
        .interact_text()?;
    let port: u16 = port_str
        .trim()
        .parse()
        .context("端口必须是 1-65535 的数字")?;

    // 4. 网络模式
    let proxy_choices = [
        "自动（直连 / 跟随系统代理）",
        "手动指定代理",
        "强制直连",
    ];
    let default_proxy_idx = if existing.as_ref().map(|c| c.auto_proxy).unwrap_or(true) {
        0
    } else {
        2
    };
    let sel = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("上游网络模式（切换梯子/公司网络时自动适应，推荐自动）")
        .items(&proxy_choices)
        .default(default_proxy_idx)
        .interact()?;
    let (proxy, auto_proxy) = match sel {
        1 => {
            let p: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("代理地址，如 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080")
                .interact_text()?;
            (Some(p.trim().to_string()), false)
        }
        2 => (None, false),
        _ => (existing.as_ref().and_then(|c| c.proxy.clone()), true),
    };

    // ---- 应用配置 ----
    let mut cfg = existing.unwrap_or_default();
    cfg.deepseek_api_key = key;
    cfg.deepseek_models = models;
    cfg.port = port;
    cfg.proxy = proxy;
    cfg.auto_proxy = auto_proxy;
    cfg.save().context("保存网关配置失败")?;

    // 备份原生配置（首次接管时）
    match crate::codex_config::ensure_pristine_backup() {
        Ok(Some(id)) => println!("✔ 已备份原生配置 -> ~/.codex-gateway/backups/{id}"),
        Ok(None) => println!("✔ 原生配置备份已存在，跳过备份"),
        Err(e) => println!("⚠ 备份失败（不影响配置写入）: {e}"),
    }

    crate::codex_config::apply_gateway_to_codex_config(port, &cfg.deepseek_models[0])
        .context("写入 Codex 配置失败")?;
    println!("✔ 已写入 Codex 配置（model_provider = custom，指向本地网关）");

    // 生成合并模型目录
    let official = crate::catalog::official_models_from_cache(&crate::paths::codex_models_cache_path()?)
        .unwrap_or_else(|_| {
            println!("⚠ 未能从 models_cache.json 抽取官方模型，使用内置兜底清单");
            crate::catalog::builtin_official_fallback()
        });
    let catalog = crate::catalog::build_catalog(&cfg.deepseek_models, official);
    crate::catalog::write_catalog_file(&catalog, &crate::paths::catalog_path()?)
        .context("生成合并模型目录失败")?;
    println!("✔ 已生成合并模型目录（官方 + DeepSeek）");

    println!();
    println!("==========================================================");
    println!("  配置完成！");
    println!("  - 端口: {}", cfg.port);
    println!("  - DeepSeek 模型: {}", cfg.deepseek_models.join(", "));
    println!("==========================================================");
    println!("接下来三步：");
    println!("  1. 运行 `codex-gateway start` 启动本地代理；");
    println!("  2. 完全退出并重新打开 Codex（仅此一次，让模型目录生效）；");
    println!("  3. 在模型菜单里点一下即可在官方 / DeepSeek 之间切换，无需再重启。");
    println!();
    println!("常用命令：status（查看状态）/ stop（切回原生）/ uninstall（彻底卸载）");
    Ok(())
}