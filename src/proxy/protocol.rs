//! 纯内存协议适配：只修改已知上游不兼容字段，不读写会话或配置。
use crate::proxy::routing::Upstream;
use axum::body::Bytes;

/// 按目标上游生成请求副本；调用方持有的原始历史不发生任何变化。
pub(crate) fn prepare_request_body(body: &[u8], up: Upstream) -> Bytes {
    match up {
        Upstream::Official => sanitize_for_official(body),
        Upstream::DeepSeek => map_deepseek_reasoning_effort(body),
    }
}

/// 只记录输入结构，不打印用户或推理文本。
pub(super) fn log_input_shape(body: &[u8]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    if let Some(arr) = v.get("input").and_then(|i| i.as_array()) {
        for (i, item) in arr.iter().enumerate() {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("?");
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("-");
            let clen = item
                .get("content")
                .and_then(|c| c.as_array())
                .map(|c| c.len())
                .unwrap_or(0);
            let content_present = item.get("content").is_some();
            tracing::debug!("input[{i}] type={ty} role={role} content_present={content_present} content_len={clen}");
        }
    }
}

/// 官方 /responses 清洗：按 input 项的 `type` 区分处理 content 字段。
///
/// - `reasoning`：官方要求 content 为空数组（length 0），桌面端会原样带非空 content，
///   被 400 "array too long" 拒绝，故强制置空；非官方来源的 reasoning 项（见
///   [`is_foreign_reasoning`]）整项丢弃。
/// - `message` / `agent_message`：保留 content，包括正常消息和子 agent 通信正文。
/// - 已知工具类型（`additional_tools` / `function_call` / `function_call_output` /
///   `custom_tool_call` / `custom_tool_call_output`）不应有 content 字段；
///   桌面端可能误带，移除以免 400 "Unknown parameter: 'input[N].content'"。
///   此外 tool call 的 `id`/`call_id` 需以 `ws` 开头；桌面端经 custom provider 生成的是
///   `call_` 前缀，改写为 `ws_` 前缀以免官方后端报 `Invalid 'input[N].id'`。
/// - 其余类型保留所有字段，避免客户端扩展协议时误删正文或关联 ID。
fn sanitize_for_official(body: &[u8]) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Bytes::copy_from_slice(body);
    };
    if let Some(input) = v.get_mut("input").and_then(|i| i.as_array_mut()) {
        let before = input.len();
        input.retain(|item| !is_foreign_reasoning(item));
        let removed = before - input.len();
        let mut normalized = 0;
        for item in input.iter_mut() {
            let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match ty {
                "reasoning" => {
                    if item
                        .get("content")
                        .and_then(|c| c.as_array())
                        .is_some_and(|c| !c.is_empty())
                    {
                        normalized += 1;
                    }
                    item["content"] = serde_json::Value::Array(vec![]);
                }
                "additional_tools"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "function_call"
                | "function_call_output" => {
                    if let Some(obj) = item.as_object_mut() {
                        obj.remove("content");
                        // OpenAI 官方后端要求 tool call 的 id 以 "ws" 开头；
                        // 桌面端经 custom provider 生成的 id 是 "call_" 开头，需改写前缀。
                        for key in ["id", "call_id"] {
                            if let Some(id) = obj.get(key).and_then(|v| v.as_str()) {
                                if let Some(rest) = id.strip_prefix("call_") {
                                    obj.insert(
                                        key.to_string(),
                                        serde_json::Value::String(format!("ws_{rest}")),
                                    );
                                }
                            }
                        }
                    }
                }
                // 未知协议项原样透传；新 agent / 图像 / 搜索类型不能套用工具规则。
                _ => {}
            }
        }
        if removed > 0 || normalized > 0 {
            tracing::info!(
                removed,
                normalized,
                "官方请求副本已适配历史推理；原始会话未修改"
            );
        }
    }
    Bytes::from(v.to_string())
}

