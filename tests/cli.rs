//! Windows 真实可执行文件回归：所有数据与进程均隔离，API Key 为虚构值，不请求外部上游。
#![cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

// 即使断言失败也停止由本测试直接创建的前台子进程，避免占用可执行文件。
struct ForegroundChild(std::process::Child);
impl Drop for ForegroundChild {
    fn drop(&mut self) {
        if self.0.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    codex: PathBuf,
    gateway: PathBuf,
    port: u16,
}

impl Fixture {
    fn new(original: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join("codex");
        let gateway = dir.path().join("gateway");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&gateway).unwrap();
        fs::write(codex.join("config.toml"), original).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = format!("deepseek_api_key = \"sk-test-do-not-use\"\nport = {port}\ndeepseek_models = [\"deepseek-v4-flash\", \"deepseek-v4-pro\"]\nauto_proxy = false\n");
        fs::write(gateway.join("config.toml"), cfg).unwrap();
        Self {
            _dir: dir,
            codex,
            gateway,
            port,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-gateway"));
        command
            .current_dir(self._dir.path())
            .env("CODEX_HOME", &self.codex)
            .env("CODEX_GATEWAY_HOME", &self.gateway)
            .env("RUST_LOG", "warn")
            .creation_flags(0x0800_0000);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.args(args);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(command.output());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("CLI 超时，可能存在输出管道继承或进程死锁")
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}: {} {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn config(&self) -> String {
        fs::read_to_string(self.codex.join("config.toml")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["stop"]);
    }
}

#[test]
fn model_update_failure_preserves_provider_and_catalog_and_start_keeps_snapshot() {
    let fixture = Fixture::new("model='gpt-original'\nmodel_provider='openai'\n");
    let catalog_path = fixture.codex.join("codex-gateway-model-catalog.json");
    fs::write(&catalog_path, "keep catalog on failure").unwrap();
    let before = fixture.config();
    // 未登录时不发送任何外网请求，也不激活 provider。
    let output = fixture.run(&["refresh-models"]);
    assert!(!output.status.success());
    assert_eq!(fixture.config(), before);
    assert_eq!(
        fs::read_to_string(&catalog_path).unwrap(),
        "keep catalog on failure"
    );
    let snapshot = fixture.codex.join("codex-gateway-official-models.json");
    fs::write(
        &snapshot,
        r#"{"models":[{"slug":"gpt-6.1-sol","visibility":"list","context_window":272000}]}"#,
    )
    .unwrap();
    fixture.ok(&["start"]);
    let catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    assert!(catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["slug"] == "gpt-6.1-sol"));
    fixture.ok(&["uninstall"]);
    assert!(!snapshot.exists());
}

#[test]
fn lifecycle_preserves_later_settings_history_and_foreign_sessions() {
    let fixture =
        Fixture::new("# original\nmodel = \"gpt-original\"\nmodel_provider = \"openai\"\n");
    fs::create_dir_all(fixture.codex.join("sessions")).unwrap();
    let history = "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"content\":[\"keep history\"]}}\n";
    let session = fixture.codex.join("sessions/test.jsonl");
    fs::write(&session, history).unwrap();
    let conn = rusqlite::Connection::open(fixture.codex.join("state_12.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads(id INTEGER, model_provider TEXT);
        INSERT INTO threads VALUES(1,'openai'),(2,'other-provider');",
    )
    .unwrap();
    fixture.ok(&["start"]);
    fixture.ok(&["start"]); // 重复启动不生成第二个进程。
    assert!(fixture.config().contains("model_provider = \"openai\""));
    assert!(fixture.config().contains("model = \"gpt-original\""));
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "openai"
    );
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads WHERE id=2", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "other-provider"
    );
    let mut config = fixture.config();
    config.push_str("\n[mcp_servers.later]\ncommand = \"keep-me\"\n");
    fs::write(fixture.codex.join("config.toml"), config).unwrap();
    let status = fixture.ok(&["status", "--offline"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("服务健康：正常"));
    fixture.ok(&["stop"]);
    assert!(fixture.config().contains("model = \"gpt-original\""));
    assert!(fixture.config().contains("keep-me"));
    assert_eq!(fs::read_to_string(&session).unwrap(), history);
    fixture.ok(&["start"]);
    fixture.ok(&["stop"]);
    fixture.ok(&["uninstall"]);
    assert!(fixture.config().contains("keep-me"));
    assert!(!fixture.gateway.join("config.toml").exists());
    assert_eq!(fs::read_to_string(session).unwrap(), history);
}

#[test]
fn occupied_port_fails_without_switching_config() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    let listener = TcpListener::bind(("127.0.0.1", fixture.port)).unwrap();
    let before = fixture.config();
    let output = fixture.run(&["start"]);
    assert!(!output.status.success());
    assert_eq!(fixture.config(), before);
    assert!(!fixture.gateway.join("gateway.pid").exists());
    drop(listener);
}

