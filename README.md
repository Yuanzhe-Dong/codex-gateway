[English](./README.en.md) | 中文

# codex-gateway

**让 Codex 同时拥有 GPT 的大脑和 DeepSeek 的性价比——同一个会话里点一下就切，上下文不丢、任务不断。**

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

## 如何获取 DeepSeek API Key

1. 打开 [platform.deepseek.com](https://platform.deepseek.com)，注册 / 登录
2. 进入「API Keys」页面，点击「创建 API Key」
3. 复制生成的 Key（以 `sk-` 开头），粘贴到 `setup` 向导中即可

> DeepSeek API 按量计费，新用户通常有免费额度，足够日常使用。

## License

MIT