/// 判断一个 input 项是否为「非官方来源的 reasoning」，是则应从发往官方的请求里剔除。
///
/// 官方推理项的 `id` 以 `rs_` 开头，`encrypted_content` 是 `gAAAAA` 开头的真密文；
/// 经 DeepSeek 路由产生的推理项则是 UUID 形状的 id 与伪密文。同一会话从 DeepSeek 切回
/// 官方模型时，官方会解密校验历史里的密文，失败即整个请求 400
/// `invalid_encrypted_content`。由于发往官方时 reasoning 的 content 本来就要置空，
/// 这类项已无可用信息，直接丢弃最稳妥。
pub(crate) fn is_foreign_reasoning(item: &serde_json::Value) -> bool {
    if item.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
        return false;
    }
    if let Some(blob) = item
        .get("encrypted_content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        // DeepSeek 现在也会使用 rs_ ID，因此 ID 前缀不能覆盖伪密文判断。
        return !blob.starts_with("gAAAAA");
    }
    let official_id = item
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.starts_with("rs_"))
        .unwrap_or(false);
    !official_id
}

/// DeepSeek 路由：把 Codex 的 `reasoning.effort` 映射到 DeepSeek 实际支持的档位，
/// 并摘掉 DeepSeek 无法真正提供的 `reasoning.encrypted_content`。
///
/// DeepSeek 支持 none（非思考）/ low / high / max。
/// 旧目录中的 xhigh 与官方 ultra 在 DS 路由映射为 max，max 原样保留；
/// 其余档位映射到 DeepSeek 接受的最近档位（medium→high、minimal→low）。
///
/// 另外，Codex 只要开启 reasoning 就会带 `include: ["reasoning.encrypted_content"]`，
/// DeepSeek 文档只支持 reasoning 的明文 content，不支持 encrypted_content / include。
/// 仅在 DS 请求副本中摘掉不支持的密文字段，完整保留明文推理和原始历史。
fn map_deepseek_reasoning_effort(body: &[u8]) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Bytes::copy_from_slice(body);
    };
    if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
        let canonical = crate::proxy::routing::canonical_deepseek_model(model).to_owned();
        v["model"] = serde_json::json!(canonical);
    }
    // 官方 Fast/Ultrafast 模式不属于 DeepSeek 参数，跨模型切换时不得继承。
    if let Some(obj) = v.as_object_mut() {
        obj.remove("service_tier");
    }
    if let Some(input) = v.get_mut("input").and_then(|i| i.as_array_mut()) {
        for item in input {
            if item["type"] == "reasoning" {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("encrypted_content");
                }
            }
        }
    }
    if let Some(include) = v.get_mut("include").and_then(|i| i.as_array_mut()) {
        let before = include.len();
        include.retain(|x| x.as_str() != Some("reasoning.encrypted_content"));
        if include.len() != before {
            tracing::info!("摘除 DeepSeek 请求中的 reasoning.encrypted_content");
        }
    }
    if let Some(reasoning) = v.get_mut("reasoning").and_then(|r| r.as_object_mut()) {
        let effort = reasoning
            .get("effort")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string());
        if let Some(effort) = effort {
            let mapped = match effort.as_str() {
                "xhigh" | "ultra" => "max",
                "medium" => "high",
                "minimal" => "low",
                other => other,
            };
            if mapped != effort.as_str() {
                reasoning.insert(
                    "effort".to_string(),
                    serde_json::Value::String(mapped.to_string()),
                );
                tracing::info!(from = %effort, to = mapped, "映射 DeepSeek reasoning effort");
            }
        }
    }
    Bytes::from(v.to_string())
}

