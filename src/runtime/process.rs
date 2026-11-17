//! 管理本实例的进程记录，核对可执行路径和创建时间，禁止按名称批量终止。
use crate::runtime::paths;
use crate::runtime::storage;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Serialize, Deserialize)]
struct ProcessRecord {
    pid: u32,
    executable: PathBuf,
    created: u64,
    #[serde(skip)]
    legacy: bool,
}

fn read_record() -> Result<Option<ProcessRecord>> {
    let path = paths::pid_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("读取进程记录失败"),
    };
    if let Ok(record) = serde_json::from_str::<ProcessRecord>(&raw) {
        return Ok(Some(record));
    }
    // 兼容 0.1 的纯 PID 文件，以写入时间约束创建时间，防止陈旧 PID 被其它进程复用。
    if let Ok(pid) = raw.trim().parse::<u32>() {
        let modified = fs::metadata(path)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        return Ok(Some(ProcessRecord {
            pid,
            executable: std::env::current_exe()?,
            created: (modified + 11_644_473_600) * 10_000_000,
            legacy: true,
        }));
    }
    bail!("进程记录损坏，停止自动终止进程；请检查 gateway.pid");
}

pub fn write_pid(pid: u32) -> Result<()> {
    let record = ProcessRecord {
        pid,
        executable: std::env::current_exe()?,
        created: creation_time(pid)?,
        legacy: false,
    };
    storage::atomic_write(&paths::pid_path()?, &serde_json::to_vec(&record)?)
}

pub fn read_pid() -> Result<Option<u32>> {
    Ok(match read_record()? {
        Some(record) => Some(record.pid),
        None => find_instance()?.map(|r| r.pid),
    })
}

pub fn remove_pid() -> Result<()> {
    match fs::remove_file(paths::pid_path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("删除 PID 文件失败"),
    }
}

pub fn is_running() -> Result<bool> {
    match read_record()? {
        Some(record) => record_matches(&record),
        None => Ok(find_instance()?.is_some()),
    }
}

#[cfg(windows)]
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn open_process(pid: u32, terminate: bool) -> Result<Option<ProcessHandle>> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    let rights = PROCESS_QUERY_LIMITED_INFORMATION
        | PROCESS_SYNCHRONIZE
        | if terminate { PROCESS_TERMINATE } else { 0 };
    let handle = unsafe { OpenProcess(rights, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        // ERROR_INVALID_PARAMETER：PID 已退出；AccessDenied 不能伪装成已停止。
        if error.raw_os_error() == Some(87) {
            return Ok(None);
        }
        return Err(error).with_context(|| {
            format!("打开网关进程失败（PID {pid}）；当前终端可能受沙箱或进程访问权限限制")
        });
    }
    Ok(Some(ProcessHandle(handle)))
}

#[cfg(windows)]
fn process_exited(handle: &ProcessHandle) -> Result<bool> {
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };
    // 使用进程对象的信号状态；退出码 259 本身也可能是合法的最终退出码。
    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(std::io::Error::last_os_error()).context("检查网关进程退出状态失败"),
    }
}

#[cfg(windows)]
fn handle_identity(handle: &ProcessHandle) -> Result<Option<(PathBuf, u64)>> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetProcessTimes, QueryFullProcessImageNameW},
    };
    // 已退出但仍有句柄的进程，查询映像路径会报 ERROR_GEN_FAILURE (31)。
    // 先识别退出状态，查询过程中恰好退出时也返回 None。
    if process_exited(handle)? {
        return Ok(None);
    }
    let mut path = vec![0u16; 32768];
    let mut size = path.len() as u32;
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = created;
    let mut kernel = created;
    let mut user = created;
    unsafe {
        if QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut size) == 0
            || GetProcessTimes(handle.0, &mut created, &mut exit, &mut kernel, &mut user) == 0
        {
            let error = std::io::Error::last_os_error();
            if process_exited(handle)? {
                return Ok(None);
            }
            return Err(error).context("核对进程身份失败");
        }
    }
    if process_exited(handle)? {
        return Ok(None);
    }
    let executable = PathBuf::from(String::from_utf16_lossy(&path[..size as usize]));
    let timestamp = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Ok(Some((executable, timestamp)))
}