#[test]
fn stop_rejects_gateway_managed_backup_without_changing_config() {
    let fixture = Fixture::new("model='gpt-original'\nmodel_provider='openai'\n");
    fs::create_dir_all(fixture.gateway.join("backups")).unwrap();
    fs::write(
        fixture.gateway.join("backups/config.toml.fixture"),
        "model_provider='codex_gateway'\n",
    )
    .unwrap();
    fs::write(
        fixture.gateway.join("state.json"),
        r#"{"backup_id":"config.toml.fixture","managed_config":true}"#,
    )
    .unwrap();
    let current = "model_provider='codex_gateway'\n[model_providers.codex_gateway]\nname='codex-gateway'\nbase_url='http://127.0.0.1:17899/v1'\n";
    fs::write(fixture.codex.join("config.toml"), current).unwrap();
    let output = fixture.run(&["stop"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("原始备份内容已被网关接管"));
    assert_eq!(fixture.config(), current);
}

#[test]
fn stop_retains_native_alias_for_gateway_history_without_touching_foreign_provider() {
    let fixture = Fixture::new("model='gpt-6.1-sol'\nmodel_provider='openai'\n");
    // 构造接管后的配置，无需启动监听进程。
    fs::create_dir(fixture.gateway.join("backups")).unwrap();
    fs::write(
        fixture.gateway.join("backups/config.toml.fixture"),
        fixture.config(),
    )
    .unwrap();
    fs::write(
        fixture.gateway.join("state.json"),
        r#"{"backup_id":"config.toml.fixture","managed_config":true}"#,
    )
    .unwrap();
    fs::write(fixture.codex.join("config.toml"), format!(
        "model='gpt-6.1-sol'\nmodel_provider='codex_gateway'\nmodel_catalog_json='codex-gateway-model-catalog.json'\n[model_providers.codex_gateway]\nname='codex-gateway'\nbase_url='http://127.0.0.1:{}/v1'\nwire_api='responses'\nrequires_openai_auth=true\n[model_providers.foreign]\nbase_url='https://example.test/v1'\n", fixture.port)).unwrap();
    fixture.ok(&["stop"]);
    let doc: toml::Value = toml::from_str(&fixture.config()).unwrap();
    assert_eq!(doc["model_provider"].as_str(), Some("codex_gateway"));
    assert_eq!(
        doc["model_providers"]["codex_gateway"]["base_url"].as_str(),
        Some("https://chatgpt.com/backend-api/codex")
    );
    assert_eq!(
        doc["model_providers"]["foreign"]["base_url"].as_str(),
        Some("https://example.test/v1")
    );
    let once = fixture.config();
    fixture.ok(&["stop"]);
    assert_eq!(fixture.config(), once);
    // 卸载保留直连身份，但不能导致后续 setup/start 把它误判为未备份的接管配置。
    let settings = fs::read(fixture.gateway.join("config.toml")).unwrap();
    fixture.ok(&["uninstall"]);
    fs::write(fixture.gateway.join("config.toml"), settings).unwrap();
    fixture.ok(&["start"]);
    fixture.ok(&["stop"]);
    let reinstalled: toml::Value = toml::from_str(&fixture.config()).unwrap();
    assert_eq!(
        reinstalled["model_provider"].as_str(),
        Some("codex_gateway")
    );
}

#[test]
fn stop_recovers_exited_process_record_while_parent_keeps_handle() {
    let fixture = Fixture::new("model='gpt-original'\nmodel_provider='openai'\n");
    let history_path = fixture.codex.join("untouched.jsonl");
    let history = b"{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"content\":[\"keep DS history\"]}}\r\n";
    fs::write(&history_path, history).unwrap();
    // Child 保留退出进程的句柄，复现 GUI/终端仍持有旧网关句柄的场景。
    let mut child = fixture
        .command()
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(child.wait().unwrap().success());
    fs::write(
        fixture.gateway.join("gateway.pid"),
        serde_json::to_vec(&serde_json::json!({
            "pid": child.id(),
            "executable": env!("CARGO_BIN_EXE_codex-gateway"),
            "created": 0
        }))
        .unwrap(),
    )
    .unwrap();
    fixture.ok(&["stop"]);
    assert!(!fixture.gateway.join("gateway.pid").exists());
    assert_eq!(fs::read(&history_path).unwrap(), history);
    assert_eq!(
        fixture.config(),
        "model='gpt-original'\nmodel_provider='openai'\n"
    );
    drop(child);
}

#[test]
fn restart_replaces_process_and_preserves_settings_and_history() {
    let fixture = Fixture::new("model='gpt-original'\nmodel_provider='openai'\n");
    fs::create_dir_all(fixture.codex.join("sessions")).unwrap();
    let path = fixture.codex.join("sessions/restart.jsonl");
    let history = b"{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"content\":[\"DS history\"]}}\r\n";
    fs::write(&path, history).unwrap();
    let settings = fs::read(fixture.gateway.join("config.toml")).unwrap();
    fixture.ok(&["start"]);
    let previous: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.gateway.join("gateway.pid")).unwrap()).unwrap();
    let before_restart = fixture.config();
    let restarting = fixture.ok(&["restart"]);
    assert!(!String::from_utf8_lossy(&restarting.stdout).contains("恢复安装前"));
    assert_eq!(fixture.config(), before_restart);
    let current: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.gateway.join("gateway.pid")).unwrap()).unwrap();
    assert!(previous["pid"] != current["pid"] || previous["created"] != current["created"]);
    assert!(fixture.config().contains("codex_gateway"));
    let status = fixture.ok(&["status", "--offline"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("服务健康：正常"));
    assert_eq!(fs::read(&path).unwrap(), history);
    assert_eq!(
        fs::read(fixture.gateway.join("config.toml")).unwrap(),
        settings
    );
}

