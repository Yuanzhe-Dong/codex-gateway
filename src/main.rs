//! codex-gateway：Codex 无缝模型切换网关（Rust 单文件）。

mod catalog;
mod cli;
mod codex_config;
mod config;
mod network;
mod paths;
mod process;
mod proxy;
mod session_sync;
mod status;
mod upstream;
mod wizard;

fn main() -> anyhow::Result<()> {
    cli::run()
}