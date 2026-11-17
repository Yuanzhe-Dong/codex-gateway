# 诊断与维护 / Diagnostics and maintenance

日常操作使用控制台即可。以下命令供维护和排障使用，不需要按顺序执行。在发布包目录运行 `codex-gateway.exe`；源码构建后对应 `target/release/codex-gateway.exe`。

| 命令 / Command | 用途 / Purpose |
| --- | --- |
| `status --offline` | 检查本地网关、连接配置和历史兼容状态，不探测外网 / Inspect local state without upstream probes |
| `refresh-models` | 按当前 Codex 登录账号刷新官方模型目录，不切换连接配置 / Refresh the account model catalog |
| `test-deepseek` | 两次正常计费的小请求，测试 Flash 文本和函数调用 / Two bounded, billed text and function-call probes |
| `test-deepseek --model deepseek-v4-pro` | 对 Pro 执行相同测试 / Run the same probes with Pro |
| `repair-sessions` | 只读统计可能不兼容的历史推理项 / Read-only compatibility scan |
| `check-session <JSONL路径>` | 使用真实适配入口验证官方和 DS 请求副本，不请求上游 / Validate both outgoing copies offline |

`test-deepseek` 不启动网关、不改 Codex 配置，不执行模型生成的工具。两次请求分别限制 64 / 128 个输出 token（包含推理 token），默认关闭思考，单次最多 30 秒。这是接口检查，不是模型能力评测。

日志默认位于 `%USERPROFILE%\.codex-gateway\gateway.log`；设置 `CODEX_GATEWAY_HOME` 时使用该目录。`RUST_LOG=debug` 可记录请求结构，日志不会记录输入正文或完整凭证。

## 会话和模型列表

首次启用网关或切换连接配置后，需要完全退出并重新打开 Codex。网关的合并模型目录包含已配置的 DS 模型；当前客户端已打开的会话可能仍持有旧的模型列表。若只有部分会话缺少 DS，先用 `status --offline` 确认连接配置，再比较新会话与完全重新打开后的旧会话。不同项目的 `.codex/config.toml` 和 profile 也可能覆盖全局连接或模型目录。

原始 JSONL 历史始终只读。发往官方的请求副本会过滤第三方推理；DS 副本保留明文推理。`repair-sessions --apply` 已禁用，不能用它删除历史。

## 进程权限

启动或停止时报“核对进程身份失败，os error 31”，可能是旧进程已退出但其系统句柄仍被其它程序持有。0.3.3 起在查询映像路径前后检查进程对象的退出状态，这种过期记录不会再阻止启动，无需删除会话历史或手工强杀 PID。

控制台仅在启动、停止、重启返回权限错误后，通过 Windows UAC 重试同一个操作，并保留当前两个数据目录。取消系统提示会保留失败状态，不会显示成功，也不会循环提权。

旧 PID 无法访问时，只有已有实例锁可以独占且配置端口空闲，程序才忽略过期记录。否则保留记录，不终止无法核实身份的进程。

English: these commands are optional maintenance tools. History checks never modify JSONL files or contact upstreams. API probes incur normal DeepSeek usage charges. Restart Codex after initially activating the gateway; compare project/profile overrides and newly opened conversations when investigating a missing model menu.

## 目录解析失败与 DS 选项

0.3.4–0.3.5 为旧客户端将 max/ultra 转成 xhigh。0.4.0 面向支持这些档位的当前客户端，保留账号原始档位；旧客户端若解析失败，请先升级。原始账号快照和会话历史不改写，DS 请求仍把 xhigh 转成其支持的 max。

## 0.4.0：子 agent 请求缺少 content

`Missing required parameter: input[N].content` 若对应 `agent_message`，可能来自旧版网关误删通信正文。0.4.0 在官方请求副本中完整保留该类型，仅清理已知工具项的不兼容字段，未知类型原样保留；无需删除或改写原会话。Ultra 使用前也应更新到此版本。

启动网关时，超过一天的官方账号快照会尝试在线更新（最多 30 秒一次，自动代理的连接失败可能再直连重试）。离线或登录失效时保留本地目录。GPT-6.1 Terra 的账号可用性以实际在线目录为准，内置兜底清单不授予模型权限。

若修复目录后仍只有个别会话不显示自定义模型，还需检查桌面端自身的模型过滤逻辑；网关无法承诺覆盖客户端所有菜单限制。

## 0.3.5：重复出现 content 长度错误

`Invalid input[N].content: array too long ... maximum length 0` 表示官方后端收到了不兼容的历史推理项。网关只适配经过它的请求；完整原始历史继续保留。

检查 `status --offline` 中的配置接管、实际服务版本和模型选项。控制程序版本与服务版本不同，使用 `restart`。官方直连、尚未重新加载配置的桌面进程、或远程托管会话都可能绕过本地过滤。先观察出错时 `gateway.log` 是否新增对应模型的请求，不能仅凭控制台窗口开着就判断请求已经过网关。

旧会话可能继续使用 `openai` provider。0.3.5 除默认 provider 外还设置 `openai_base_url` 接入本地网关，停止时恢复原值；不允许重定义新客户端保留的 `model_providers.openai`。重启网关后需要完全退出并重开桌面端。保留的 `codex_gateway` 直连别名只负责让历史 provider 可以解析，停止后不会执行过滤。

本地文件目录与 `/models` 接口统一；刷新接口不再用官方列表替换掉 DeepSeek。已知 6.x 选项在窄缓存下也会补充，但不代表实际账号授权。版本化桌面后端与旧 `bin/codex.exe` 可能不同，排查必须使用桌面日志里实际启动的程序。

可选的真实桌面后端回归（仅用隔离目录、虚构 key 和合成历史，不发送模型生成请求）：

```powershell
python tests/desktop_compat.py --codex-exe <桌面端实际codex.exe路径> --gateway-exe target/release/codex-gateway.exe
```

该检查覆盖模型目录、旧会话恢复、启停与原始历史前缀保留；不代表桌面 GUI 每个远程工作模式或上游账号调用均已验证。当前验证版本为 0.164.0-alpha.11.1。

## 0.4.0：modelProviderMismatch

当前 Codex 0.167 在加载历史前检查会话身份与运行时的默认 provider，单次 `thread/resume` 参数不能绕过。旧网关只改 SQLite、保留 JSONL 原值，会使两个数据源不一致。0.4.0 已移除这条写入路径；官方用户保持 `openai`，旧网关用户保持 `codex_gateway`，启停只切换连接地址。

`codex-gateway check-session <历史路径>` 会显示原始 provider，并检查消息/agent 正文是否完整。若某个旧会话身份不同，需要用匹配该 provider 的 Codex 运行时恢复；不要直接批量改 SQLite 或历史。CLI 可用 `codex -c 'model_provider="openai"' resume <会话ID>`（网关历史使用 `codex_gateway`）。这是独立 CLI 进程的配置，不是桌面内按会话切换开关。需要跨模型适配时仍须保持网关运行。已验证匹配 provider 的 app-server 可以恢复同目录的另一种历史；没有承诺修复旧版本已经造成的全部索引异常。

## 维护验证

- `tests/desktop_compat.py`：隔离配置与合成会话，检查当前客户端模型目录、Max/Ultra、两轮启停、相同身份恢复及混合身份限制。不调用模型。
- `tests/live_smoke.py --live`：自愿执行一次正常计费的官方请求，只发送合成 agent 消息，不启动真实子 agent、不发送历史。OAuth 仅在内存使用。默认低档位 GPT-6 Luna。
- 历史审计文档反映当时版本；以 `REFACTOR-0.4.0.md` 的最新验证范围为准。
