//! 进程管理：后台启动/停止代理、PID 文件。

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn write_pid(pid: u32) -> Result<()> {
    let p = crate::paths::pid_path()?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, pid.to_string()).context("写入 PID 文件失败")?;
    Ok(())
}

pub fn read_pid() -> Result<Option<u32>> {
    let p = crate::paths::pid_path()?;
    if !p.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&p)?;
    Ok(raw.trim().parse::<u32>().ok())
}

pub fn remove_pid() -> Result<()> {
    let p = crate::paths::pid_path()?;
    if p.exists() {
        fs::remove_file(&p)?;
    }
    Ok(())
}

pub fn is_running() -> Result<bool> {
    Ok(match read_pid()? {
        Some(pid) => pid_alive(pid),
        None => false,
    })
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // STILL_ACTIVE = 259
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
pub fn spawn_background(exe: &Path, log: &Path) -> Result<u32> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("打开日志 {} 失败", log.display()))?;
    let stdout = file.try_clone()?;
    let child = Command::new(exe)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(file)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("启动后台代理失败")?;
    Ok(child.id())
}

#[cfg(not(windows))]
pub fn spawn_background(exe: &Path, log: &Path) -> Result<u32> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let stdout = file.try_clone()?;
    let child = Command::new(exe)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(file)
        .spawn()?;
    Ok(child.id())
}

#[cfg(windows)]
pub fn kill_pid(pid: u32) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            // 进程已不存在
            return Ok(());
        }
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn kill_pid(pid: u32) -> Result<()> {
    let _ = Command::new("kill").arg(pid.to_string()).status()?;
    Ok(())
}

pub fn stop() -> Result<()> {
    // 1. 优先按 PID 文件杀（精确，不影响当前命令进程）
    if let Some(pid) = read_pid()? {
        if pid_alive(pid) {
            kill_pid(pid)?;
        }
        remove_pid()?;
    }
    // 2. 兜底：按进程名杀，排除当前进程（PID 文件丢失或残留进程时确保网关停掉）
    kill_by_name_excluding_current();
    Ok(())
}

/// 按进程名杀 codex-gateway，排除当前命令进程自身（避免 stop/uninstall 命令杀掉自己）。
#[cfg(windows)]
fn kill_by_name_excluding_current() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let current_pid = std::process::id();
    let script = format!(
        "Get-Process codex-gateway -ErrorAction SilentlyContinue | Where-Object {{ $_.Id -ne {current_pid} }} | Stop-Process -Force"
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(windows))]
fn kill_by_name_excluding_current() {
    let _ = Command::new("pkill")
        .args(["-f", "codex-gateway serve"])
        .status();
}