#[cfg(windows)]
fn creation_time(pid: u32) -> Result<u64> {
    let handle = open_process(pid, false)?.context("网关进程已退出")?;
    Ok(handle_identity(&handle)?.context("网关进程已退出")?.1)
}

fn same_identity(record: &ProcessRecord, executable: &Path, created: u64) -> bool {
    let path_matches = fs::canonicalize(&record.executable)
        .ok()
        .zip(fs::canonicalize(executable).ok())
        .is_some_and(|(a, b)| a == b);
    // 新 JSON 记录精确比较；旧版时间按秒取整且父进程稍后写 PID，最多容差 10 秒。
    path_matches
        && (record.created == created
            || (record.legacy && record.created.abs_diff(created) <= 100_000_000))
}

#[cfg(windows)]
fn record_matches(record: &ProcessRecord) -> Result<bool> {
    let handle = match open_process(record.pid, false) {
        Ok(handle) => handle,
        Err(error) if crate::permission_denied(&error) => {
            // 不能把 AccessDenied 当作进程已退出。只有实例锁、配置端口均空闲，
            // 才能忽略旧记录；不尝试终止无法核实身份的 PID。
            if let Ok(_idle) = idle_instance_guard() {
                tracing::warn!(
                    pid = record.pid,
                    "旧 PID 无法访问，但实例锁和端口均空闲，忽略过期记录"
                );
                return Ok(false);
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let Some(handle) = handle else {
        return Ok(false);
    };
    Ok(handle_identity(&handle)?
        .is_some_and(|(path, created)| same_identity(record, &path, created)))
}

#[cfg(windows)]
fn idle_instance_guard() -> Result<(fs::File, std::net::TcpListener)> {
    let cfg = crate::settings::gateway::GatewayConfig::load_for_setup()?
        .context("缺少网关配置，无法核实旧进程记录")?;
    idle_instance_guard_at(&paths::gateway_dir()?.join("instance.lock"), cfg.port)
}

#[cfg(windows)]
fn idle_instance_guard_at(
    lock_path: &Path,
    port: u16,
) -> Result<(fs::File, std::net::TcpListener)> {
    // 只打开已有锁；缺失或无法获取锁都不能证明旧实例已停止。
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)?;
    file.try_lock().context("网关实例锁仍被持有")?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .context("网关端口无法独占，保留旧进程记录")?;
    Ok((file, listener))
}

#[cfg(not(windows))]
fn creation_time(_pid: u32) -> Result<u64> {
    Ok(0)
}
#[cfg(not(windows))]
fn record_matches(_record: &ProcessRecord) -> Result<bool> {
    Ok(false)
}

/// 创建的进程句柄由父进程持有，启动失败时能精确停止自己的子进程。
pub struct BackgroundChild {
    pid: u32,
    #[cfg(windows)]
    handle: ProcessHandle,
}

impl BackgroundChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    #[cfg(windows)]
    pub fn try_wait(&self) -> Result<Option<u32>> {
        let mut code = 0;
        unsafe {
            if windows_sys::Win32::System::Threading::GetExitCodeProcess(self.handle.0, &mut code)
                == 0
            {
                return Err(std::io::Error::last_os_error()).context("查询子进程失败");
            }
        }
        Ok((code != 259).then_some(code))
    }

    #[cfg(windows)]
    pub fn kill(&self) -> Result<()> {
        unsafe {
            if windows_sys::Win32::System::Threading::TerminateProcess(self.handle.0, 1) == 0 {
                return Err(std::io::Error::last_os_error()).context("停止启动失败的子进程失败");
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn wait(&self) -> Result<()> {
        unsafe {
            anyhow::ensure!(
                windows_sys::Win32::System::Threading::WaitForSingleObject(self.handle.0, 5000)
                    == 0,
                "等待子进程退出超时"
            );
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn try_wait(&self) -> Result<Option<u32>> {
        bail!("此平台不支持后台进程管理");
    }
    #[cfg(not(windows))]
    pub fn kill(&self) -> Result<()> {
        bail!("此平台不支持后台进程管理");
    }
    #[cfg(not(windows))]
    pub fn wait(&self) -> Result<()> {
        bail!("此平台不支持后台进程管理");
    }
}

pub fn spawn_background(exe: &Path, log: &Path) -> Result<BackgroundChild> {
    #[cfg(not(windows))]
    bail!("后台进程管理目前仅支持 Windows；其它系统请使用 start --foreground");
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                CreateProcessW, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
            },
        };
        fs::create_dir_all(log.parent().context("日志目录不存在")?)?;
        if fs::metadata(log).is_ok_and(|m| m.len() > 10 * 1024 * 1024) {
            let rotated = log.with_file_name("gateway.log.1");
            if rotated.exists() {
                fs::remove_file(&rotated)?;
            }
            fs::rename(log, rotated)?;
        }
        let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut command = std::ffi::OsString::from("\"");
        command.push(exe);
        command.push("\" serve");
        let mut command: Vec<u16> = command.encode_wide().chain(Some(0)).collect();
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // 完全禁止句柄继承：PowerShell/嵌套管道还可能有非标准流句柄，逐个清除不可靠。
        // 日志由 serve 进程自己打开，父进程只传入固定命令，不通过 shell 执行。
        unsafe {
            if CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NO_WINDOW,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut info,
            ) == 0
            {
                return Err(std::io::Error::last_os_error()).context("启动后台代理失败");
            }
            CloseHandle(info.hThread);
        }
        Ok(BackgroundChild {
            pid: info.dwProcessId,
            handle: ProcessHandle(info.hProcess),
        })
    }
}

pub struct HealthInfo {
    pub pid: u32,
    pub version: String,
}

pub async fn health(port: u16) -> Result<Option<u32>> {
    Ok(health_info(port).await?.map(|info| info.pid))
}

pub async fn health_info(port: u16) -> Result<Option<HealthInfo>> {
    let response = crate::runtime::network::build_client(None)?
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .timeout(Duration::from_millis(500))
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let value: serde_json::Value = response.json().await?;
    if value["ok"] != true
        || value["service"] != "codex-gateway"
        || value["instance"] != paths::instance_name()?
    {
        return Ok(None);
    }
    Ok(value["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .map(|pid| HealthInfo {
            pid,
            version: value["version"].as_str().unwrap_or("unknown").to_owned(),
        }))
}

pub fn wait_ready(child: &mut BackgroundChild, port: u16) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("代理启动失败（{status}），请查看 gateway.log");
            }
            if health(port).await.ok().flatten() == Some(child.id()) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                remove_pid()?;
                bail!("代理在 5 秒内未通过健康检查，已停止；请查看 gateway.log");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

