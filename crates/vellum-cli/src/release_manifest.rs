//! Strict package manifests and bounded metadata probes. Checksums are integrity,
//! not a claim that a publisher is trusted.
use super::paths::{BINARIES, RESOURCES, safe_id, safe_relative};
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_RESOURCE_BYTES: u64 = 4 * 1024 * 1024;
pub fn file_cap(relative: &str) -> u64 {
    if relative.starts_with("bin/") {
        MAX_FILE_BYTES
    } else {
        MAX_RESOURCE_BYTES
    }
}
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use vellum_core::build_info::BuildInfo;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub release_id: String,
    pub version: String,
    pub build: BuildInfo,
    pub files: Vec<FileRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
    pub format: String,
    pub release_id: String,
    pub build_id: String,
    pub generated: Vec<FileRecord>,
    pub files: Vec<FileRecord>,
    pub legacy: bool,
}
pub fn required_paths() -> BTreeSet<String> {
    BINARIES
        .iter()
        .map(|name| format!("bin/{name}"))
        .chain(RESOURCES.iter().map(|name| format!("resources/{name}")))
        .chain(["LICENSE", "README.md", "install.sh"].map(str::to_string))
        .collect()
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let file = regular(path)?;
    if file.metadata().map_err(|_| "无法读取清单属性")?.len() > 256 * 1024 {
        return Err("清单超过安全上限".into());
    }
    serde_json::from_reader(file.take(256 * 1024 + 1)).map_err(|_| "发布清单格式无效".into())
}
pub fn regular(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "发布文件不可读取或为符号链接")?;
    let metadata = file.metadata().map_err(|_| "无法检查发布文件")?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_FILE_BYTES {
        return Err("发布成员必须是独立普通文件".into());
    }
    Ok(file)
}
pub fn digest(path: &Path) -> Result<String, String> {
    let mut file = regular(path)?.take(MAX_FILE_BYTES + 1);
    let mut seen = 0u64;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let count = file.read(&mut bytes).map_err(|_| "无法校验发布文件")?;
        if count == 0 {
            break;
        }
        seen += count as u64;
        if seen > MAX_FILE_BYTES {
            return Err("发布文件读取期间超过上限".into());
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn record(root: &Path, relative: &str) -> Result<FileRecord, String> {
    if !safe_relative(relative) {
        return Err("发布文件路径无效".into());
    }
    let path = root.join(relative);
    let metadata = regular(&path)?
        .metadata()
        .map_err(|_| "无法读取发布文件属性")?;
    Ok(FileRecord {
        path: relative.into(),
        sha256: digest(&path)?,
        size: metadata.len(),
        executable: metadata.permissions().mode() & 0o111 != 0,
    })
}
pub fn verify_record(root: &Path, entry: &FileRecord) -> Result<(), String> {
    if !safe_relative(&entry.path)
        || entry.sha256.len() != 64
        || !entry
            .sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || entry.size > file_cap(&entry.path)
    {
        return Err("发布成员清单无效".into());
    }
    let metadata = regular(&root.join(&entry.path))?
        .metadata()
        .map_err(|_| "无法检查发布成员大小")?;
    if metadata.len() > file_cap(&entry.path)
        || metadata.len() != entry.size
        || (metadata.permissions().mode() & 0o111 != 0) != entry.executable
    {
        return Err("发布文件大小或权限与清单不一致".into());
    }
    let actual = record(root, &entry.path)?;
    if actual.size != entry.size
        || actual.sha256 != entry.sha256
        || actual.executable != entry.executable
    {
        return Err("发布文件大小、权限或SHA256不一致".into());
    }
    Ok(())
}
pub fn inventory(root: &Path) -> Result<BTreeSet<String>, String> {
    fn walk(
        root: &Path,
        directory: &Path,
        output: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 12 {
            return Err("发布目录过深".into());
        }
        let metadata = fs::symlink_metadata(directory).map_err(|_| "无法检查发布目录")?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("发布目录不能是链接".into());
        }
        for entry in fs::read_dir(directory).map_err(|_| "无法列出发布目录")? {
            let path = entry.map_err(|_| "无法读取发布成员")?.path();
            let meta = fs::symlink_metadata(&path).map_err(|_| "无法检查发布成员")?;
            if meta.file_type().is_symlink() {
                return Err("发布包不能含符号链接".into());
            }
            if meta.is_dir() {
                walk(root, &path, output, depth + 1)?;
            } else if meta.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| "发布路径越界")?
                    .to_str()
                    .ok_or("发布路径非UTF-8")?
                    .to_string();
                if !safe_relative(&relative) || !output.insert(relative) || output.len() > 128 {
                    return Err("发布文件数量或路径无效".into());
                }
            } else {
                return Err("发布包不能含特殊文件".into());
            }
        }
        Ok(())
    }
    let mut output = BTreeSet::new();
    walk(root, root, &mut output, 0)?;
    Ok(output)
}
impl Manifest {
    pub fn read(root: &Path) -> Result<Self, String> {
        let manifest: Self = read_json(&root.join("manifest.json"))?;
        if manifest.format != "vellum-release-v1"
            || !safe_id(&manifest.release_id)
            || manifest.release_id != manifest.build.build_id
            || manifest.version != manifest.build.version
            || manifest.build.format != "vellum-build-info-v1"
            || manifest.build.config_schema != 1
            || manifest.build.ipc_schema != 1
        {
            return Err("不支持的发布格式、版本或配置/IPC schema".into());
        }
        let paths: BTreeSet<_> = manifest
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        if paths != required_paths() || paths.len() != manifest.files.len() {
            return Err("发行包必须完整列出唯一的固定成员".into());
        }
        Ok(manifest)
    }
    pub fn verify(&self, root: &Path, probe: bool) -> Result<(), String> {
        let expected: BTreeSet<_> = self
            .files
            .iter()
            .map(|f| f.path.clone())
            .chain(["manifest.json".into()])
            .collect();
        if inventory(root)? != expected {
            return Err("发行包包含未知或遗漏文件".into());
        }
        let mut total = 0u64;
        for entry in &self.files {
            total = total.checked_add(entry.size).ok_or("发行包大小溢出")?;
            if total > 1024 * 1024 * 1024 {
                return Err("发行包过大".into());
            }
            verify_record(root, entry)?;
        }
        for name in BINARIES {
            let entry = self
                .files
                .iter()
                .find(|file| file.path == format!("bin/{name}"))
                .ok_or("缺少二进制")?;
            if !entry.executable {
                return Err("发行二进制不可执行".into());
            }
            if probe && probe_build_info(&root.join(&entry.path))? != self.build {
                return Err("四个二进制构建身份不一致".into());
            }
        }
        Ok(())
    }
}
pub fn has_build_info_marker(path: &Path) -> Result<bool, String> {
    const MARKER: &[u8] = b"vellum-build-info-v1";
    let mut file = regular(path)?.take(MAX_FILE_BYTES);
    let mut tail = Vec::new();
    let mut buffer = [0u8; 65536];
    let mut found = false;
    loop {
        let n = file.read(&mut buffer).map_err(|_| "无法扫描构建身份")?;
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buffer[..n]);
        if tail.windows(MARKER.len()).any(|window| window == MARKER) {
            found = true;
            break;
        }
        let keep = tail.len().saturating_sub(MARKER.len());
        tail.drain(..keep);
    }
    Ok(found)
}
pub fn probe_build_info(path: &Path) -> Result<BuildInfo, String> {
    if !has_build_info_marker(path)? {
        return Err("二进制没有安全构建信息入口，拒绝执行探测".into());
    }
    let mut command = vellum_core::proc::command(path);
    let mut child = command
        .arg("--build-info-json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "无法探测构建信息")?;
    let mut stdout = child.stdout.take().ok_or("构建探测没有输出管道")?;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Err("无法建立有界构建探测".into());
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut output = Vec::new();
    let result = loop {
        let mut buffer = [0u8; 4096];
        match stdout.read(&mut buffer) {
            Ok(n) => {
                output.extend_from_slice(&buffer[..n]);
                if output.len() > 65536 {
                    break Err("构建信息输出超限".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break Err("构建信息读取失败".into()),
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain only already available bytes; descendants cannot hold us open.
                loop {
                    match stdout.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            output.extend_from_slice(&buffer[..n]);
                            if output.len() > 65536 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                if !status.success() || output.len() > 65536 {
                    break Err("构建信息探测失败".into());
                }
                break serde_json::from_slice::<BuildInfo>(&output)
                    .map_err(|_| "构建信息JSON无效".into());
            }
            Ok(None) => {}
            Err(_) => break Err("无法等待构建探测".into()),
        }
        if Instant::now() >= deadline {
            break Err("构建信息探测超时".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Only this newly created process group is ever signalled.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}
pub fn version_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(inventory(root)?
        .into_iter()
        .map(|name| root.join(name))
        .collect())
}
