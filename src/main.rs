//! codex-gateway：应用入口和进程退出码。
mod app;
mod history;
mod models;
mod proxy;
mod runtime;
mod settings;

fn permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

fn main() -> std::process::ExitCode {
    match app::cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            // 控制台仅对实际权限错误提供 UAC 重试，普通错误不请求提权。
            std::process::ExitCode::from(if permission_denied(&error) { 5 } else { 1 })
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn distinguishes_permission_errors_through_context() {
        let denied = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("查询网关失败");
        assert!(super::permission_denied(&denied));
        assert!(!super::permission_denied(&anyhow::anyhow!("普通错误")));
    }
}
