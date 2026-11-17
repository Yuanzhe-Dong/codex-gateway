//! 会话只读诊断；所有兼容转换复用请求协议层，不写数据库或 JSONL。
use crate::runtime::paths;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

#[derive(Default, Debug)]
pub struct RepairReport {
    pub files: usize,
    pub items: usize,
    pub skipped: usize,
}

fn count_incompatible(raw: &str) -> usize {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record["type"] == "response_item" && record["payload"]["type"] == "reasoning"
        })
        .filter(|record| {
            let item = &record["payload"];
            crate::proxy::protocol::is_foreign_reasoning(item)
                || item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
        })
        .count()
}

/// 保留旧命令名称，仅允许只读检查；在解析任何目录之前拒绝写回。
pub fn repair_sessions(apply: bool) -> Result<RepairReport> {
    anyhow::ensure!(!apply, "已停用会话历史改写，请启动网关适配请求副本。");
    let home = paths::codex_home()?;
    let mut report = RepairReport::default();
    for name in ["sessions", "archived_sessions"] {
        let dir = home.join(name);
        if dir.is_dir() {
            inspect_dir(&dir, &mut report)?;
        }
    }
    Ok(report)
}

fn inspect_dir(dir: &Path, report: &mut RepairReport) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        // 不跟随符号链接；此路径只有读取操作。
        if ty.is_dir() {
            inspect_dir(&path, report)?;
        } else if ty.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            match fs::read_to_string(&path) {
                Ok(raw) => {
                    let n = count_incompatible(&raw);
                    if n > 0 {
                        report.files += 1;
                        report.items += n;
                    }
                }
                Err(error) => {
                    report.skipped += 1;
                    tracing::warn!("无法读取 {}：{error}", path.display());
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct HistoryCheck {
    pub provider: Option<String>,
    pub items: usize,
    pub official_removed: usize,
    pub deepseek_reasoning: usize,
}

/// 用真实历史构造两个内存请求，调用与代理相同的适配入口，不访问上游或写回文件。
pub fn check_session(path: &Path) -> Result<HistoryCheck> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("读取会话失败：{}", path.display()))?;
    let mut history = Vec::new();
    let mut provider = None;
    for line in raw.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["type"] == "session_meta" {
            provider = record["payload"]["model_provider"]
                .as_str()
                .map(str::to_owned);
        } else if record["type"] == "compacted" {
            if let Some(replacement) = record["payload"]["replacement_history"].as_array() {
                history = replacement.clone();
            }
        } else if record["type"] == "response_item" {
            history.push(record["payload"].clone());
        }
    }
    anyhow::ensure!(!history.is_empty(), "会话中未找到可验证的历史项");
    let mut request = serde_json::json!({"model":"gpt-6.1-sol", "input":history});
    let official: Value = serde_json::from_slice(&crate::proxy::protocol::prepare_request_body(
        request.to_string().as_bytes(),
        crate::proxy::routing::Upstream::Official,
    ))?;
    request["model"] = serde_json::json!("deepseek-flash");
    let deepseek: Value = serde_json::from_slice(&crate::proxy::protocol::prepare_request_body(
        request.to_string().as_bytes(),
        crate::proxy::routing::Upstream::DeepSeek,
    ))?;
    let ds_items = deepseek["input"]
        .as_array()
        .context("DS 请求缺少 input 数组")?;
    anyhow::ensure!(ds_items.len() == history.len(), "DS 历史项数量发生变化");
    for (original, outgoing) in history.iter().zip(ds_items) {
        let mut expected = original.clone();
        if expected["type"] == "reasoning" {
            if let Some(object) = expected.as_object_mut() {
                object.remove("encrypted_content");
            }
        }
        anyhow::ensure!(&expected == outgoing, "DS 历史正文或推理内容发生变化");
    }
    let official_items = official["input"]
        .as_array()
        .context("官方请求缺少 input 数组")?;
    anyhow::ensure!(
        !official_items
            .iter()
            .any(crate::proxy::protocol::is_foreign_reasoning),
        "官方请求仍含第三方推理"
    );
    let retained: Vec<_> = history
        .iter()
        .filter(|item| !crate::proxy::protocol::is_foreign_reasoning(item))
        .collect();
    anyhow::ensure!(
        retained.len() == official_items.len(),
        "官方请求删除了非推理项"
    );
    for (original, outgoing) in retained.into_iter().zip(official_items) {
        if !matches!(
            original["type"].as_str(),
            Some(
                "reasoning"
                    | "additional_tools"
                    | "function_call"
                    | "function_call_output"
                    | "custom_tool_call"
                    | "custom_tool_call_output"
            )
        ) {
            anyhow::ensure!(
                original == outgoing,
                "官方请求修改了消息、agent 通信或未知协议项"
            );
        }
    }
    Ok(HistoryCheck {
        provider,
        items: history.len(),
        official_removed: history.len() - official_items.len(),
        deepseek_reasoning: ds_items
            .iter()
            .filter(|item| {
                item["type"] == "reasoning"
                    && item["content"].as_array().is_some_and(|c| !c.is_empty())
            })
            .count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_scan_never_modifies_records_and_rejects_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.jsonl");
        let raw = concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"reasoning\",\"content\":[\"user data\"]}}\r\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"content\":[\"DS thought\"]}}\r\n",
            "{\"incomplete\":");
        fs::write(&path, raw).unwrap();
        let _writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let mut report = RepairReport::default();
        inspect_dir(dir.path(), &mut report).unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(report.items, 1);
        assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
        assert!(repair_sessions(true)
            .unwrap_err()
            .to_string()
            .contains("已停用"));
        assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
    }

    #[test]
    fn check_session_uses_latest_compaction_and_preserves_all_ds_thoughts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut lines = vec![
            serde_json::json!({"type":"response_item","payload":{"type":"reasoning","id":"old","content":["old"]}}),
            serde_json::json!({"type":"compacted","payload":{"replacement_history":[{"role":"user","content":"keep"}]}}),
        ];
        for i in 0..38 {
            lines.push(serde_json::json!({"type":"response_item","payload":{
                "type":"reasoning","id":format!("ds-{i}"),"encrypted_content":"foreign",
                "content":[{"type":"reasoning_text","text":format!("thought {i}")}]}}));
        }
        lines.push(serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}));
        let raw = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n";
        fs::write(&path, &raw).unwrap();
        let report = check_session(&path).unwrap();
        assert_eq!(report.items, 40);
        assert_eq!(report.official_removed, 38);
        assert_eq!(report.deepseek_reasoning, 38);
        assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
    }
}
