//! 从安装目录发现运行时，按程序报告的版本选择；同步时间戳不代表版本新旧。
use std::path::{Path, PathBuf};

fn local_binaries(bin: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(bin)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("codex.exe"))
        .filter(|path| path.is_file())
        .collect();
    if bin.join("codex.exe").is_file() {
        paths.push(bin.join("codex.exe"));
    }
    paths
}

#[cfg(any(windows, test))]
fn runtime_in_package(root: &Path) -> Option<PathBuf> {
    let path = root.join("app/resources/codex.exe");
    path.is_file().then_some(path)
}

#[cfg(windows)]
fn registered_store_binaries() -> Vec<PathBuf> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    // WindowsApps 可以读取已知文件却不能枚举目录；使用当前用户的安装注册信息。
    let root = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(packages) = root.open_subkey(r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages") else { return Vec::new(); };
    packages
        .enum_keys()
        .filter_map(Result::ok)
        .filter(|name| name.starts_with("OpenAI.Codex_"))
        .filter_map(|name| packages.open_subkey(name).ok())
        .filter_map(|key| key.get_value::<String, _>("PackageRootFolder").ok())
        .filter_map(|root| runtime_in_package(Path::new(&root)))
        .collect()
}

#[cfg(not(windows))]
fn registered_store_binaries() -> Vec<PathBuf> {
    Vec::new()
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
enum VersionPart {
    Number(u64),
    Text(String),
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct RuntimeVersion {
    core: Vec<u64>,
    stable: bool,
    prerelease: Vec<VersionPart>,
}

fn version_key(version: &str) -> Option<RuntimeVersion> {
    let (core, pre) = version
        .split_once('-')
        .map_or((version, None), |(core, pre)| (core, Some(pre)));
    let core: Vec<_> = core
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<_, _>>()
        .ok()?;
    if core.len() != 3 {
        return None;
    }
    let prerelease = pre
        .map(|s| {
            s.split('.')
                .map(|part| {
                    part.parse::<u64>()
                        .map(VersionPart::Number)
                        .unwrap_or_else(|_| VersionPart::Text(part.into()))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(RuntimeVersion {
        core,
        stable: pre.is_none(),
        prerelease,
    })
}

fn select_runtime(
    paths: Vec<PathBuf>,
    version: impl Fn(&Path) -> Option<String>,
) -> Option<PathBuf> {
    paths
        .into_iter()
        .filter_map(|path| Some((version_key(&version(&path)?)?, path)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, path)| path)
}

pub fn installed_binary() -> Option<PathBuf> {
    let mut paths = dirs::data_local_dir()
        .map(|dir| local_binaries(&dir.join("OpenAI/Codex/bin")))
        .unwrap_or_default();
    paths.extend(registered_store_binaries());
    select_runtime(paths, binary_version)
}

pub fn installed_version() -> Option<String> {
    binary_version(&installed_binary()?)
}

fn binary_version(binary: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let version = raw.trim().strip_prefix("codex-cli ")?;
    (!version.is_empty()
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)))
    .then(|| version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_both_installation_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir
            .path()
            .join("OpenAI.Codex_26.1106_x64/app/resources/codex.exe");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, "fixture").unwrap();
        assert_eq!(
            runtime_in_package(&dir.path().join("OpenAI.Codex_26.1106_x64")),
            Some(store)
        );
        let local = dir.path().join("runtime/codex.exe");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "fixture").unwrap();
        assert_eq!(local_binaries(dir.path()), vec![local]);
    }
    #[test]
    fn version_order_ignores_mtime_and_compares_prerelease_numerically() {
        let older = PathBuf::from("newly-synced-old-runtime");
        let newer = PathBuf::from("older-file-new-runtime");
        assert_eq!(
            select_runtime(vec![older.clone(), newer.clone()], |path| {
                Some(
                    if path == older {
                        "0.167.0-alpha.3.2"
                    } else {
                        "0.167.0-alpha.4.1"
                    }
                    .into(),
                )
            }),
            Some(newer)
        );
        assert!(version_key("0.167.0-alpha.10.1") > version_key("0.167.0-alpha.9.1"));
        assert!(version_key("0.167.0") > version_key("0.167.0-alpha.10.1"));
        assert!(version_key("0.168.0-alpha.1") > version_key("0.167.0"));
        assert!(version_key("not-a-version").is_none());
    }
}