#[test]
fn malformed_configuration_returns_error_without_creating_backup_or_pid() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    for config in [
        "deepseek_api_key='sk-test'\nport=0",
        "deepseek_api_key='sk-test'\nport=17899\ndeepseek_models=[]",
        "deepseek_api_key=''\nport=17899",
        "deepseek_api_key='sk-test'\nport=17899\nproxy='file:///wrong'",
        "deepseek_api_key='sk-test'\nport=17899\ndeepseek_models=['unknown-model']",
    ] {
        fs::write(fixture.gateway.join("config.toml"), config).unwrap();
        let output = fixture.run(&["start"]);
        assert!(!output.status.success());
        assert!(!fixture.gateway.join("backups").exists());
        assert!(!fixture.gateway.join("gateway.pid").exists());
        assert_eq!(fixture.config(), "model = \"gpt-original\"\n");
    }
}

#[test]
fn stop_recovers_own_instance_when_pid_file_is_lost() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    fixture.ok(&["start"]);
    fs::remove_file(fixture.gateway.join("gateway.pid")).unwrap();
    fixture.ok(&["stop"]);
    let listener = TcpListener::bind(("127.0.0.1", fixture.port)).unwrap();
    assert!(fixture.config().contains("gpt-original"));
    drop(listener);
}