pub(super) fn parse_model(body: &[u8]) -> std::result::Result<String, &'static str> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| "请求体必须是合法 JSON")?;
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.trim().is_empty())
        .ok_or("请求缺少非空 model 字段")?;
    Ok(model.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unknown_items_multimodal_messages_and_tool_links_are_stable() {
        let raw = json!({"model":"gpt-6.1-sol", "input":[
            {"type":"future_agent_event", "id":"call_future", "content":[{"type":"input_text","text":"retain"}], "metadata":{"opaque":true}},
            {"type":"message", "role":"user", "content":[{"type":"input_image","image_url":"data:image/png;base64,fixture"}]},
            {"type":"function_call", "id":"call_item", "call_id":"call_pair", "name":"tool", "arguments":"{}"},
            {"type":"function_call_output", "call_id":"call_pair", "output":[{"type":"input_text","text":"result"}]}
        ]});
        let original = raw.clone();
        let once = prepare_request_body(raw.to_string().as_bytes(), Upstream::Official);
        let result: serde_json::Value = serde_json::from_slice(&once).unwrap();
        assert_eq!(result["input"][0], original["input"][0]);
        assert_eq!(result["input"][1], original["input"][1]);
        assert_eq!(result["input"][2]["call_id"], result["input"][3]["call_id"]);
        assert_eq!(result["input"][3]["output"], original["input"][3]["output"]);
        assert_eq!(once, prepare_request_body(&once, Upstream::Official));
        assert_eq!(raw, original);
    }

    #[test]
    fn official_preserves_agent_messages_and_advanced_settings() {
        // 子 agent 启动及回传均使用 agent_message；删除正文会导致 input[N].content 缺失。
        for effort in ["max", "ultra"] {
            let raw = json!({
                "model": "gpt-6.1-sol", "service_tier": "priority",
                "reasoning": {"effort": effort},
                "input": [
                    {"type":"agent_message", "id":"msg_task", "author":"/root",
                     "recipient":"/root/worker", "content":[{"type":"input_text","text":"Inspect the change."}]},
                    {"type":"agent_message", "id":"msg_result", "author":"/root/worker",
                     "recipient":"/root", "content":[{"type":"input_text","text":"Inspection complete."}]}
                ]
            });
            let out: serde_json::Value = serde_json::from_slice(&prepare_request_body(
                raw.to_string().as_bytes(),
                Upstream::Official,
            ))
            .unwrap();
            assert_eq!(out, raw, "通信正文、身份和官方档位应完整透传");
        }
    }

    #[test]
    fn sanitize_official_empties_reasoning_keeps_message_removes_others() {
        // reasoning content 置空；message content 保留（含 developer 系统指令）；
        // 非 message 类型（additional_tools / custom_tool_call 等）移除 content 字段
        let raw = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "sys"}]},
                {"type": "reasoning", "id": "rs_1", "summary": [], "content": [{"type": "reasoning_text", "text": "think"}]},
                {"type": "additional_tools", "role": "developer", "content": [], "tools": [{"type": "function", "name": "f"}]},
                {"type": "custom_tool_call", "name": "f", "arguments": "{}", "content": [{"type": "output_text", "text": "x"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(raw.to_string().as_bytes())).unwrap();
        let input = out["input"].as_array().unwrap();
        // developer message content 保留（系统指令）
        assert_eq!(
            input[0]["content"].as_array().unwrap().len(),
            1,
            "developer message content 应保留"
        );
        // reasoning content 置空，id/summary 保留
        assert!(
            input[1]["content"].as_array().unwrap().is_empty(),
            "reasoning.content 应置空"
        );
        assert_eq!(input[1]["id"], "rs_1", "reasoning 的 id/summary 应保留");
        // additional_tools 的 content 应被移除，tools 保留
        assert!(
            input[2].get("content").is_none(),
            "additional_tools 不应残留 content"
        );
        assert!(
            input[2].get("tools").is_some(),
            "additional_tools 的 tools 应保留"
        );
        // custom_tool_call 的 content 应被移除，name 保留
        assert!(
            input[3].get("content").is_none(),
            "custom_tool_call 不应残留 content"
        );
        assert_eq!(input[3]["name"], "f", "custom_tool_call 的 name 应保留");
        // 用户消息保留
        assert_eq!(
            input[4]["content"].as_array().unwrap().len(),
            1,
            "用户消息 content 应保留"
        );
    }

    #[test]
    fn sanitize_official_rewrites_tool_call_id_prefix() {
        let raw = json!({
            "model": "gpt-5.6-terra",
            "input": [
                {"type": "custom_tool_call", "name": "f", "arguments": "{}", "id": "call_00_abc123", "content": []},
                {"type": "custom_tool_call_output", "call_id": "call_00_abc123", "output": "x"},
                {"type": "function_call", "name": "g", "arguments": "{}", "id": "call_00_def456"},
                {"type": "function_call_output", "call_id": "call_00_def456", "output": "y"}
            ]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(raw.to_string().as_bytes())).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(
            input[0]["id"], "ws_00_abc123",
            "custom_tool_call id 应改写为 ws_ 前缀"
        );
        assert_eq!(
            input[1]["call_id"], "ws_00_abc123",
            "custom_tool_call_output call_id 应改写"
        );
        assert_eq!(
            input[2]["id"], "ws_00_def456",
            "function_call id 应改写为 ws_ 前缀"
        );
        assert_eq!(
            input[3]["call_id"], "ws_00_def456",
            "function_call_output call_id 应改写"
        );
    }

    #[test]
    fn sanitize_official_drops_foreign_encrypted_content() {
        // 经 DeepSeek 路由产生的 reasoning 项带 UUID id 与伪密文，转发官方会被
        // 400 invalid_encrypted_content 拒绝，应整项丢弃；官方自身（rs_ id / gAAAAA 密文）保留。
        let raw = json!({
            "model": "gpt-5.6-terra",
            "input": [
                {"type": "reasoning", "id": "5628d400-025", "summary": [], "content": [],
                 "encrypted_content": "a49a46cd-af4c-4b7c-a8fb-fb4afff11114-0"},
                {"type": "reasoning", "id": "rs_1", "summary": [], "content": [],
                 "encrypted_content": "gAAAAABfakeofficialblob"},
                {"type": "reasoning", "id": "5628d400-026", "summary": [], "content": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(raw.to_string().as_bytes())).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "两条非官方 reasoning 应被丢弃");
        assert_eq!(input[0]["id"], "rs_1", "官方 reasoning 项应保留");
        assert_eq!(
            input[0]["encrypted_content"], "gAAAAABfakeofficialblob",
            "官方真密文应原样保留"
        );
        assert_eq!(input[1]["type"], "message", "消息项不受影响");
    }

    #[test]
    fn map_deepseek_strips_encrypted_content_include() {
        // Codex 开 reasoning 就会要 encrypted_content，DeepSeek 只给伪密文，
        // 会污染会话历史，故转发前摘掉该 include，其余 include 保留。
        let raw = json!({
            "model": "deepseek-v4-flash",
            "include": ["reasoning.encrypted_content", "message.output_text.logprobs"],
            "reasoning": {"effort": "xhigh"},
            "input": "hi"
        });
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(
            out["include"],
            json!(["message.output_text.logprobs"]),
            "应只摘掉 reasoning.encrypted_content"
        );
        assert_eq!(out["reasoning"]["effort"], "max", "effort 映射不受影响");

        // 无 include 字段时不报错、不新增字段
        let raw = json!({"model": "deepseek-v4-flash", "input": "hi"});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert!(out.get("include").is_none());
    }

    #[test]
    fn map_deepseek_effort_xhigh_to_max() {
        // xhigh → max（DeepSeek 最深档）
        let raw = json!({
            "model": "deepseek-v4-flash",
            "reasoning": {"effort": "xhigh", "summary": "none"},
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "max");

        // low 保持 low（当前 DeepSeek 支持低强度推理）
        let raw = json!({"model": "deepseek-v4-flash", "reasoning": {"effort": "low"}});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "low");

        // medium → high
        let raw = json!({"model": "deepseek-v4-flash", "reasoning": {"effort": "medium"}});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "high");

        // high 保持不变
        let raw = json!({"model": "deepseek-v4-flash", "reasoning": {"effort": "high"}});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "high");

        // none 保持不变
        let raw = json!({"model": "deepseek-v4-flash", "reasoning": {"effort": "none"}});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "none");

        // 无 reasoning 字段时原样返回
        let raw = json!({"model": "deepseek-v4-flash", "input": "hi"});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(raw.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["model"], "deepseek-flash");
        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_id_cannot_override_foreign_ciphertext_and_shorthand_message_survives() {
        let input = json!({"input": [
            {"type":"reasoning","id":"rs_fake","encrypted_content":"uuid-fake","content":[]},
            {"role":"user","content":"keep this shorthand message"}
        ]});
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(input.to_string().as_bytes())).unwrap();
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
        assert_eq!(out["input"][0]["content"], "keep this shorthand message");
    }

    #[test]
    fn deepseek_preserves_plain_reasoning_and_low_effort() {
        let input = json!({"model":"deepseek-flash", "reasoning":{"effort":"minimal"},
        "input":[{"type":"reasoning","content":[{"type":"reasoning_text","text":"keep"}],"encrypted_content":"official"}]});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(input.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["reasoning"]["effort"], "low");
        assert_eq!(out["input"][0]["content"][0]["text"], "keep");
        assert!(out["input"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn history_compatibility_switches_ds_official_ds_without_losing_source() {
        let mut history = vec![json!({"role":"user","content":"keep user message"})];
        for i in 0..38 {
            history.push(json!({"type":"reasoning","id":format!("ds-{i}"),
            "encrypted_content":format!("foreign-{i}"),
            "content":[{"type":"reasoning_text","text":format!("DS thought {i}")}]}));
        }
        history.extend([
        json!({"type":"function_call","id":"call_tool","call_id":"call_tool","name":"f","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":"call_tool","output":"keep tool result"}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"keep final answer"}]}),
        json!({"type":"reasoning","id":"rs_official","encrypted_content":"gAAAAA-keep","content":[]}),
    ]);
        let source =
            json!({"model":"deepseek-flash","input":history,"reasoning":{"effort":"xhigh"}});
        let source_bytes = source.to_string().into_bytes();
        let ds_before: serde_json::Value =
            serde_json::from_slice(&prepare_request_body(&source_bytes, Upstream::DeepSeek))
                .unwrap();
        let mut official_request = source.clone();
        official_request["model"] = json!("gpt-6.1-sol");
        let official: serde_json::Value = serde_json::from_slice(&prepare_request_body(
            official_request.to_string().as_bytes(),
            Upstream::Official,
        ))
        .unwrap();
        assert_eq!(official["input"].as_array().unwrap().len(), 5);
        assert_eq!(official["input"][0], source["input"][0]);
        assert_eq!(
            official["input"][1]["call_id"],
            official["input"][2]["call_id"]
        );
        assert_eq!(official["input"][2]["output"], "keep tool result");
        assert_eq!(official["input"][3], source["input"][41]);
        assert_eq!(official["input"][4]["encrypted_content"], "gAAAAA-keep");
        let ds_after: serde_json::Value =
            serde_json::from_slice(&prepare_request_body(&source_bytes, Upstream::DeepSeek))
                .unwrap();
        assert_eq!(ds_after, ds_before);
        assert_eq!(ds_after["input"].as_array().unwrap().len(), 43);
        for i in 1..=38 {
            assert_eq!(
                ds_after["input"][i]["content"],
                source["input"][i]["content"]
            );
        }
        assert_eq!(ds_after["input"][39]["call_id"], "call_tool");
        assert_eq!(source.to_string().as_bytes(), source_bytes);
        assert_eq!(source["input"][1]["encrypted_content"], "foreign-0");
    }

    #[test]
    fn new_flash_alias_and_official_speed_and_effort_switch_cleanly() {
        let input = json!({"model":"deepseek-v4-flash-vision-exp",
        "service_tier":"ultrafast", "reasoning":{"effort":"ultra"},
        "input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,test"}]}]});
        let out: serde_json::Value =
            serde_json::from_slice(&map_deepseek_reasoning_effort(input.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out["model"], "deepseek-flash");
        assert_eq!(out["reasoning"]["effort"], "max");
        assert!(out.get("service_tier").is_none());
        assert_eq!(out["input"], input["input"]);
        let official = json!({"model":"gpt-6.1-sol","service_tier":"ultrafast","reasoning":{"effort":"max"},"input":"keep"});
        let out: serde_json::Value =
            serde_json::from_slice(&sanitize_for_official(official.to_string().as_bytes()))
                .unwrap();
        assert_eq!(out, official);
    }
}
