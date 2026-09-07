[English](./README.en.md) | 中文

# codex-gateway

**让 Codex 同时拥有 GPT 的大脑和 DeepSeek 的性价比——同一个会话里点一下就切，上下文不丢、任务不断。**

**核心优势：** 与 Codex++ / CC Switch 不同，codex-gateway 能把官方订阅套餐和 DeepSeek 真正放在一起用——同一个菜单、同一个会话，官方模型走你的 ChatGPT 订阅额度，DeepSeek 模型走你的 DeepSeek API，各用各的。其他工具要么只能二选一切换着用，要么合并使用但 GPT 也改走 OpenAI API 按量计费，订阅套餐完全用不上。

<div align="center">
  <img src="./assets/demo.png" width="600" alt="效果预览">
</div>

Rust 编写的本地网关（单文件 exe，零运行时依赖）。Codex 把 `config.toml` 指向网关，网关按请求里的 `model` 字段自动路由到官方或 DeepSeek，无需协议转换，SSE 流式原样透传。

## 工作原理

```
              ┌────────────────────────────────────────────┐
 Codex ─────► │ codex-gateway (127.0.0.1:17899)            │
              │  按 model 路由：                             │
              │   deepseek-* ──► https://api.deepseek.com   │
              │   其它(官方)  ──► https://chatgpt.com/...    │
              └────────────────────────────────────────────┘
```

- 官方请求透传 Codex 的 OAuth 登录态，零额外配置
- DeepSeek 请求替换为你的 API Key
- 合并模型目录让官方 + DeepSeek 模型同时出现在 Codex 菜单里

## 命令

| 命令          | 作用                         |
| ----------- | -------------------------- |
| `setup`     | 交互式配置向导（填 API Key、选模型、设端口） |
| `start`     | 启动网关，切到 DeepSeek 模式        |
| `stop`      | 停止网关，切回原生 GPT 模式（会话不丢）     |
| `status`    | 查看运行状态与上游连通性               |
| `uninstall` | 彻底卸载（删配置、备份，会话不丢）          |

**`stop`** **vs** **`uninstall`：** `stop` 保留网关数据，`start` 秒切回；`uninstall` 全删，需重新 `setup`。两者都不删 `~/.codex/` 下的会话历史。

## 快速开始

```powershell
# 0. 先 cd 到项目目录（exe 所在路径）

cd c:\Codex无缝接私有API

# 1. 配置（一次性）

.\target\release\codex-gateway.exe setup

# 2. 启动

.\target\release\codex-gateway.exe start

# 3. 重启 Codex，模型菜单里即可切换 GPT / DeepSeek
```

日常切换：

```powershell
.\target\release\codex-gateway.exe stop    # 切回原生 GPT
.\target\release\codex-gateway.exe start   # 切回 DeepSeek
```

## 控制台脚本与开始菜单快捷方式

项目根目录自带菜单式控制台，覆盖全部操作：

- **`codex-gateway 控制台.ps1`**：界面核心（PowerShell 脚本，菜单：`1 启动 / 2 停止 / 3 状态 / 4 配置向导 / 5 查看日志 / 6 卸载 / 0 退出`；支持 `start|stop|status|setup|log|uninstall` 参数模式）
- **`codex-gateway 控制台.bat`**：纯 ASCII 启动器，转发给上面的 ps1（直接双击 bat 也可用）

> 界面用 PowerShell 而不是 bat 实现，是为了绕开 cmd 在 UTF-8 代码页下解析中文批处理的已知 bug（中文会乱码/截断）。

一键安装到开始菜单（当前用户，无需管理员）：

```powershell
powershell -ExecutionPolicy Bypass -File "安装到开始菜单.ps1"
```

安装后开始菜单 → 所有应用 → **Codex 网关** 文件夹里有 6 个快捷方式，**全部直接调用 powershell.exe + ps1**（不经过 bat，避免 .bat 关联被第三方软件劫持）：
**Codex 网关控制台**、**启动网关**、**停止网关**、**查看状态**、**配置向导**、**查看日志**（带自定义闪电图标，启动/停止/状态为最小化运行）。

移除快捷方式：

```powershell
powershell -ExecutionPolicy Bypass -File "安装到开始菜单.ps1" -Remove
```

## 如何获取 DeepSeek API Key

1. 打开 [platform.deepseek.com](https://platform.deepseek.com)，注册 / 登录
2. 进入「API Keys」页面，点击「创建 API Key」
3. 复制生成的 Key（以 `sk-` 开头），粘贴到 `setup` 向导中即可

> DeepSeek API 按量计费，新用户通常有免费额度，足够日常使用。

## License

MIT
