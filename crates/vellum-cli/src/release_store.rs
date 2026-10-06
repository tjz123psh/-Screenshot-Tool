//! A single writer, durable intent journal, immutable release directories, and
//! one current symlink. No version directories or user settings are auto-deleted.
use super::manifest::{self, FileRecord, Installed, Manifest};
use super::paths::{BINARIES, RESOURCES, check_chain, safe_id};
use super::{Hooks, Operation, Paths, Report, ServiceSnapshot, Services};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE: &str = "state.json";
const JOURNAL: &str = "journal.json";
const UNITS: [&str; 3] = [
    "vellum.service",
    "vellum-tray.service",
    "vellum-shortcuts.service",
];
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format: String,
    paths: Paths,
    current: Option<String>,
    previous: Option<String>,
    candidate: Option<String>,
    status: String,
}
impl State {
    fn empty(paths: &Paths) -> Self {
        Self {
            format: "vellum-release-state-v1".into(),
            paths: paths.clone(),
            current: None,
            previous: None,
            candidate: None,
            status: "not-installed".into(),
        }
    }
    fn report(&self, message: &str) -> Report {
        Report {
            success: true,
            state: self.status.clone(),
            current: self.current.clone(),
            previous: self.previous.clone(),
            candidate: self.candidate.clone(),
            message: message.into(),
            preserved: Vec::new(),
            repair_program: None,
            repair_args: Vec::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: String,
    paths: Paths,
    operation: String,
    target: Option<String>,
    old: State,
    services: ServiceSnapshot,
    phase: String,
    stage: Option<String>,
    adoption: Option<String>,
    #[serde(default)]
    cleanup: Vec<CleanupPlan>,
    #[serde(default)]
    cleanup_prepared: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupPlan {
    id: String,
    files: Vec<FileRecord>,
}
fn nonce() -> String {
    format!(
        "{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| "无法同步发布目录".into())
}
fn directory(path: &Path) -> Result<(), String> {
    check_chain(path)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| "无法创建发布目录")?;
    let meta = fs::symlink_metadata(path).map_err(|_| "无法检查发布目录")?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err("发布目录不属于当前用户".into());
    }
    Ok(())
}
struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn lock(paths: &Paths, create: bool) -> Result<Lock, String> {
    if create {
        directory(&paths.root)?;
    }
    let meta = fs::symlink_metadata(&paths.root).map_err(|_| "无法检查安装根")?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("安装根必须为用户私有目录（0700）".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.root.join(".lock"))
        .map_err(|_| "无法打开发布管理锁")?;
    let meta = file.metadata().map_err(|_| "无法检查发布锁")?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } {
        return Err("发布管理锁归属无效".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("另一个发布操作正在进行，请稍后重试".into());
    }
    Ok(Lock(file))
}
fn write_new(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    directory(path.parent().ok_or("缺少发布父目录")?)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if executable { 0o755 } else { 0o600 })
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "无法独占创建发布文件")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "发布写入或同步失败")?;
    sync_dir(path.parent().unwrap())
}
fn atomic_json<T: Serialize>(root: &Path, name: &str, value: &T) -> Result<(), String> {
    let destination = root.join(name);
    if let Ok(metadata) = fs::symlink_metadata(&destination)
        && (!metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1)
    {
        return Err("发布状态文件被替换，拒绝覆盖".into());
    }
    let temporary = root.join(format!(".{name}-{}", nonce()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| "无法编码发布状态")?;
    write_new(&temporary, &bytes, false)?;
    fs::rename(&temporary, &destination).map_err(|_| "无法提交发布状态")?;
    sync_dir(root)
}
fn remove_journal(paths: &Paths) -> Result<(), String> {
    manifest::regular(&paths.root.join(JOURNAL))?;
    fs::remove_file(paths.root.join(JOURNAL)).map_err(|_| "无法完成发布日志清理")?;
    sync_dir(&paths.root)
}
fn checkpoint(
    paths: &Paths,
    journal: &mut Journal,
    phase: &str,
    hook: &str,
    hooks: &mut dyn Hooks,
) -> Result<(), String> {
    journal.phase = phase.into();
    atomic_json(&paths.root, JOURNAL, journal)?;
    hooks.checkpoint(hook)
}
fn load_state(paths: &Paths) -> Result<State, String> {
    let state = match fs::symlink_metadata(paths.root.join(STATE)) {
        Ok(_) => manifest::read_json::<State>(&paths.root.join(STATE))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State::empty(paths));
        }
        Err(_) => return Err("无法读取发布状态".into()),
    };
    if state.format != "vellum-release-state-v1"
        || state.paths != *paths
        || [&state.current, &state.previous, &state.candidate]
            .iter()
            .any(|id| id.as_ref().is_some_and(|id| !safe_id(id)))
    {
        return Err("发布状态或路径不一致".into());
    }
    Ok(state)
}
fn current(paths: &Paths) -> Result<Option<String>, String> {
    let path = paths.root.join("current");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("无法读取当前版本".into()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(path).map_err(|_| "无法读取当前版本链接")?;
            let text = target.to_str().ok_or("当前版本链接无效")?;
            let id = text
                .strip_prefix("releases/")
                .ok_or("当前链接不属于发布管理器")?;
            if !safe_id(id) {
                return Err("当前链接版本无效".into());
            }
            Ok(Some(id.into()))
        }
        Ok(_) => Err("current被非管理器文件占用".into()),
    }
}
fn replace_symlink(path: &Path, target: &Path) -> Result<(), String> {
    directory(path.parent().ok_or("链接缺少父目录")?)?;
    let temporary = path
        .parent()
        .unwrap()
        .join(format!(".vellum-link-{}", nonce()));
    symlink(target, &temporary).map_err(|_| "无法准备版本链接")?;
    fs::rename(&temporary, path).map_err(|_| "无法切换版本链接")?;
    sync_dir(path.parent().unwrap())
}
fn switch_current(
    paths: &Paths,
    id: Option<&str>,
    allowed: &[Option<String>],
) -> Result<(), String> {
    let actual = current(paths)?;
    if !allowed.contains(&actual) {
        return Err("当前版本在操作期间改变，拒绝覆盖".into());
    }
    if let Some(id) = id {
        verify_release(paths, id)?;
        replace_symlink(
            &paths.root.join("current"),
            Path::new(&format!("releases/{id}")),
        )
    } else if actual.is_some() {
        fs::remove_file(paths.root.join("current")).map_err(|_| "无法恢复空的当前版本")?;
        sync_dir(&paths.root)
    } else {
        Ok(())
    }
}
fn previous_link(paths: &Paths, previous: Option<&str>) -> Result<(), String> {
    let path = paths.root.join("previous");
    if let Ok(meta) = fs::symlink_metadata(&path)
        && (!meta.file_type().is_symlink()
            || !fs::read_link(&path)
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned))
                .is_some_and(|p| p.strip_prefix("releases/").is_some_and(safe_id)))
    {
        return Err("previous链接归属无效".into());
    }
    if let Some(previous) = previous {
        replace_symlink(&path, Path::new(&format!("releases/{previous}")))
    } else {
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(path).map_err(|_| "无法移除旧版本链接")?;
            sync_dir(&paths.root)?;
        }
        Ok(())
    }
}
fn public_target(paths: &Paths, relative: &str) -> PathBuf {
    paths.root.join("current").join(relative)
}
fn public_is_ours(paths: &Paths, entry: &super::paths::Entry) -> bool {
    fs::read_link(&entry.public).is_ok_and(|target| target == public_target(paths, &entry.relative))
}
fn copy(source: &Path, destination: &Path, executable: bool) -> Result<(), String> {
    let input = manifest::regular(source)?;
    let cap = if BINARIES
        .iter()
        .any(|name| destination.file_name() == Some(std::ffi::OsStr::new(name)))
    {
        manifest::MAX_FILE_BYTES
    } else {
        manifest::MAX_RESOURCE_BYTES
    };
    if input.metadata().map_err(|_| "无法检查复制源大小")?.len() > cap {
        return Err("复制源超过对应成员上限".into());
    }
    let mut input = input.take(cap + 1);
    directory(destination.parent().ok_or("发布成员缺少父目录")?)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(if executable { 0o755 } else { 0o600 })
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)
        .map_err(|_| "无法复制发布成员")?;
    let copied = std::io::copy(&mut input, &mut output).map_err(|_| "发布复制失败")?;
    if copied > cap {
        return Err("发布成员复制期间超过上限".into());
    }
    output.sync_all().map_err(|_| "发布同步失败")?;
    sync_dir(destination.parent().unwrap())
}
fn render_resource(text: &str, launcher: &Path, tray: &Path) -> String {
    let launcher = launcher.to_string_lossy();
    let tray = tray.to_string_lossy();
    // TryExec is a single pathname, not the Exec argv syntax.
    text.replace("TryExec=@VELLUM_LAUNCHER@", &format!("TryExec={launcher}"))
        .replace("TryExec=@VELLUM_TRAY@", &format!("TryExec={tray}"))
        .replace("@VELLUM_LAUNCHER@", &format!("\"{launcher}\""))
        .replace("@VELLUM_TRAY@", &format!("\"{tray}\""))
}
fn generated(paths: &Paths, directory: &Path, id: &str) -> Result<Vec<FileRecord>, String> {
    let final_directory = paths.release(id)?;
    let mut records = Vec::new();
    for relative in RESOURCES {
        let source = directory.join("resources").join(relative);
        let file = manifest::regular(&source)?;
        if file.metadata().map_err(|_| "无法检查资源模板")?.len() > manifest::MAX_RESOURCE_BYTES
        {
            return Err("资源模板过大".into());
        }
        let mut bytes = Vec::new();
        file.take(manifest::MAX_RESOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "无法生成资源")?;
        if bytes.len() as u64 > manifest::MAX_RESOURCE_BYTES {
            return Err("资源模板读取期间超过上限".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "资源模板非UTF-8")?;
        if !relative.starts_with("icons/")
            && !text.contains("@VELLUM_LAUNCHER@")
            && !text.contains("@VELLUM_TRAY@")
        {
            return Err("启动资源必须使用受管入口模板".into());
        }
        // systemd units pin a generation. Desktop/DBus/autostart hosts may cache
        // Exec, so their entry remains stable and resolves current at launch.
        let bin = if relative.starts_with("systemd/") {
            final_directory.join("bin")
        } else {
            paths.bin_dir.clone()
        };
        let text = render_resource(&text, &bin.join("vellum"), &bin.join("vellum-tray"));
        let relative = format!("generated/{relative}");
        write_new(&directory.join(&relative), text.as_bytes(), false)?;
        records.push(manifest::record(directory, &relative)?);
    }
    Ok(records)
}

fn verify_release(paths: &Paths, id: &str) -> Result<Installed, String> {
    let directory = paths.release(id)?;
    check_chain(&directory)?;
    let receipt: Installed = manifest::read_json(&directory.join("installed.json"))?;
    if receipt.format != "vellum-installed-v1" || receipt.build_id != id || receipt.release_id != id
    {
        return Err("已安装版本标记无效".into());
    }
    let expected: BTreeSet<_> = receipt
        .files
        .iter()
        .chain(&receipt.generated)
        .map(|file| file.path.clone())
        .chain(["installed.json".into()])
        .collect();
    if expected.len() != receipt.files.len() + receipt.generated.len() + 1
        || manifest::inventory(&directory)? != expected
    {
        return Err("已安装版本含未知或重复成员".into());
    }
    for record in receipt.files.iter().chain(&receipt.generated) {
        manifest::verify_record(&directory, record)?;
    }
    if !receipt.legacy {
        let manifest = Manifest::read(&directory)?;
        if manifest.release_id != id {
            return Err("版本清单ID不一致".into());
        }
    }
    Ok(receipt)
}
fn stage(
    paths: &Paths,
    bundle: &Path,
    manifest: &Manifest,
    journal: &mut Journal,
    hooks: &mut dyn Hooks,
) -> Result<(), String> {
    let final_directory = paths.release(&manifest.release_id)?;
    if fs::symlink_metadata(&final_directory).is_ok() {
        verify_release(paths, &manifest.release_id)?;
        let installed = Manifest::read(&final_directory)?;
        if serde_json::to_value(&installed).ok() != serde_json::to_value(manifest).ok() {
            return Err("同一版本ID对应不同内容，拒绝覆盖".into());
        }
        return checkpoint(paths, journal, "staged", "staged", hooks);
    }
    let name = format!("stage-{}", nonce());
    let temporary = paths.root.join(&name);
    journal.stage = Some(name);
    atomic_json(&paths.root, JOURNAL, journal)?;
    directory(&temporary)?;
    sync_dir(&paths.root)?;
    hooks.checkpoint("stage-created")?;
    for entry in &manifest.files {
        copy(
            &bundle.join(&entry.path),
            &temporary.join(&entry.path),
            entry.executable,
        )?;
        manifest::verify_record(&temporary, entry)?;
        hooks.checkpoint("file-copied")?;
    }
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|_| "无法编码版本清单")?;
    write_new(&temporary.join("manifest.json"), &bytes, false)?;
    manifest.verify(&temporary, true)?;
    let mut files = manifest.files.clone();
    files.push(manifest::record(&temporary, "manifest.json")?);
    let receipt = Installed {
        format: "vellum-installed-v1".into(),
        release_id: manifest.release_id.clone(),
        build_id: manifest.release_id.clone(),
        generated: generated(paths, &temporary, &manifest.release_id)?,
        files,
        legacy: false,
    };
    write_new(
        &temporary.join("installed.json"),
        &serde_json::to_vec_pretty(&receipt).map_err(|_| "无法编码安装记录")?,
        false,
    )?;
    directory(&paths.root.join("releases"))?;
    fs::rename(&temporary, &final_directory).map_err(|_| "无法提交完整版本目录")?;
    sync_dir(&paths.root.join("releases"))?;
    sync_dir(&paths.root)?;
    journal.stage = None;
    checkpoint(paths, journal, "staged", "staged", hooks)
}
fn legacy_needed(paths: &Paths, old: &State, adopt: bool) -> Result<bool, String> {
    let mut regular = false;
    for entry in paths.entries() {
        check_chain(entry.public.parent().ok_or("入口缺少父目录")?)?;
        match fs::symlink_metadata(&entry.public) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("无法检查已有入口".into()),
            Ok(_) if public_is_ours(paths, &entry) && old.current.is_some() => {}
            Ok(meta) if meta.is_file() && old.current.is_none() && adopt => {
                manifest::regular(&entry.public)?;
                regular = true;
            }
            Ok(_) => {
                return Err(
                    "已有入口不属于受管版本；仅明确的旧式安装可用--adopt-legacy接管".into(),
                );
            }
        }
    }
    if regular {
        for bin in BINARIES {
            let path = paths.bin_dir.join(bin);
            manifest::regular(&path)?;
            if manifest::has_build_info_marker(&path)? {
                return Err(
                    "拒绝将带新式构建身份的散装程序移入legacy目录；请使用完整发行包建立受管安装"
                        .into(),
                );
            }
        }
    }
    Ok(regular)
}
fn adopt_legacy_installation(
    paths: &Paths,
    journal: &mut Journal,
    hooks: &mut dyn Hooks,
) -> Result<(), String> {
    let target = journal.target.as_ref().ok_or("缺少目标版本")?.clone();
    let id = format!("legacy-{}", nonce());
    journal.adoption = Some(id.clone());
    checkpoint(paths, journal, "adopting", "adoption-started", hooks)?;
    let directory_path = paths.release(&id)?;
    directory(&directory_path)?;
    let mut files = Vec::new();
    let mut generated_files = Vec::new();
    for entry in paths.entries() {
        let present = fs::symlink_metadata(&entry.public).is_ok();
        if entry.relative.starts_with("bin/") {
            copy(&entry.public, &directory_path.join(&entry.relative), true)?;
            files.push(manifest::record(&directory_path, &entry.relative)?);
        } else {
            let resource = entry
                .relative
                .strip_prefix("generated/")
                .ok_or("旧入口路径无效")?;
            let bytes = if present {
                let original = format!("originals/{}", entry.relative);
                copy(&entry.public, &directory_path.join(&original), false)?;
                files.push(manifest::record(&directory_path, &original)?);
                fs::read(directory_path.join(original)).map_err(|_| "无法读取旧资源备份")?
            } else {
                fs::read(paths.release(&target)?.join("resources").join(resource))
                    .map_err(|_| "无法读取缺失资源的模板")?
            };
            let text = String::from_utf8(bytes).map_err(|_| "旧资源不是UTF-8，拒绝自动接管")?;
            let bin = if resource.starts_with("systemd/") {
                directory_path.join("bin")
            } else {
                paths.bin_dir.clone()
            };
            let public_launcher = paths.bin_dir.join("vellum").to_string_lossy().into_owned();
            let public_tray = paths
                .bin_dir
                .join("vellum-tray")
                .to_string_lossy()
                .into_owned();
            let text = if resource.starts_with("icons/") {
                text
            } else {
                let normalized = text
                    .replace(&format!("\"{public_tray}\""), "@VELLUM_TRAY@")
                    .replace(&public_tray, "@VELLUM_TRAY@")
                    .replace(&format!("\"{public_launcher}\""), "@VELLUM_LAUNCHER@")
                    .replace(&public_launcher, "@VELLUM_LAUNCHER@");
                render_resource(&normalized, &bin.join("vellum"), &bin.join("vellum-tray"))
            };
            write_new(
                &directory_path.join(&entry.relative),
                text.as_bytes(),
                false,
            )?;
            generated_files.push(manifest::record(&directory_path, &entry.relative)?);
        }
    }
    let receipt = Installed {
        format: "vellum-installed-v1".into(),
        release_id: id.clone(),
        build_id: id.clone(),
        files,
        generated: generated_files,
        legacy: true,
    };
    write_new(
        &directory_path.join("installed.json"),
        &serde_json::to_vec_pretty(&receipt).map_err(|_| "无法写入旧版保全记录")?,
        false,
    )?;
    verify_release(paths, &id)?;
    journal.old.current = Some(id.clone());
    journal.old.status = "ready".into();
    checkpoint(
        paths,
        journal,
        "adoption-backed-up",
        "adoption-backed-up",
        hooks,
    )?;
    switch_current(paths, Some(&id), &[None, Some(id.clone())])?;
    Ok(())
}
fn install_links(paths: &Paths, journal: &Journal) -> Result<(), String> {
    let legacy = if journal.adoption.is_some() {
        journal
            .old
            .current
            .as_deref()
            .map(|id| verify_release(paths, id))
            .transpose()?
    } else {
        None
    };
    for entry in paths.entries() {
        check_chain(entry.public.parent().ok_or("入口缺少父目录")?)?;
        match fs::symlink_metadata(&entry.public) {
            Ok(_) if public_is_ours(paths, &entry) => continue,
            Ok(meta) if meta.is_file() && legacy.is_some() => {
                let legacy = legacy.as_ref().unwrap();
                let relative = if entry.relative.starts_with("bin/") {
                    entry.relative.clone()
                } else {
                    format!("originals/{}", entry.relative)
                };
                let saved = legacy
                    .files
                    .iter()
                    .find(|file| file.path == relative)
                    .ok_or("旧入口没有备份，拒绝覆盖")?;
                if manifest::digest(&entry.public)? != saved.sha256 {
                    return Err("旧入口在备份后已被修改，拒绝覆盖".into());
                }
            }
            Ok(_) => return Err("公共入口在操作期间被替换，拒绝覆盖".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("无法读取公共入口".into()),
        }
        replace_symlink(&entry.public, &public_target(paths, &entry.relative))?;
    }
    Ok(())
}
fn remove_links(paths: &Paths) -> Result<Vec<PathBuf>, String> {
    let mut preserved = Vec::new();
    for entry in paths.entries() {
        if public_is_ours(paths, &entry) {
            fs::remove_file(&entry.public).map_err(|_| "无法移除自有入口")?;
            sync_dir(entry.public.parent().unwrap())?;
        } else if fs::symlink_metadata(&entry.public).is_ok() {
            preserved.push(entry.public);
        }
    }
    Ok(preserved)
}
fn rollback_failed(
    paths: &Paths,
    journal: &mut Journal,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    let target = journal.target.clone();
    let old = journal.old.current.clone();
    if old.is_none() {
        services.restore(paths, None, &journal.services)?;
    }
    switch_current(paths, old.as_deref(), &[old.clone(), target.clone(), None])?;
    if let Some(id) = &old {
        services.restore(paths, Some(&paths.release(id)?), &journal.services)?;
    } else {
        remove_links(paths)?;
    }
    previous_link(paths, journal.old.previous.as_deref())?;
    checkpoint(paths, journal, "rolled-back", "rollback-restored", hooks)?;
    let mut state = journal.old.clone();
    state.status = "rolled-back".into();
    state.candidate = None;
    atomic_json(&paths.root, STATE, &state)?;
    remove_journal(paths)?;
    let mut report = state.report("新版本激活失败，已恢复原版本及服务状态；设置未回滚");
    report.success = false;
    Ok(report)
}
fn finish(
    paths: &Paths,
    journal: &mut Journal,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    let target = journal.target.clone().ok_or("日志缺少候选版本")?;
    verify_release(paths, &target)?;
    let now = services.snapshot(paths)?;
    if now.busy {
        checkpoint(paths, journal, "deferred", "activation-deferred", hooks)?;
        let mut state = journal.old.clone();
        state.candidate = Some(target);
        state.status = "installed-pending-activation".into();
        atomic_json(&paths.root, STATE, &state)?;
        return Ok(state.report("完整版本已准备；现有窗口或截图任务仍在运行，稍后执行repair激活"));
    }
    // Re-capture the policy at the actual switch, not the possibly hours-old
    // deferred snapshot. This is also the compensation baseline.
    journal.services = now.clone();
    atomic_json(&paths.root, JOURNAL, journal)?;
    install_links(paths, journal)?;
    checkpoint(paths, journal, "links-ready", "links-ready", hooks)?;
    checkpoint(paths, journal, "prepared", "journal-prepared", hooks)?;
    let old = journal.old.current.clone();
    switch_current(
        paths,
        Some(&target),
        &[old.clone(), Some(target.clone()), None],
    )?;
    previous_link(paths, old.as_deref())?;
    checkpoint(paths, journal, "switched", "current-switched", hooks)?;
    if !now.reachable {
        checkpoint(
            paths,
            journal,
            "pending-activation",
            "activation-deferred",
            hooks,
        )?;
        let state = State {
            current: Some(target.clone()),
            previous: old,
            candidate: Some(target),
            status: "installed-pending-activation".into(),
            ..journal.old.clone()
        };
        atomic_json(&paths.root, STATE, &state)?;
        return Ok(
            state.report("文件已安装；用户服务不可达或激活被禁用，尚未确认运行，请稍后repair")
        );
    }
    let mut desired = if journal.services.reachable {
        journal.services.clone()
    } else {
        now
    };
    desired.busy = false;
    desired.reachable = true;
    if old.is_none() {
        desired.enabled = UNITS.iter().map(|s| (*s).into()).collect();
        desired.active = desired.enabled.clone();
    }
    checkpoint(paths, journal, "activating", "activation-started", hooks)?;
    if services
        .activate(paths, &paths.release(&target)?, &desired)
        .is_err()
    {
        return rollback_failed(paths, journal, services, hooks);
    }
    checkpoint(paths, journal, "activated", "activated", hooks)?;
    let state = State {
        current: Some(target),
        previous: old,
        candidate: None,
        status: "ready".into(),
        ..journal.old.clone()
    };
    atomic_json(&paths.root, STATE, &state)?;
    checkpoint(paths, journal, "committed", "committed", hooks)?;
    remove_journal(paths)?;
    Ok(state.report("版本已完整安装并按原服务策略激活；用户设置保持不变"))
}
pub fn version_in_use(directory: &Path) -> Result<bool, String> {
    use std::os::unix::ffi::OsStrExt;
    let uid = unsafe { libc::geteuid() };
    for entry in fs::read_dir("/proc").map_err(|_| "无法确认运行中的版本")? {
        let entry = entry.map_err(|_| "无法检查进程")?;
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let meta = match fs::metadata(entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.uid() != uid {
            continue;
        }
        match fs::read_link(entry.path().join("exe")) {
            Ok(exe) if exe.starts_with(directory) => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("无法确认用户进程是否使用该版本".into()),
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(entry.path().join("cmdline"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("无法读取用户进程路径".into()),
        };
        let mut args = Vec::new();
        file.take(65537)
            .read_to_end(&mut args)
            .map_err(|_| "无法判断进程使用的版本")?;
        if args.len() > 65536 {
            return Err("进程参数超过安全检查上限".into());
        }
        for argument in args.split(|byte| *byte == 0) {
            if Path::new(std::ffi::OsStr::from_bytes(argument)).starts_with(directory) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn prepare_cleanup(paths: &Paths, hooks: &mut dyn Hooks) -> Result<Vec<CleanupPlan>, String> {
    let root = paths.root.join("releases");
    if !root.exists() {
        return Ok(Vec::new());
    }
    check_chain(&root)?;
    let mut entries: Vec<_> = fs::read_dir(&root)
        .map_err(|_| "无法检查安装的程序版本")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "无法列出版本")?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut plans = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if !safe_id(id) || hooks.version_in_use(&entry.path()).unwrap_or(true) {
            continue;
        }
        let receipt = match verify_release(paths, id) {
            Ok(receipt) if !receipt.legacy => receipt,
            _ => continue,
        };
        let mut files = receipt.files;
        files.extend(receipt.generated);
        files.push(manifest::record(&entry.path(), "installed.json")?);
        plans.push(CleanupPlan {
            id: id.into(),
            files,
        });
        // Keep the durable journal below its bounded reader limit. A repeated
        // explicit uninstall can clean further unused versions; all are reported.
        if plans.len() >= 16 {
            break;
        }
    }
    Ok(plans)
}
fn cleanup_layout(directory: &Path, plan: &CleanupPlan) -> Result<(), String> {
    let names: BTreeSet<_> = plan.files.iter().map(|file| file.path.clone()).collect();
    if names.len() != plan.files.len()
        || plan.files.len() > 128
        || !names.contains("installed.json")
        || !names.contains("manifest.json")
    {
        return Err("程序清理计划无效".into());
    }
    let mut allowed_directories = BTreeSet::new();
    allowed_directories.insert(PathBuf::new());
    for file in &plan.files {
        if !super::paths::safe_relative(&file.path) {
            return Err("程序清理路径越界".into());
        }
        let mut parent = Path::new(&file.path).parent();
        while let Some(path) = parent {
            allowed_directories.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    fn walk(
        at: &Path,
        root: &Path,
        names: &BTreeSet<String>,
        dirs: &BTreeSet<PathBuf>,
    ) -> Result<(), String> {
        let metadata = fs::symlink_metadata(at).map_err(|_| "无法检查待清理目录")?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("待清理程序目录被替换".into());
        }
        for entry in fs::read_dir(at).map_err(|_| "无法列出待清理版本")? {
            let path = entry.map_err(|_| "无法读取清理成员")?.path();
            let relative = path.strip_prefix(root).map_err(|_| "清理路径越界")?;
            let meta = fs::symlink_metadata(&path).map_err(|_| "无法检查清理成员")?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if !dirs.contains(relative) {
                    return Err("版本内有未知目录，已保留".into());
                }
                walk(&path, root, names, dirs)?;
            } else if !meta.is_file() || !names.contains(relative.to_str().ok_or("清理路径非UTF8")?)
            {
                return Err("版本内有未知成员，已保留".into());
            }
        }
        Ok(())
    }
    walk(directory, directory, &names, &allowed_directories)?;
    for file in &plan.files {
        if fs::symlink_metadata(directory.join(&file.path)).is_ok() {
            manifest::verify_record(directory, file)?;
        }
    }
    Ok(())
}
fn clean_program(paths: &Paths, plan: &CleanupPlan, hooks: &mut dyn Hooks) -> Result<bool, String> {
    if !safe_id(&plan.id) {
        return Err("清理版本ID无效".into());
    }
    let directory = paths.release(&plan.id)?;
    if fs::symlink_metadata(&directory).is_err() {
        return Ok(true);
    }
    if hooks.version_in_use(&directory).unwrap_or(true) || cleanup_layout(&directory, plan).is_err()
    {
        return Ok(false);
    }
    let mut files = plan.files.clone();
    files.sort_by_key(|file| match file.path.as_str() {
        "installed.json" => 2,
        "manifest.json" => 1,
        _ => 0,
    });
    for file in &files {
        let path = directory.join(&file.path);
        if fs::symlink_metadata(&path).is_err() {
            continue;
        }
        if hooks.version_in_use(&directory).unwrap_or(true)
            || manifest::verify_record(&directory, file).is_err()
        {
            return Ok(false);
        }
        fs::remove_file(&path).map_err(|_| "无法删除已校验的程序文件")?;
        sync_dir(path.parent().unwrap())?;
        hooks.checkpoint("program-file-removed")?;
    }
    let mut directories = BTreeSet::new();
    directories.insert(directory.clone());
    for file in files {
        let path = directory.join(file.path);
        let mut parent = path.parent();
        while let Some(path) = parent {
            if !path.starts_with(&directory) {
                break;
            }
            directories.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in directories {
        if !path.exists() {
            continue;
        }
        if fs::remove_dir(&path).is_err() {
            return Ok(false);
        }
        sync_dir(path.parent().unwrap())?;
    }
    hooks.checkpoint("program-version-removed")?;
    Ok(true)
}

fn read_journal(paths: &Paths) -> Result<Journal, String> {
    let journal: Journal = manifest::read_json(&paths.root.join(JOURNAL))?;
    if journal.format != "vellum-release-journal-v1"
        || journal.paths != *paths
        || journal.old.paths != *paths
        || journal.old.format != "vellum-release-state-v1"
        || [
            &journal.old.current,
            &journal.old.previous,
            &journal.old.candidate,
            &journal.target,
            &journal.adoption,
        ]
        .iter()
        .any(|id| id.as_ref().is_some_and(|id| !safe_id(id)))
        || journal
            .stage
            .as_ref()
            .is_some_and(|name| !safe_id(name) || !name.starts_with("stage-"))
    {
        return Err("恢复日志路径或格式无效".into());
    }
    Ok(journal)
}
fn cancel_pending(
    paths: &Paths,
    mut state: State,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    let pending = read_journal(paths)?;
    if pending.operation == "uninstall" || pending.operation == "cancel-legacy" {
        return uninstall(paths, pending.old.clone(), services, hooks, Some(pending));
    }
    if !matches!(pending.operation.as_str(), "install" | "rollback")
        || !matches!(
            pending.phase.as_str(),
            "deferred" | "pending-activation" | "legacy-deferred"
        )
    {
        return Err("该事务不是待激活安装，请先repair确认中断状态".into());
    }
    let legacy = pending.phase == "legacy-deferred";
    if legacy && (pending.old.current.is_some() || pending.adoption.is_some()) {
        return Err("旧式待接管日志不一致".into());
    }
    let actual = current(paths)?;
    let expected = if pending.phase == "pending-activation" {
        pending.target.clone()
    } else {
        pending.old.current.clone()
    };
    if actual != expected {
        return Err("待激活事务的current已改变，请先repair检查".into());
    }
    state.current = actual;
    if state.current != pending.old.current {
        state.previous = pending.old.current.clone();
    }
    let cancellation = Journal {
        format: "vellum-release-journal-v1".into(),
        paths: paths.clone(),
        operation: if legacy { "cancel-legacy" } else { "uninstall" }.into(),
        target: pending.target,
        old: state.clone(),
        services: pending.services,
        phase: "uninstall-prepared".into(),
        stage: None,
        adoption: pending.adoption,
        cleanup: Vec::new(),
        cleanup_prepared: false,
    };
    // Replace the install intent durably before touching anything: repair may
    // now only finish cancellation, never unexpectedly activate the candidate.
    atomic_json(&paths.root, JOURNAL, &cancellation)?;
    uninstall(paths, state, services, hooks, Some(cancellation))
}

fn uninstall(
    paths: &Paths,
    state: State,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
    existing: Option<Journal>,
) -> Result<Report, String> {
    for entry in paths.entries() {
        check_chain(entry.public.parent().ok_or("入口缺少父目录")?)?;
    }
    let links_already_removed = existing.as_ref().is_some_and(|journal| {
        matches!(
            journal.phase.as_str(),
            "uninstall-stopped" | "uninstall-links-removed" | "committed"
        )
    });
    let cancel_legacy = existing
        .as_ref()
        .is_some_and(|journal| journal.operation == "cancel-legacy");
    if cancel_legacy && (state.current.is_some() || current(paths)?.is_some()) {
        return Err("待取消的旧式安装已出现current，拒绝覆盖新的选择".into());
    }
    // No manager-owned current means no authority to stop an existing legacy
    // application's services, or to let its GUI block cancellation of our files.
    let snapshot = if cancel_legacy || state.current.is_none() {
        ServiceSnapshot::default()
    } else {
        services.snapshot(paths)?
    };
    if snapshot.busy {
        let mut cancellation = existing.unwrap_or(Journal {
            format: "vellum-release-journal-v1".into(),
            paths: paths.clone(),
            operation: "uninstall".into(),
            target: None,
            old: state.clone(),
            services: snapshot.clone(),
            phase: "uninstall-deferred".into(),
            stage: None,
            adoption: None,
            cleanup: Vec::new(),
            cleanup_prepared: false,
        });
        checkpoint(
            paths,
            &mut cancellation,
            "uninstall-deferred",
            "uninstall-deferred",
            hooks,
        )?;
        let mut report =
            state.report("现有窗口或截图任务未结束，卸载意图已保留且未删除入口；稍后repair继续");
        report.state = "installed-pending-activation".into();
        report.candidate = None;
        return Ok(report);
    }
    let mut journal = existing.unwrap_or(Journal {
        format: "vellum-release-journal-v1".into(),
        paths: paths.clone(),
        operation: "uninstall".into(),
        target: None,
        old: state.clone(),
        services: snapshot.clone(),
        phase: "uninstall-prepared".into(),
        stage: None,
        adoption: None,
        cleanup: Vec::new(),
        cleanup_prepared: false,
    });
    if !links_already_removed {
        journal.services = snapshot.clone();
        checkpoint(
            paths,
            &mut journal,
            "uninstall-prepared",
            "uninstall-prepared",
            hooks,
        )?;
        if snapshot.reachable {
            services.stop(paths, &journal.services)?;
        }
        checkpoint(
            paths,
            &mut journal,
            "uninstall-stopped",
            "uninstall-stopped",
            hooks,
        )?;
    }
    let mut preserved = if cancel_legacy {
        paths
            .entries()
            .into_iter()
            .filter(|entry| fs::symlink_metadata(&entry.public).is_ok())
            .map(|entry| entry.public)
            .collect()
    } else {
        remove_links(paths)?
    };
    if !journal.cleanup_prepared {
        journal.cleanup = prepare_cleanup(paths, hooks)?;
        if cancel_legacy {
            journal.cleanup.retain(|plan| {
                Some(&plan.id) == journal.target.as_ref()
                    && Some(&plan.id) != journal.old.previous.as_ref()
            });
        }
        journal.cleanup_prepared = true;
    }
    checkpoint(
        paths,
        &mut journal,
        "uninstall-links-removed",
        "uninstall-links-removed",
        hooks,
    )?;
    if !cancel_legacy {
        switch_current(paths, None, &[journal.old.current.clone(), None])?;
        previous_link(paths, None)?;
    }
    for plan in &journal.cleanup {
        let _ = clean_program(paths, plan, hooks)?;
    }
    if let Ok(entries) = fs::read_dir(paths.root.join("releases")) {
        for entry in entries.flatten() {
            preserved.push(entry.path());
        }
    }
    let previous = if cancel_legacy {
        journal.old.previous.clone()
    } else {
        journal
            .old
            .current
            .clone()
            .filter(|id| verify_release(paths, id).is_ok())
    };
    if !cancel_legacy {
        previous_link(paths, previous.as_deref())?;
    }
    let state = State {
        current: None,
        previous,
        candidate: None,
        status: if cancel_legacy {
            "cancelled-pending-install"
        } else {
            "uninstalled"
        }
        .into(),
        ..state
    };
    atomic_json(&paths.root, STATE, &state)?;
    checkpoint(paths, &mut journal, "committed", "committed", hooks)?;
    remove_journal(paths)?;
    // No journal is open any more, so any staging directory left in the root is
    // an abandoned copy from an earlier interrupted attempt.
    let swept = discard_orphan_stages(paths);
    let mut report = state.report(if swept.is_empty() {
        "已卸载自有入口并清理完整且未使用的程序版本；legacy备份、在用/改动/未知版本及全部用户数据保留"
    } else {
        "已卸载自有入口并清理完整且未使用的程序版本；同时清理了上次失败留下的暂存副本；用户设置与图片保留"
    });
    if cancel_legacy {
        report.message =
            "已取消尚未接管的候选安装；旧式regular入口、运行中的旧程序与服务均未修改".into();
    }
    report.preserved = preserved;
    Ok(report)
}
/// Remove the private staging copy of a candidate that was never activated.
///
/// Only a `stage-*` directory directly under our own root whose manifest names
/// the journal target is ever touched; anything unexpected is left in place and
/// reported instead, so a surprising layout is never deleted.
fn discard_stage(
    paths: &Paths,
    name: &str,
    require_manifest: bool,
    target: Option<&str>,
) -> Option<PathBuf> {
    if !name.starts_with("stage-") || name.contains('/') || name.contains("..") {
        return None;
    }
    let path = paths.root.join(name);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    // A directory the journal recorded is ours even when the copy was cut short
    // before the manifest landed; a manifest that does exist must still agree
    // with the target. An unreferenced directory has to prove itself instead.
    match Manifest::read(&path) {
        Ok(manifest) => {
            if let Some(target) = target
                && manifest.release_id != target
            {
                return None;
            }
        }
        Err(_) if require_manifest => return None,
        Err(_) => {}
    }
    fs::remove_dir_all(&path).ok()?;
    Some(path)
}

/// Staging directories only exist while a transaction is open, so any that
/// survive without a journal are abandoned copies. Swept by an explicit
/// uninstall; each one still has to look like our own staging area.
fn discard_orphan_stages(paths: &Paths) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.root) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if discard_stage(paths, &name, true, None).is_some() {
            removed.push(paths.root.join(name));
        }
    }
    removed
}

fn repair(
    paths: &Paths,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    if fs::symlink_metadata(paths.root.join(JOURNAL)).is_err() {
        let state = load_state(paths)?;
        if let Some(id) = &state.current {
            verify_release(paths, id)?;
            if current(paths)? != state.current {
                return Err("当前指针与状态不一致，需要人工检查".into());
            }
        }
        return Ok(state.report("没有未完成的发布事务"));
    }
    let mut journal = read_journal(paths)?;
    if matches!(journal.operation.as_str(), "uninstall" | "cancel-legacy") {
        return uninstall(paths, journal.old.clone(), services, hooks, Some(journal));
    }
    if journal.phase == "activating" {
        if services.snapshot(paths)?.busy {
            let mut report = journal.old.report("激活中断，用户窗口仍在运行，未强制回退");
            report.state = "installed-pending-activation".into();
            report.current = current(paths)?;
            report.candidate = journal.target.clone();
            return Ok(report);
        }
        return rollback_failed(paths, &mut journal, services, hooks);
    }
    if journal.phase == "rolled-back" {
        if current(paths)? != journal.old.current {
            return Err("已回退的current被修改，拒绝覆盖".into());
        }
        if let Some(id) = &journal.old.current {
            verify_release(paths, id)?;
        }
        let state = State {
            status: "rolled-back".into(),
            candidate: None,
            ..journal.old.clone()
        };
        atomic_json(&paths.root, STATE, &state)?;
        remove_journal(paths)?;
        return Ok(state.report("已完成原版本恢复，清理中断日志"));
    }
    if matches!(journal.phase.as_str(), "preparing" | "adopting") {
        // No public file was touched in these phases: the candidate only ever
        // existed as a private staging copy. Discard it instead of leaving a
        // bundle-sized directory behind for the user to find and wonder about.
        let old = journal.old.clone();
        let stage = journal.stage.clone();
        let target = journal.target.clone();
        atomic_json(&paths.root, STATE, &old)?;
        remove_journal(paths)?;
        let cleaned = stage
            .as_deref()
            .and_then(|name| discard_stage(paths, name, false, target.as_deref()));
        let mut report = old.report(if cleaned.is_some() {
            "未完成准备已撤销；旧安装未改动，私有临时副本已清理"
        } else {
            "未完成准备已撤销；旧安装未改动，私有临时副本保留供检查"
        });
        if cleaned.is_none()
            && let Some(stage) = stage
        {
            report.preserved.push(paths.root.join(stage));
        }
        return Ok(report);
    }
    if journal.phase == "committed" || journal.phase == "activated" {
        let target = journal.target.clone().ok_or("日志缺少版本")?;
        verify_release(paths, &target)?;
        if current(paths)? != Some(target.clone()) {
            return Err("已提交事务的current被修改，拒绝盲目修复".into());
        }
        let state = State {
            current: Some(target),
            previous: journal.old.current.clone(),
            candidate: None,
            status: "ready".into(),
            ..journal.old.clone()
        };
        atomic_json(&paths.root, STATE, &state)?;
        remove_journal(paths)?;
        return Ok(state.report("已完成事务日志修复"));
    }
    if journal.phase == "legacy-deferred" {
        if services.snapshot(paths)?.busy {
            let mut pending = journal.old.report("旧式入口未修改，等待现有窗口关闭");
            pending.state = "installed-pending-activation".into();
            pending.candidate = journal.target.clone();
            return Ok(pending);
        }
        adopt_legacy_installation(paths, &mut journal, hooks)?;
    }
    if journal.phase == "adoption-backed-up" {
        switch_current(
            paths,
            journal.old.current.as_deref(),
            &[None, journal.old.current.clone()],
        )?;
    }
    finish(paths, &mut journal, services, hooks)
}
pub fn execute(
    operation: Operation,
    paths: &Paths,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    if !paths.root.exists() && matches!(operation, Operation::Status | Operation::Repair) {
        return Ok(State::empty(paths).report("尚未安装受管版本"));
    }
    if matches!(operation, Operation::Status)
        && !paths.root.join(STATE).exists()
        && !paths.root.join(JOURNAL).exists()
    {
        let mut report = State::empty(paths).report("尚未安装受管版本");
        if current(paths)?.is_some() {
            report.state = "interrupted/needs-repair".into();
        }
        return Ok(report);
    }
    let _lock = lock(paths, !matches!(operation, Operation::Status))?;
    let mut state = load_state(paths)?;
    let mut journal_exists = fs::symlink_metadata(paths.root.join(JOURNAL)).is_ok();
    if journal_exists
        && matches!(&operation, Operation::Install { .. })
        && matches!(
            read_journal(paths)?.phase.as_str(),
            "preparing" | "adopting"
        )
    {
        // A transaction that stopped while merely preparing never touched a
        // public file, so a fresh install rolls it back and continues instead
        // of sending the user to repair for a state that lost nothing.
        repair(paths, services, hooks)?;
        state = load_state(paths)?;
        journal_exists = fs::symlink_metadata(paths.root.join(JOURNAL)).is_ok();
    }
    if matches!(operation, Operation::Status) {
        let mut report = state.report("发布状态");
        if journal_exists {
            let journal: Journal = manifest::read_json(&paths.root.join(JOURNAL))?;
            if matches!(
                journal.phase.as_str(),
                "deferred" | "pending-activation" | "legacy-deferred" | "uninstall-deferred"
            ) {
                report.state = "installed-pending-activation".into();
                report.current = current(paths)?;
                if journal.phase == "uninstall-deferred" {
                    report.candidate = None;
                    report.message = "待激活安装已取消，等待现有窗口结束后repair继续卸载".into();
                } else {
                    report.candidate = journal.target;
                    report.message =
                        "完整版本等待安全激活；也可用uninstall --yes取消，无需等待服务恢复".into();
                }
            } else {
                report.state = "interrupted/needs-repair".into();
                report.message = "存在未完成发布事务，请运行repair；未声称服务已就绪".into();
            }
        } else if let Some(id) = &state.current {
            if verify_release(paths, id).is_err() || current(paths)? != state.current {
                report.state = "interrupted/needs-repair".into();
            } else if !services.snapshot(paths)?.reachable {
                report.state = "installed-pending-activation".into();
                report.message = "文件一致；用户服务当前不可达，未声称已就绪".into();
            }
        }
        return Ok(report);
    }
    if matches!(operation, Operation::Repair) {
        return repair(paths, services, hooks);
    }
    if journal_exists {
        if let Operation::Uninstall { confirmed } = operation {
            if !confirmed {
                return Err("卸载需要明确--yes；待激活安装未改变".into());
            }
            return cancel_pending(paths, state, services, hooks);
        }
        // Name a program the user can actually run: nothing may be on PATH yet.
        let program = read_journal(paths)?
            .target
            .and_then(|id| paths.release(&id).ok())
            .map(|release| release.join("bin/vellum").display().to_string())
            .unwrap_or_else(|| "发行包内的 bin/vellum".into());
        return Err(format!(
            "有未完成的发布事务，请先运行repair；待激活安装可明确uninstall --yes取消（修复程序：{program}）"
        ));
    }
    if current(paths)? != state.current {
        return Err("current与发布记录不一致，拒绝修改".into());
    }
    match operation {
        Operation::Install {
            bundle,
            adopt_legacy,
        } => {
            let manifest = Manifest::read(&bundle)?;
            manifest.verify(&bundle, false)?;
            if state.current.as_deref() == Some(manifest.release_id.as_str()) {
                verify_release(paths, &manifest.release_id)?;
                let installed = Manifest::read(&paths.release(&manifest.release_id)?)?;
                if serde_json::to_value(&installed).ok() != serde_json::to_value(&manifest).ok() {
                    return Err("同一版本ID内容不同，拒绝覆盖".into());
                }
                return Ok(state.report("该完整版本已安装；保留原回退版本与服务状态"));
            }
            let legacy = legacy_needed(paths, &state, adopt_legacy)?;
            let snapshot = services.snapshot(paths)?;
            let mut journal = Journal {
                format: "vellum-release-journal-v1".into(),
                paths: paths.clone(),
                operation: "install".into(),
                target: Some(manifest.release_id.clone()),
                old: state,
                services: snapshot.clone(),
                phase: "preparing".into(),
                stage: None,
                adoption: None,
                cleanup: Vec::new(),
                cleanup_prepared: false,
            };
            atomic_json(&paths.root, JOURNAL, &journal)?;
            stage(paths, &bundle, &manifest, &mut journal, hooks)?;
            if legacy {
                if snapshot.busy {
                    checkpoint(
                        paths,
                        &mut journal,
                        "legacy-deferred",
                        "activation-deferred",
                        hooks,
                    )?;
                    let mut pending = journal.old.clone();
                    pending.status = "installed-pending-activation".into();
                    pending.candidate = journal.target.clone();
                    atomic_json(&paths.root, STATE, &pending)?;
                    return Ok(pending
                        .report("已准备完整候选版；旧式入口未修改，等待现有窗口关闭后repair接管"));
                }
                adopt_legacy_installation(paths, &mut journal, hooks)?;
            }
            finish(paths, &mut journal, services, hooks)
        }
        Operation::Rollback => {
            let target = state.previous.clone().ok_or("没有可回退的旧版")?;
            verify_release(paths, &target)?;
            let snapshot = services.snapshot(paths)?;
            let mut journal = Journal {
                format: "vellum-release-journal-v1".into(),
                paths: paths.clone(),
                operation: "rollback".into(),
                target: Some(target),
                old: state,
                services: snapshot,
                phase: "staged".into(),
                stage: None,
                adoption: None,
                cleanup: Vec::new(),
                cleanup_prepared: false,
            };
            atomic_json(&paths.root, JOURNAL, &journal)?;
            let mut report = finish(paths, &mut journal, services, hooks)?;
            if report.state == "ready" {
                report.state = "rolled-back".into();
                report.message = "已切回完整旧版，设置未回退".into();
            }
            Ok(report)
        }
        Operation::Uninstall { confirmed } => {
            if !confirmed {
                return Err("卸载需要明确--yes；用户设置与图片默认保留".into());
            }
            uninstall(paths, state, services, hooks, None)
        }
        Operation::Status | Operation::Repair => unreachable!(),
    }
}