pub fn stop() -> Result<()> {
    let record = match read_record()? {
        Some(record) => Some(record),
        None => find_instance()?,
    };
    let Some(record) = record else {
        return Ok(());
    };
    anyhow::ensure!(record.pid != std::process::id(), "拒绝停止当前控制进程");
    stop_record(&record)?;
    remove_pid()
}

/// PID 文件丢失时，仅接管健康响应属于此数据目录、且可执行路径匹配的实例。
#[cfg(windows)]
fn find_instance() -> Result<Option<ProcessRecord>> {
    let Some(cfg) = crate::settings::gateway::GatewayConfig::load_for_setup()? else {
        return Ok(None);
    };
    let pid = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(health(cfg.port))
        .ok()
        .flatten();
    let Some(pid) = pid else {
        return Ok(None);
    };
    let Some(handle) = open_process(pid, false)? else {
        return Ok(None);
    };
    let Some((path, created)) = handle_identity(&handle)? else {
        return Ok(None);
    };
    let record = ProcessRecord {
        pid,
        executable: std::env::current_exe()?,
        created,
        legacy: false,
    };
    Ok(same_identity(&record, &path, created).then_some(record))
}

#[cfg(not(windows))]
fn find_instance() -> Result<Option<ProcessRecord>> {
    Ok(None)
}