#[test]
fn stop_never_kills_recycled_pid_or_second_instance() {
    let first = Fixture::new("model = \"gpt-one\"\n");
    let second = Fixture::new("model = \"gpt-two\"\n");
    first.ok(&["start"]);
    second.ok(&["start"]);
    first.ok(&["stop"]);
    let status = second.ok(&["status", "--offline"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("服务健康：正常"));
    fs::write(
        first.gateway.join("gateway.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    first.ok(&["stop"]); // 当前测试进程的可执行路径不同，不能被网关终止。
    second.ok(&["stop"]);
}

#[test]
fn foreground_start_can_be_stopped_by_another_command() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    let mut child = ForegroundChild(
        fixture
            .command()
            .args(["start", "--foreground"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if fixture.gateway.join("gateway.pid").is_file() {
            let status = fixture.run(&["status", "--offline"]);
            if status.status.success()
                && String::from_utf8_lossy(&status.stdout).contains("服务健康：正常")
            {
                break;
            }
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            panic!("前台实例未启动");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    fixture.ok(&["stop"]);
    child.0.wait().unwrap();
    assert!(fixture.config().contains("gpt-original"));
}

#[test]
fn repair_preview_never_creates_control_locks_or_gateway_directory() {
    // 使用不存在的网关目录，验证预览确实只读，而非先创建控制锁。
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    let gateway = dir.path().join("gateway-not-created");
    fs::create_dir_all(codex.join("sessions")).unwrap();
    let path = codex.join("sessions/test.jsonl");
    let raw = "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"content\":[\"old\"]}}\r\n";
    fs::write(&path, raw).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codex-gateway"))
        .arg("repair-sessions")
        .env("CODEX_HOME", &codex)
        .env("CODEX_GATEWAY_HOME", &gateway)
        .creation_flags(0x0800_0000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 个文件，1 个推理项"));
    assert_eq!(fs::read_to_string(path).unwrap(), raw);
    assert!(!gateway.exists());
    assert!(!codex.join("codex-gateway.lock").exists());
}

#[test]
fn repair_apply_is_rejected_without_changing_history() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    fs::create_dir_all(fixture.codex.join("sessions")).unwrap();
    let path = fixture.codex.join("sessions/test.jsonl");
    let raw = "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"content\":[\"DS history\"]}}\r\n";
    fs::write(&path, raw).unwrap();
    let output = fixture.run(&["repair-sessions", "--apply"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("已停用会话历史改写"));
    assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
    assert!(!fixture.gateway.join("control.lock").exists());
    assert!(!fixture.gateway.join("backups").exists());
}

#[test]
fn check_session_verifies_both_copies_without_network_or_history_writes() {
    let fixture = Fixture::new("model = \"gpt-original\"\n");
    let path = fixture.codex.join("one-session.jsonl");
    let raw = concat!(
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"keep\"}}\r\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"uuid\",\"encrypted_content\":\"foreign\",\"content\":[{\"type\":\"reasoning_text\",\"text\":\"DS thought\"}]}}\r\n");
    fs::write(&path, raw).unwrap();
    let output = fixture.ok(&["check-session", path.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("官方请求副本过滤：1"));
    assert!(text.contains("DS 请求副本保留：1"));
    assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
    assert_eq!(fixture.config(), "model = \"gpt-original\"\n");
    assert!(!fixture.gateway.join("control.lock").exists());
}

#[test]
fn legacy_provider_migrates_without_changing_original_backup() {
    let fixture = Fixture::new("model_provider='custom'\nmodel='deepseek-v4-flash'\nmodel_catalog_json='codex-gateway-model-catalog.json'\n[model_providers.custom]\nname='deepseek'\nbase_url='http://127.0.0.1:17899/v1'\n");
    fs::create_dir_all(fixture.gateway.join("backups")).unwrap();
    let original = "model='gpt-original'\nmodel_provider='openai'\n";
    fs::write(fixture.gateway.join("backups/config.toml.legacy"), original).unwrap();
    fs::write(
        fixture.gateway.join("state.json"),
        r#"{"backup_id":"config.toml.legacy","managed_config":true}"#,
    )
    .unwrap();
    fixture.ok(&["start"]);
    assert!(fixture.config().contains("codex_gateway"));
    assert!(!fixture.config().contains("[model_providers.custom]"));
    assert_eq!(
        fs::read_to_string(fixture.gateway.join("backups/config.toml.legacy")).unwrap(),
        original
    );
    fixture.ok(&["stop"]);
    assert!(fixture.config().contains("gpt-original"));
}

#[test]
fn stop_restores_original_custom_provider_and_preserves_its_table() {
    let original = "model='private-model'\nmodel_provider='custom'\nmodel_catalog_json='mine.json'\n[model_providers.custom]\nbase_url='https://example.test/v1'\n";
    let fixture = Fixture::new(original);
    let conn = rusqlite::Connection::open(fixture.codex.join("state_12.sqlite")).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads(id INTEGER, model_provider TEXT);
        INSERT INTO threads VALUES(1,'custom'),(2,'openai'),(3,'other-provider');",
    )
    .unwrap();
    fixture.ok(&["start"]);
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "custom"
    );
    assert_eq!(
        conn.query_row("SELECT model_provider FROM threads WHERE id=2", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "openai"
    );
    assert!(fixture.config().contains("https://example.test/v1"));
    fixture.ok(&["stop"]);
    assert!(
        fixture.config().contains("model_provider='custom'")
            || fixture.config().contains("model_provider = 'custom'")
    );
    assert!(fixture.config().contains("private-model"));
    assert!(fixture.config().contains("mine.json"));
    assert!(fixture.config().contains("https://example.test/v1"));
}

#[test]
fn data_directory_cannot_be_reused_with_another_codex_home() {
    let fixture = Fixture::new("model='gpt-original'\n");
    fixture.ok(&["start"]);
    fixture.ok(&["stop"]);
    let another = fixture._dir.path().join("another");
    fs::create_dir_all(&another).unwrap();
    fs::write(another.join("config.toml"), "model='do-not-touch'\n").unwrap();
    let output = fixture
        .command()
        .env("CODEX_HOME", &another)
        .arg("start")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(another.join("config.toml")).unwrap(),
        "model='do-not-touch'\n"
    );
}

#[test]
fn powershell_output_capture_returns_while_background_daemon_keeps_running() {
    let fixture = Fixture::new("model='gpt-original'\n");
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoProfile",
            "-Command",
            "& $env:GATEWAY_TEST_EXE start | Out-Host; exit $LASTEXITCODE",
        ])
        .env("GATEWAY_TEST_EXE", env!("CARGO_BIN_EXE_codex-gateway"))
        .env("CODEX_HOME", &fixture.codex)
        .env("CODEX_GATEWAY_HOME", &fixture.gateway)
        .env("RUST_LOG", "warn")
        .creation_flags(0x0800_0000);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(command.output());
    });
    let output = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("PowerShell 输出仍被后台进程持有")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = fixture.ok(&["status", "--offline"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("服务健康：正常"));
    fixture.ok(&["stop"]);
}
