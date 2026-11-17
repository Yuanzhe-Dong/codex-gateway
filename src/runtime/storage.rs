//! 同目录临时文件 + 原子替换，避免配置在写入中断时被截断。

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_impl(path, bytes, false)
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_impl(path, bytes, true)
}

fn atomic_write_impl(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let parent = path.parent().context("目标文件没有父目录")?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    if private {
        restrict_permissions(tmp.path())?;
    } else if path.exists() {
        tmp.as_file()
            .set_permissions(std::fs::metadata(path)?.permissions())?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("原子替换 {} 失败", path.display()))?;
    Ok(())
}

/// 在写入任何密钥之前设置权限；失败必须中止，不能宣称已安全保存。
#[cfg(windows)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let system =
        std::path::PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot 未设置")?)
            .join("System32");
    let output = Command::new(system.join("whoami.exe"))
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(0x0800_0000)
        .output()?;
    anyhow::ensure!(output.status.success(), "获取当前 Windows 用户 SID 失败");
    let text = String::from_utf8_lossy(&output.stdout);
    let sid = text
        .trim()
        .rsplit(',')
        .next()
        .unwrap_or("")
        .trim_matches('"');
    anyhow::ensure!(sid.starts_with("S-1-"), "无法解析当前用户 SID");
    let output = Command::new(system.join("icacls.exe"))
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(F)")])
        .creation_flags(0x0800_0000)
        .output()?;
    anyhow::ensure!(output.status.success(), "限制密钥文件权限失败");
    Ok(())
}

#[cfg(not(windows))]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// 文件锁随句柄关闭自动释放，崩溃后不需要删除陈旧的锁文件。
pub fn lock(path: &Path) -> Result<File> {
    let parent = path.parent().context("锁文件没有父目录")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("无法创建锁目录：{}", parent.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "无法读写锁文件：{}；请检查当前终端对该目录的访问权限",
                path.display()
            )
        })?;
    file.try_lock()
        .with_context(|| format!("另一个网关操作正在运行：{}", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_complete_file_and_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        atomic_write(&file, b"original long value").unwrap();
        atomic_write(&file, b"new").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        let path = dir.path().join("control.lock");
        let guard = lock(&path).unwrap();
        assert!(lock(&path).is_err());
        drop(guard);
        assert!(lock(&path).is_ok());
    }
}