#[cfg(windows)]
fn stop_record(record: &ProcessRecord) -> Result<()> {
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    // 先以查询权限核实身份，避免对已复用 PID 的其它程序请求终止权限。
    if !record_matches(record)? {
        return Ok(());
    }
    let Some(handle) = open_process(record.pid, true)? else {
        return Ok(());
    };
    // 换句柄后再次核对，防止两次 OpenProcess 之间发生 PID 复用。
    let Some((path, created)) = handle_identity(&handle)? else {
        return Ok(());
    };
    if !same_identity(record, &path, created) {
        tracing::warn!("陈旧 PID 对应其它进程，未执行终止：{}", record.pid);
        return Ok(());
    }
    unsafe {
        if TerminateProcess(handle.0, 0) == 0 {
            let error = std::io::Error::last_os_error();
            if process_exited(&handle)? {
                return Ok(());
            }
            return Err(error).context("停止网关失败");
        }
        anyhow::ensure!(WaitForSingleObject(handle.0, 5000) == 0, "停止网关超时");
    }
    Ok(())
}

#[cfg(not(windows))]
fn stop_record(_record: &ProcessRecord) -> Result<()> {
    bail!("此平台不支持后台进程管理");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn process_exit_fixture() {
        if let Ok(code) = std::env::var("GATEWAY_TEST_PROCESS_EXIT") {
            std::process::exit(code.parse().unwrap());
        }
    }

    #[test]
    #[cfg(windows)]
    fn terminated_process_with_open_handle_is_not_running() {
        use std::os::windows::process::CommandExt;
        let command = std::env::current_exe().unwrap();
        // 保留 Child 和查询句柄，让已退出的进程对象继续存在；259 也不能被误判为运行中。
        let mut child = std::process::Command::new(&command)
            .args(["--exact", "runtime::process::tests::process_exit_fixture"])
            .env("GATEWAY_TEST_PROCESS_EXIT", "259")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let handle = open_process(child.id(), false).unwrap().unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        assert!(
            handle_identity(&handle).unwrap().is_none(),
            "保留句柄的已退出进程不能被判为运行中"
        );
        let record = ProcessRecord {
            pid: child.id(),
            executable: command,
            created: 0,
            legacy: false,
        };
        assert!(!record_matches(&record).unwrap());
        stop_record(&record).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn idle_proof_requires_existing_unlocked_instance_file_and_free_port() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("instance.lock");
        assert!(idle_instance_guard_at(&lock_path, 0).is_err());
        fs::write(&lock_path, b"").unwrap();
        let busy_lock = crate::runtime::storage::lock(&lock_path).unwrap();
        assert!(idle_instance_guard_at(&lock_path, 0).is_err());
        drop(busy_lock);
        let busy_port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy_port.local_addr().unwrap().port();
        assert!(idle_instance_guard_at(&lock_path, port).is_err());
        drop(busy_port);
        let idle = idle_instance_guard_at(&lock_path, port).unwrap();
        assert!(crate::runtime::storage::lock(&lock_path).is_err());
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        drop(idle);
        assert!(idle_instance_guard_at(&lock_path, port).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn stop_does_not_terminate_reused_pid() {
        let record = ProcessRecord {
            pid: std::process::id(),
            executable: std::env::current_exe().unwrap(),
            created: 0,
            legacy: false,
        };
        stop_record(&record).unwrap();
    }

    #[test]
    fn rejects_recycled_pid_and_different_executable() {
        let exe = std::env::current_exe().unwrap();
        let record = ProcessRecord {
            pid: 10,
            executable: exe.clone(),
            created: 123456789,
            legacy: false,
        };
        assert!(same_identity(&record, &exe, 123456789));
        assert!(!same_identity(&record, &exe, 223456789));
        assert!(!same_identity(
            &record,
            Path::new("unrelated.exe"),
            123456789
        ));
    }

    #[tokio::test]
    async fn does_not_mistake_any_http_service_for_gateway() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().fallback(|| async {
            axum::Json(serde_json::json!({"ok":true,"service":"other","pid":1}))
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert_eq!(health(port).await.unwrap(), None);
        server.abort();
    }
}
