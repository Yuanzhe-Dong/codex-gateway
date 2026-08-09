//! 同步 Codex 会话的 `model_provider`，避免 stop/start 切换 config 后会话从菜单消失。
//!
//! Codex 桌面端按 `model_provider` 过滤会话列表。当网关 stop/start 切换 config 的
//! provider 时，若 SQLite (`state_5.sqlite`) 的 `threads` 表里 `model_provider` 与
//! config 不一致，历史会话会被过滤隐藏。本模块在 stop/start 时同步该字段，
//! 效果与 CC Switch 的 `unifyCodexSessionHistory` 一致。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::paths;

/// 把 `state_5.sqlite` 中所有 `threads` 的 `model_provider` 改为 `target`。
///
/// `target` 通常是 `"openai"`（stop 时）或 `"custom"`（start 时）。
///
/// - 数据库不存在 → 静默跳过（返回 Ok）。
/// - `threads` 表无 `model_provider` 列 → 静默跳过。
/// - 全部会话已是 `target` → 不写入（no-op）。
/// - Codex 正在运行锁库 → busy_timeout 等 5s，仍失败则返回 Err（调用方应吞掉不阻断主流程）。
pub fn sync_session_provider(target: &str) -> Result<()> {
    let db = paths::codex_home()?.join("state_5.sqlite");
    if !db.exists() {
        tracing::debug!("sync_session_provider: {} 不存在，跳过", db.display());
        return Ok(());
    }

    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("打开 {} 失败（Codex 可能正在运行且锁库）", db.display()))?;

    // 等 Codex 释放写锁最多 5 秒
    conn.busy_timeout(Duration::from_secs(5))?;

    // 确认 threads 表有 model_provider 列（旧版 Codex 可能没有）
    let has_col: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('threads') WHERE name='model_provider'")
        .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
        .map(|n| n > 0)
        .unwrap_or(false);
    if !has_col {
        tracing::debug!("sync_session_provider: threads 表无 model_provider 列，跳过");
        return Ok(());
    }

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
        .context("查询 threads 总数失败")?;
    if total == 0 {
        tracing::debug!("sync_session_provider: threads 表为空，跳过");
        return Ok(());
    }

    let matching: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE model_provider = ?1",
            params![target],
            |r| r.get(0),
        )
        .context("查询匹配数失败")?;

    if matching == total {
        tracing::debug!(
            "sync_session_provider: 全部 {} 条会话已是 {}，跳过",
            total,
            target
        );
        return Ok(());
    }

    let changed = conn
        .execute("UPDATE threads SET model_provider = ?1", params![target])
        .context("更新 threads.model_provider 失败（可能锁库）")?;

    tracing::info!(
        "sync_session_provider: 已把 {} 条会话的 provider 同步为 {}（共 {} 条）",
        changed,
        target,
        total
    );
    Ok(())
}

/// 清空会话历史里所有 `reasoning` 项的 `content`（置为空数组 `[]`）。
///
/// 官方 API 要求 `reasoning` 的 `content` 为空数组（max length 0），但会话历史里
/// 累积了模型生成的思维链摘要（`reasoning_text`，content 非空），导致关网关后
/// GPT 直连官方时报 `array too long`。本函数在 stop/restore 时清洗，让历史干净，
/// GPT 直连可用。
///
/// 遍历 `~/.codex/sessions/**/*.jsonl`，逐行解析 JSON，递归找 `type=reasoning`
/// 且 `content` 非空数组的项，置为 `[]`，有改动才写回。返回清洗的 reasoning 项数。
/// 单个文件读/写失败（如被 Codex 锁定）则跳过并 warn，不阻断。
pub fn clean_session_reasoning() -> Result<usize> {
    let sessions_dir = paths::codex_home()?.join("sessions");
    if !sessions_dir.exists() {
        return Ok(0);
    }
    let mut total = 0usize;
    clean_dir_recursive(&sessions_dir, &mut total)?;
    if total > 0 {
        tracing::info!(
            "clean_session_reasoning: 共清洗 {} 个 reasoning content 非空项",
            total
        );
    }
    Ok(total)
}

fn clean_dir_recursive(dir: &Path, total: &mut usize) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "clean_session_reasoning: 读取目录项失败 {}: {}",
                    dir.display(),
                    e
                );
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            clean_dir_recursive(&path, total)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            match clean_jsonl_file(&path) {
                Ok(n) => *total += n,
                Err(e) => tracing::warn!(
                    "clean_session_reasoning: 处理 {} 失败（跳过）: {}",
                    path.display(),
                    e
                ),
            }
        }
    }
    Ok(())
}

fn clean_jsonl_file(path: &Path) -> Result<usize> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut changed = false;
    let mut cleaned = 0usize;
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                let n = clean_reasoning_recursive(&mut v);
                if n > 0 {
                    cleaned += n;
                    changed = true;
                    out.push_str(&v.to_string());
                } else {
                    out.push_str(line);
                }
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    if changed {
        fs::write(path, &out)
            .with_context(|| format!("写入 {} 失败（可能被 Codex 锁定）", path.display()))?;
    }
    Ok(cleaned)
}

/// 递归把 `type=reasoning` 且 `content` 非空数组的项的 content 置为 `[]`。返回清洗数。
fn clean_reasoning_recursive(v: &mut Value) -> usize {
    let mut count = 0;
    match v {
        Value::Object(obj) => {
            if obj.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                if let Some(content) = obj.get_mut("content") {
                    if let Some(arr) = content.as_array() {
                        if !arr.is_empty() {
                            *content = Value::Array(vec![]);
                            count += 1;
                        }
                    }
                }
            }
            for (_, val) in obj.iter_mut() {
                count += clean_reasoning_recursive(val);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                count += clean_reasoning_recursive(item);
            }
        }
        _ => {}
    }
    count
}
