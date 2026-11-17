English | [中文](./README.md)

# codex-gateway

A Windows loopback gateway that routes Codex Responses requests to either the ChatGPT subscription backend or DeepSeek, using the requested model. Official requests keep Codex OAuth authentication; DeepSeek requests use your local DeepSeek API key.

Switch between official and DeepSeek models in the same conversation. The gateway adapts outgoing requests automatically and preserves original conversation history.

## Build and run

Download the Windows ZIP from [Releases](https://github.com/Yuanzhe-Dong/codex-gateway/releases/latest), extract all files, and double-click `codex-gateway 控制台.bat`. Choose **4 Configure**, then **1 Start**, and fully restart Codex. The gateway runs in the background after closing the console.

The console contains six everyday operations: start, stop, status, configure, restart and uninstall. After replacing the executable or changing network settings, choose **5 Restart**. A Windows permission prompt appears only when the requested control operation is denied. Stop and uninstall preserve Codex conversations.

Rust 1.89 or later and Windows C++ build tools are required to build from source:

```powershell
cargo build --release --locked
.\target\release\codex-gateway.exe setup
.\target\release\codex-gateway.exe start
```

Portable releases place `codex-gateway.exe` next to the console scripts and `assets/`. Run the executable directly or double-click the `.bat` launcher.

`setup` saves settings, creates a private backup, and generates the model catalog. `start` activates Codex configuration only after the service passes a health check. Restart Codex after initially activating or restoring the provider. While the gateway runs, model selection within a conversation usually needs no task restart.

| Command | Purpose |
| --- | --- |
| `setup` | Configure key, models, port and network mode; stop first before reconfiguration |
| `start` | Run in the background; failures return nonzero |
| `start --foreground` | Run in foreground; Ctrl+C restores configuration |
| `stop` | Stop this instance and restore direct routing, retaining official/gateway identity |
| `restart` | Stop the old instance and start the current executable, preserving settings and history |
| `status` / `status --offline` | Inspect local state, optionally probing upstream HTTP reachability |
| `uninstall` | Remove gateway credentials, logs, catalog and original-config backup |

Catalog refresh, API probes, logs and read-only session checks are documented in [Diagnostics and maintenance](./docs/DIAGNOSTICS.md), outside the everyday console.

Stop preserves gateway settings. Uninstall preserves conversation history, session-repair backups, and empty coordination lock files.

## Models and compatibility

Default models are `deepseek-flash` and `deepseek-v4-pro`. The [V4.1-Flash-1017 release](https://api-docs.deepseek.com/news/news261017/) on 2026-10-17 keeps the API name `deepseek-flash`; do not append a revision date. Retired `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` names normalize to `deepseek-flash` in settings, catalogs, and requests. Effort levels are `none`, `low`, `high`, and `xhigh`; `xhigh` and incoming `ultra` map to DeepSeek `max`, `minimal` to `low`, and `medium` to `high`. Official-only `service_tier` settings are removed for DeepSeek and preserved for official requests. Flash supports images; Pro remains text-only. Checked on 2026-10-17 against the [Responses API documentation](https://api-docs.deepseek.com/api/create-response/).

Run `refresh-models` to fetch the official catalog for your logged-in ChatGPT account. Version 0.4.0 preserves account-advertised Max / Ultra efforts and agent-message content; use a current Codex client (validated against 0.167.0-alpha.4.1). It preserves server-provided capabilities and writes a separate `CODEX_HOME/codex-gateway-official-models.json`, leaving native settings, authentication, and cache untouched. Setup/start merge that snapshot with native `models_cache.json`, keep newer metadata for duplicate slugs, and preserve frontier models present in either source even when the newer cache contains only a narrower recommended set. Failed refreshes preserve the existing catalog. Refresh also works before configuring a DeepSeek key; expired OAuth requires signing in again through Codex.

Known 6.x choices are supplemented even when the cache contains only older models. Same-slug account metadata, including hidden visibility, takes precedence. A limited fallback includes GPT-6.1 Sol, GPT-6.1 Terra, GPT-6 Astra, GPT-6 Sol, and GPT-6 Luna, checked against the [official OpenAI model documentation](https://developers.openai.com/api/docs/models) on 2026-10-17. Fallback entries do not grant subscription access; actual Codex limits and availability come from the account catalog.

This gateway controls the local backend. Remotely hosted Desktop conversations use independent configuration and model discovery, so this gateway cannot add DeepSeek to those menus.

Both upstreams use Responses and streamed SSE, with request compatibility adjustments. DeepSeek lacks server-side compact and some official tools; see its [compatibility documentation](https://api-docs.deepseek.com/guides/responses_api/). The ChatGPT backend, model cache and SQLite schema remain external compatibility dependencies. Real account integration must be checked after client updates.

## Data and recovery

Gateway data uses `CODEX_GATEWAY_HOME`, defaulting to `~/.codex-gateway`. Codex data uses `CODEX_HOME`, defaulting to `~/.codex`. A gateway data directory is bound to its original Codex directory. Use separate data directories for independent instances; use one active gateway configuration per Codex directory.

Native users retain `openai`; existing gateway users retain `codex_gateway`. Activation changes routing, not history identity. Third-party configurations still restore their original provider on stop. A direct official alias remains for legacy gateway histories. Restoration merges owned fields and preserves later settings. Credential files and backups remain private plaintext files.

Version 0.4.0 removes all SQLite writes. JSONL histories are read-only. Official requests adapt known incompatible fields in memory; agent messages and unknown protocol items pass through unchanged. A current client requires matching provider identities when resuming a conversation; mixed legacy identities cannot be transparently merged by an HTTP gateway. See [diagnostics](./docs/DIAGNOSTICS.md), [architecture](./docs/ARCHITECTURE.md), and [validation](./docs/REFACTOR-0.4.0.md).

Configuration, catalog, state and PID writes replace same-directory temporary files. Startup failure preserves native configuration; termination failures prevent data purging.

## Network and access

Manual proxy overrides automatic detection. Automatic mode reads environment and Windows WinINET settings at startup, including per-protocol proxy lists; restart after changing those settings. Forced direct mode disables implicit proxies. Manual proxy failure never bypasses the proxy. Only automatic-proxy connection failures may retry directly; response timeouts and HTTP errors are not retried.

The service listens on IPv4 loopback, rejects browser-origin requests and non-loopback hosts, and requires Bearer headers for API requests. DeepSeek receives an allowlist of headers, excluding official account identity. Redirects are not followed. These controls do not isolate other processes running as the same Windows user or validate OAuth token authenticity.

Supported endpoints are `/responses`, official `/responses/compact`, `/models`, and `/healthz`, with optional `/v1` prefix. Limits: 64 MiB request body, eight active requests, 30 seconds to read client bodies, 10 seconds to connect, and 10 minutes without upstream data. Active SSE streams have no fixed total timeout. Logs rotate at startup if larger than 10 MiB; there is no live rotation. `RUST_LOG=debug` enables structural diagnostics without input text or credentials.

## Console and validation

The PowerShell console supports portable packages and source builds, honors custom data directories and preserves command exit codes. Start-menu shortcuts provide the console, start, stop and restart. Reinstalling shortcuts removes obsolete diagnostic entries while retaining user-created files. `.ps1` scripts use UTF-8 BOM for Windows PowerShell 5.1; the `.bat` launcher is ASCII.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "安装到开始菜单.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File "安装到开始菜单.ps1" -Remove
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
powershell -NoProfile -ExecutionPolicy Bypass -File tests/console.ps1
```

Tests use local upstreams, temporary data and fake keys, with no billed model requests. See the [review record](./docs/REFACTOR-0.4.0.md) for findings and limitations.

MIT licensed.
