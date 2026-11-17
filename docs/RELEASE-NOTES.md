# codex-gateway 0.4.0

- 按应用生命周期、模型目录、设置、系统运行时、协议与传输、只读历史诊断重构项目。
- 修复多 agent `agent_message.content` 被删除导致的 HTTP 400；只适配已知不兼容项，保留未知协议和官方高级档位。
- 移除所有 SQLite 会话迁移；原生/网关用户保持会话 provider 身份，启停只切换路由，支持卸载后重新安装。
- 官方账号 Max/Ultra 及未来非空档位不再被降级或删除。支持 Store 和独立桌面运行时，按程序版本选择，避免同步时间戳选中旧版本。
- 新增真实 HTTP 的并发 agent 消息回归、官方在线烟雾测试、当前客户端完整启停测试和历史身份诊断。

验证详情和限制见 [验证记录](REFACTOR-0.4.0.md)。旧客户端请先升级；不同 provider 的旧会话仍需匹配身份后恢复，网关不改写历史来绕过客户端限制。

以下为此前版本记录：

# codex-gateway 0.3.6

- 修复官方请求清洗误删 `agent_message.content`，保留子 agent 启动与回传消息。
- 合并目录保留账号公布的 Max / Ultra 档位，不再统一降级为 xhigh；DeepSeek 档位映射保持独立。
- 识别 Microsoft Store 桌面版运行时，刷新模型时使用实际安装版本。
- 面向当前桌面运行时 0.167.0-alpha.4.1；旧客户端若拒绝 Max / Ultra，应先升级客户端。此版本不再承诺旧枚举客户端兼容。

更新后重启网关并重新打开 Codex 载入模型菜单。模型和速度权限仍由账号目录及官方后端决定。

# codex-gateway 0.3.5

- 统一本地模型文件和 `/models` 返回值，保留 DeepSeek；较窄的原生缓存也会补充已知 GPT-6.x（包括 GPT-6.1 Terra）选项。同名账号元数据和隐藏标记优先，菜单选项不代表账号授权。
- 兼容恢复后仍使用 `openai` 的旧会话：通过 `openai_base_url` 接入网关，停止时恢复原地址。原始 JSONL 历史不改写。
- 重启只替换服务进程，不再中途删除 provider 配置、往返切换会话归属。停止后保留指向官方直连的历史 provider 兼容定义。
- 补齐模型条目必需字段；模型刷新优先使用版本化桌面运行时，避免沿用旧缓存中的客户端版本。
- 控制台按程序版本选择可执行文件；状态区分控制程序与实际服务版本，避免更新后仍运行旧程序。

已有配置的使用者在控制台选择 **5 重启**，然后完全退出并重新打开 Codex。首次使用选择 **4 配置 → 1 启动**。

网关适用于本地后端会话；远程托管会话不经过本机配置，无法由本地网关接入 DeepSeek。停止网关后的官方直连不执行跨模型历史适配。

Original history is preserved. This release unifies model discovery, intercepts native-provider history requests, and improves restart and version handling. Current Desktop runtime validation uses 0.164.0-alpha.11.1; see the audit record for scope and limitations.
