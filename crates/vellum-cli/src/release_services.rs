//! Bounded user-service activation. Tests inject commands and /proc; they never
//! call the workstation's manager. No raw command output is returned to users.
use super::{Paths, manifest};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const UNITS: [&str; 3] = [
    "vellum.service",
    "vellum-shortcuts.service",
    "vellum-tray.service",
];
const COMMAND_BYTES: usize = 64 * 1024;
const CMDLINE_BYTES: u64 = 64 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub reachable: bool,
    pub busy: bool,
    pub enabled: Vec<String>,
    pub active: Vec<String>,
}
pub trait Services {
    fn snapshot(&mut self, paths: &Paths) -> Result<ServiceSnapshot, String>;
    fn activate(
        &mut self,
        paths: &Paths,
        release_dir: &Path,
        previous: &ServiceSnapshot,
    ) -> Result<(), String>;
    fn restore(
        &mut self,
        paths: &Paths,
        release_dir: Option<&Path>,
        previous: &ServiceSnapshot,
    ) -> Result<(), String>;
    fn stop(&mut self, paths: &Paths, previous: &ServiceSnapshot) -> Result<(), String>;
}
#[derive(Default)]
pub struct RealServices {
    _private: (),
}
impl Services for RealServices {
    fn snapshot(&mut self, paths: &Paths) -> Result<ServiceSnapshot, String> {
        real_adapter().snapshot(paths)
    }
    fn activate(
        &mut self,
        paths: &Paths,
        release_dir: &Path,
        previous: &ServiceSnapshot,
    ) -> Result<(), String> {
        real_adapter().apply(paths, release_dir, previous)
    }
    fn restore(
        &mut self,
        paths: &Paths,
        release_dir: Option<&Path>,
        previous: &ServiceSnapshot,
    ) -> Result<(), String> {
        if !previous.reachable {
            return Ok(());
        }
        let mut adapter = real_adapter();
        match release_dir {
            Some(release) => adapter.apply(paths, release, previous),
            None => adapter.stop(paths, previous),
        }
    }
    fn stop(&mut self, paths: &Paths, previous: &ServiceSnapshot) -> Result<(), String> {
        real_adapter().stop(paths, previous)
    }
}
fn real_adapter() -> Adapter<SystemCommand> {
    Adapter {
        runner: SystemCommand(PathBuf::from("/usr/bin/systemctl")),
        proc_root: PathBuf::from("/proc"),
        uid: unsafe { libc::geteuid() },
        timeout: OPERATION_TIMEOUT,
        ipc_busy: real_ipc_busy,
    }
}

fn real_ipc_busy(deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return true;
    }
    vellum_ipc::client::send(
        &vellum_ipc::protocol::Request::Status,
        remaining.min(vellum_ipc::client::STATUS_TIMEOUT),
    )
    .is_some_and(|response| response.state == Some(vellum_ipc::protocol::State::Busy))
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
}
trait Runner {
    fn run(&mut self, args: &[OsString], deadline: Instant) -> Result<CommandOutput, String>;
}
struct SystemCommand(PathBuf);
impl Runner for SystemCommand {
    fn run(&mut self, args: &[OsString], deadline: Instant) -> Result<CommandOutput, String> {
        bounded_command(&self.0, args, deadline)
    }
}
struct Reap(Child, bool);
impl Drop for Reap {
    fn drop(&mut self) {
        if self.1 {
            return;
        }
        // Commands, not user GUI/service processes, are placed in this private group.
        let _ = unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn nonblocking(fd: i32) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("无法设置服务命令的有界输出读取".into());
    }
    Ok(())
}
fn drain(stream: &mut impl Read, bytes: &mut Vec<u8>, eof: &mut bool) -> Result<(), String> {
    let mut buffer = [0; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(n) => {
                if bytes.len().saturating_add(n) > COMMAND_BYTES {
                    return Err("服务命令输出超过安全上限".into());
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("无法读取服务命令结果".into()),
        }
    }
}
fn bounded_command(
    program: &Path,
    args: &[OsString],
    deadline: Instant,
) -> Result<CommandOutput, String> {
    if Instant::now() >= deadline {
        return Err("服务操作超过总时限".into());
    }
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .env("LC_ALL", "C")
        .process_group(0)
        .spawn()
        .map_err(|_| "无法启动用户服务管理命令".to_string())?;
    let mut child = Reap(child, false);
    let mut stdout = child.0.stdout.take().ok_or("服务命令缺少输出管道")?;
    let mut stderr = child.0.stderr.take().ok_or("服务命令缺少错误管道")?;
    nonblocking(stdout.as_raw_fd())?;
    nonblocking(stderr.as_raw_fd())?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut out_eof, mut err_eof) = (false, false);
    let mut status = None;
    loop {
        drain(&mut stdout, &mut out, &mut out_eof)?;
        drain(&mut stderr, &mut err, &mut err_eof)?;
        if status.is_none() {
            status = child
                .0
                .try_wait()
                .map_err(|_| "无法回收服务命令".to_string())?;
        }
        if let Some(status) = status
            && out_eof
            && err_eof
        {
            child.1 = true; // Already reaped with both pipes closed.
            return Ok(CommandOutput {
                success: status.success(),
                stdout: out,
            });
        }
        if Instant::now() >= deadline {
            return Err("服务操作超过总时限".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Clone, Debug)]
struct UnitState {
    fragment: PathBuf,
    active: bool,
    transition: bool,
    enabled: bool,
    missing: bool,
    main_pid: u32,
}
fn unit_state(output: CommandOutput) -> Result<UnitState, String> {
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "服务状态格式无效")?;
    let mut properties = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if properties.insert(key, value).is_some() {
            return Err("服务状态含重复字段".into());
        }
    }
    let get = |key| {
        properties
            .get(key)
            .copied()
            .ok_or_else(|| "服务状态缺少必要字段".to_string())
    };
    let missing = get("LoadState")? == "not-found";
    if !output.success && !missing {
        return Err("无法查询用户服务状态".into());
    }
    let state = get("ActiveState")?;
    let enabled = get("UnitFileState")?;
    if matches!(enabled, "enabled-runtime" | "linked-runtime") {
        return Err("当前服务使用临时启用状态，请先明确持久服务设置后再更新".into());
    }
    Ok(UnitState {
        fragment: PathBuf::from(get("FragmentPath")?),
        active: state == "active",
        transition: matches!(state, "activating" | "deactivating" | "reloading"),
        enabled: enabled == "enabled",
        missing,
        main_pid: get("MainPID")?.parse().map_err(|_| "服务进程标识无效")?,
    })
}

struct Adapter<R> {
    runner: R,
    proc_root: PathBuf,
    uid: u32,
    timeout: Duration,
    ipc_busy: fn(Instant) -> bool,
}
impl<R: Runner> Adapter<R> {
    fn command(&mut self, args: &[&OsStr], deadline: Instant) -> Result<CommandOutput, String> {
        let mut full: Vec<OsString> = ["--user", "--no-pager", "--no-ask-password"]
            .into_iter()
            .map(Into::into)
            .collect();
        full.extend(args.iter().map(|arg| arg.to_os_string()));
        self.runner.run(&full, deadline)
    }
    fn required(&mut self, args: &[&str], deadline: Instant) -> Result<(), String> {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        if !self.command(&args, deadline)?.success {
            return Err("用户服务管理操作失败；未确认服务就绪".into());
        }
        Ok(())
    }
    fn show(&mut self, name: &str, deadline: Instant) -> Result<UnitState, String> {
        let output = self.command(
            &[
                OsStr::new("show"),
                OsStr::new("--property=LoadState,FragmentPath,ActiveState,UnitFileState,MainPID"),
                OsStr::new("--"),
                OsStr::new(name),
            ],
            deadline,
        )?;
        unit_state(output)
    }
    fn busy(&self, deadline: Instant) -> Result<bool, String> {
        Ok(proc_busy(&self.proc_root, self.uid, deadline)? || (self.ipc_busy)(deadline))
    }
    fn ensure_idle(&self, deadline: Instant) -> Result<(), String> {
        if self.busy(deadline)? {
            return Err("Vellum窗口或截图任务仍在使用，请结束后再激活；不会强制关闭".into());
        }
        Ok(())
    }
    fn snapshot(&mut self, paths: &Paths) -> Result<ServiceSnapshot, String> {
        let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
        let activity = proc_activity(&self.proc_root, self.uid, deadline)?;
        let busy = activity.busy || (self.ipc_busy)(deadline);
        let reachable = self
            .command(
                &[
                    OsStr::new("show"),
                    OsStr::new("--property=Version"),
                    OsStr::new("--value"),
                ],
                deadline,
            )
            .is_ok_and(|output| output.success);
        let mut snapshot = ServiceSnapshot {
            reachable,
            busy,
            ..ServiceSnapshot::default()
        };
        if !reachable {
            snapshot.busy |= !activity.daemon_pids.is_empty();
            return Ok(snapshot);
        }
        let mut tracked_daemon = None;
        for name in UNITS {
            let state = self.show(name, deadline)?;
            if !state.missing {
                let loaded = state
                    .fragment
                    .canonicalize()
                    .map_err(|_| "无法确认已加载同名服务的归属；未改动服务")?;
                let public = paths
                    .config_dir
                    .join("systemd/user")
                    .join(name)
                    .canonicalize()
                    .ok();
                // current may have moved to B while systemd still caches A. Only
                // a matching receipt member from this same root permits that gap.
                if public.as_ref() != Some(&loaded)
                    && !verified_release_member(
                        paths,
                        &loaded,
                        &format!("generated/systemd/user/{name}"),
                    )
                {
                    return Err("已有同名服务来自其它安装或全局目录；未改动服务".into());
                }
                if state.active && !self.owned_process(paths, name, state.main_pid) {
                    return Err("已运行的同名服务不是本实例的对应Vellum进程；未改动服务".into());
                }
            }
            snapshot.busy |= state.transition;
            if name == "vellum.service" && !state.missing && state.main_pid != 0 {
                tracked_daemon = Some(state.main_pid);
            }
            if state.enabled {
                snapshot.enabled.push(name.into());
            }
            if state.active {
                snapshot.active.push(name.into());
            }
        }
        // A standalone legacy daemon can own the IPC socket while idle. It is
        // not safe to declare a new managed installation ready beside that process.
        snapshot.busy |= activity
            .daemon_pids
            .iter()
            .any(|pid| Some(*pid) != tracked_daemon);
        Ok(snapshot)
    }
    fn bindings(
        &mut self,
        paths: &Paths,
        release: &Path,
        deadline: Instant,
    ) -> Result<Vec<(String, UnitState)>, String> {
        let mut states = Vec::new();
        for name in UNITS {
            let state = self.show(name, deadline)?;
            verify_binding(paths, release, name, &state)?;
            if state.active && !self.owned_process(paths, name, state.main_pid) {
                return Err("已运行的同名服务不是本实例的对应Vellum进程；未改动服务".into());
            }
            states.push((name.to_string(), state));
        }
        Ok(states)
    }
    fn apply(
        &mut self,
        paths: &Paths,
        release: &Path,
        desired: &ServiceSnapshot,
    ) -> Result<(), String> {
        if !desired.reachable {
            return Err("用户服务管理器不可达，安装应保持待激活状态".into());
        }
        if desired.busy
            || desired
                .enabled
                .iter()
                .chain(&desired.active)
                .any(|name| !UNITS.contains(&name.as_str()))
        {
            return Err("服务恢复快照不可用于激活".into());
        }
        let deadline = Instant::now() + self.timeout;
        self.ensure_idle(deadline)?;
        self.required(&["daemon-reload"], deadline)?;
        // Validate all three fragments before touching any enable/active state.
        let states = self.bindings(paths, release, deadline)?;
        let mut changed_links = false;
        for (name, state) in states {
            let enable = desired.enabled.contains(&name);
            if enable == state.enabled {
                continue;
            }
            self.ensure_idle(deadline)?;
            let public = paths.config_dir.join("systemd/user").join(&name);
            let link = fs::read_link(&public).map_err(|_| "受管理服务入口不再是符号链接")?;
            verify_public(paths, release, &name)?;
            let result = self.required(
                &[if enable { "enable" } else { "disable" }, "--", &name],
                deadline,
            );
            // systemctl disable also removes manually linked unit entries. Restore
            // exactly the verified link only if that name is now absent.
            match fs::symlink_metadata(&public) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    std::os::unix::fs::symlink(&link, &public)
                        .map_err(|_| "无法恢复受管理服务入口")?;
                }
                Err(_) => return Err("无法确认受管理服务入口".into()),
                Ok(_) => {}
            }
            verify_public(paths, release, &name)?;
            result?;
            changed_links = true;
        }
        if changed_links {
            self.required(&["daemon-reload"], deadline)?;
            self.bindings(paths, release, deadline)?;
        }
        for name in UNITS.into_iter().rev() {
            if !desired.active.iter().any(|active| active == name) {
                let state = self.show(name, deadline)?;
                verify_binding(paths, release, name, &state)?;
                if state.active || state.transition {
                    self.ensure_idle(deadline)?;
                    self.required(&["stop", "--", name], deadline)?;
                }
            }
        }
        for name in UNITS {
            if desired.active.iter().any(|active| active == name) {
                self.ensure_idle(deadline)?;
                let state = self.show(name, deadline)?;
                verify_binding(paths, release, name, &state)?;
                self.required(&["restart", "--", name], deadline)?;
                loop {
                    let current = self.show(name, deadline)?;
                    verify_binding(paths, release, name, &current)?;
                    if current.active && self.matches_process(release, name, current.main_pid) {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err("服务未在总时限内运行目标版本".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        for (name, state) in self.bindings(paths, release, deadline)? {
            if state.transition
                || state.active != desired.active.contains(&name)
                || state.enabled != desired.enabled.contains(&name)
            {
                return Err("服务启用或运行状态未与目标快照一致".into());
            }
            if state.active && !self.matches_process(release, &name, state.main_pid) {
                return Err("服务实际进程不是目标版本".into());
            }
        }
        Ok(())
    }
    fn owned_process(&self, paths: &Paths, name: &str, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let process = self.proc_root.join(pid.to_string());
        if !fs::metadata(&process).is_ok_and(|metadata| metadata.uid() == self.uid) {
            return false;
        }
        let binary = match name {
            "vellum.service" => "vellum",
            "vellum-shortcuts.service" => "vellum-ui",
            "vellum-tray.service" => "vellum-tray",
            _ => return false,
        };
        let role_matches = match name {
            "vellum.service" => command_mode(&process).is_ok_and(|mode| mode == b"daemon"),
            "vellum-shortcuts.service" => {
                command_mode(&process).is_ok_and(|mode| mode == b"shortcuts-service")
            }
            _ => true,
        };
        if !role_matches {
            return false;
        }
        let Ok(link) = fs::read_link(process.join("exe")) else {
            return false;
        };
        let Some(text) = link.to_str() else {
            return false;
        };
        let actual = Path::new(text.strip_suffix(" (deleted)").unwrap_or(text));
        // The kernel retains the old public path after a legacy binary is
        // replaced/unlinked. Requiring that name plus the exact service role
        // distinguishes it from a user drop-in launching sleep or another app.
        actual == paths.bin_dir.join(binary)
            || verified_release_member(paths, actual, &format!("bin/{binary}"))
    }

    fn matches_process(&self, release: &Path, name: &str, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let binary = match name {
            "vellum.service" => "vellum",
            "vellum-shortcuts.service" => "vellum-ui",
            "vellum-tray.service" => "vellum-tray",
            _ => return false,
        };
        let Ok(expected) = release.join("bin").join(binary).canonicalize() else {
            return false;
        };
        let process = self.proc_root.join(pid.to_string());
        let Ok(actual) = fs::read_link(process.join("exe")).and_then(|p| p.canonicalize()) else {
            return false;
        };
        if actual != expected {
            return false;
        }
        match name {
            "vellum.service" => command_mode(&process).is_ok_and(|mode| mode == b"daemon"),
            "vellum-shortcuts.service" => {
                command_mode(&process).is_ok_and(|mode| mode == b"shortcuts-service")
            }
            _ => true,
        }
    }
    fn stop(&mut self, paths: &Paths, previous: &ServiceSnapshot) -> Result<(), String> {
        if !previous.reachable {
            return Err("用户服务管理器不可达，未确认服务停止".into());
        }
        let release = paths
            .root
            .join("current")
            .canonicalize()
            .map_err(|_| "缺少可验证的当前版本，未停止任何服务")?;
        let desired = ServiceSnapshot {
            reachable: true,
            busy: previous.busy,
            enabled: Vec::new(),
            active: Vec::new(),
        };
        self.apply(paths, &release, &desired)
    }
}
/// Verify only the relevant immutable member, without executing another binary
/// or probing systemd again. Both ordinary and adopted-legacy receipts record
/// generated units and copied binaries using this same schema.
fn verified_release_member(paths: &Paths, member: &Path, relative: &str) -> bool {
    (|| {
        let releases = paths.root.join("releases").canonicalize().ok()?;
        let tail = member.strip_prefix(&releases).ok()?;
        let id = tail.components().next()?.as_os_str().to_str()?;
        if !super::paths::safe_id(id) {
            return None;
        }
        let directory = releases.join(id);
        super::paths::check_chain(&directory).ok()?;
        if member != directory.join(relative) || member.canonicalize().ok()?.as_path() != member {
            return None;
        }
        let receipt: manifest::Installed =
            manifest::read_json(&directory.join("installed.json")).ok()?;
        if receipt.format != "vellum-installed-v1"
            || receipt.build_id != id
            || receipt.release_id != id
        {
            return None;
        }
        let records = if relative.starts_with("generated/") {
            &receipt.generated
        } else {
            &receipt.files
        };
        let entry = records.iter().find(|record| record.path == relative)?;
        if receipt
            .generated
            .iter()
            .chain(&receipt.files)
            .filter(|record| record.path == relative)
            .count()
            != 1
        {
            return None;
        }
        if entry.executable != relative.starts_with("bin/") {
            return None;
        }
        manifest::verify_record(&directory, entry).ok()?;
        Some(())
    })()
    .is_some()
}

fn owned_unit(paths: &Paths, release: &Path, name: &str) -> Result<PathBuf, String> {
    if !UNITS.contains(&name) {
        return Err("不受支持的服务名称".into());
    }
    let root = paths
        .root
        .join("releases")
        .canonicalize()
        .map_err(|_| "无法确认版本目录")?;
    let release_real = release.canonicalize().map_err(|_| "无法确认目标版本")?;
    if release_real.parent() != Some(root.as_path()) {
        return Err("目标服务不属于受管理版本".into());
    }
    let expected = release_real.join("generated/systemd/user").join(name);
    let canonical = expected
        .canonicalize()
        .map_err(|_| "目标版本缺少生成的服务资源")?;
    if canonical != expected || !fs::symlink_metadata(&canonical).is_ok_and(|m| m.is_file()) {
        return Err("目标服务资源路径无效".into());
    }
    Ok(expected)
}
fn verify_public(paths: &Paths, release: &Path, name: &str) -> Result<PathBuf, String> {
    let expected = owned_unit(paths, release, name)?;
    let public = paths.config_dir.join("systemd/user").join(name);
    if public.canonicalize().ok().as_ref() != Some(&expected) {
        return Err("服务公共入口不是目标版本，未重启服务".into());
    }
    Ok(expected)
}
fn verify_binding(
    paths: &Paths,
    release: &Path,
    name: &str,
    state: &UnitState,
) -> Result<(), String> {
    let expected = verify_public(paths, release, name)?;
    if state.missing || state.fragment.canonicalize().ok().as_ref() != Some(&expected) {
        return Err("当前用户管理器未加载目标服务资源；自定义目录需正确加载后再激活".into());
    }
    Ok(())
}
fn command_mode(process: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    fs::File::open(process.join("cmdline"))
        .and_then(|f| f.take(CMDLINE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| "无法确认Vellum进程用途")?;
    if bytes.len() as u64 > CMDLINE_BYTES {
        return Err("进程参数超过安全上限".into());
    }
    Ok(bytes.split(|b| *b == 0).nth(1).unwrap_or_default().to_vec())
}
#[derive(Default)]
struct ProcActivity {
    busy: bool,
    daemon_pids: Vec<u32>,
}
fn proc_busy(proc_root: &Path, uid: u32, deadline: Instant) -> Result<bool, String> {
    Ok(proc_activity(proc_root, uid, deadline)?.busy)
}
fn proc_activity(proc_root: &Path, uid: u32, deadline: Instant) -> Result<ProcActivity, String> {
    proc_activity_with_reader(proc_root, uid, deadline, |path| fs::read_link(path))
}

/// Linux comm is a small, non-argument process name. Do not inspect ptrace-gated
/// exe/cmdline for unrelated processes (browsers and credential helpers commonly
/// disallow that access even when owned by this same UID).
fn vellum_comm(process: &Path) -> bool {
    let Ok(file) = fs::File::open(process.join("comm")) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(32);
    if file.take(65).read_to_end(&mut bytes).is_err() || bytes.len() > 64 {
        return false;
    }
    let name = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    matches!(
        name,
        b"vellum" | b"vellum-ui" | b"vellumctl" | b"vellum-tray"
    )
}

fn proc_activity_with_reader(
    proc_root: &Path,
    uid: u32,
    deadline: Instant,
    mut read_exe: impl FnMut(&Path) -> io::Result<PathBuf>,
) -> Result<ProcActivity, String> {
    let mut activity = ProcActivity::default();
    let entries = fs::read_dir(proc_root).map_err(|_| "无法检查当前窗口与截图任务")?;
    for (count, entry) in entries.enumerate() {
        if count > 65_536 || Instant::now() >= deadline {
            return Err("进程检查超过安全预算，未激活服务".into());
        }
        let entry = entry.map_err(|_| "无法检查当前进程")?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process = entry.path();
        let Ok(metadata) = fs::metadata(&process) else {
            continue;
        };
        if metadata.uid() != uid || !vellum_comm(&process) {
            continue;
        }
        let executable = match read_exe(&process.join("exe")) {
            Ok(path) => path,
            // /proc/PID/exe disappears when a process exits (including zombies).
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err("无法确认Vellum候选进程身份，未激活服务".into()),
        };
        let name = executable.file_name().unwrap_or_default().to_string_lossy();
        let name = name.strip_suffix(" (deleted)").unwrap_or(&name);
        if name == "vellum-ui" {
            let mode = command_mode(&process)?;
            if !matches!(
                mode.as_slice(),
                b"shortcuts-service" | b"shortcuts-control" | b"--build-info-json"
            ) {
                activity.busy = true;
                return Ok(activity);
            }
        } else if matches!(name, "vellum" | "vellumctl") {
            let mode = command_mode(&process)?;
            if name == "vellum" && mode == b"daemon" {
                activity.daemon_pids.push(pid);
            }
            if matches!(
                mode.as_slice(),
                b"region"
                    | b"long"
                    | b"pin-last"
                    | b"panel"
                    | b"view"
                    | b"ocr"
                    | b"translate"
                    | b"debug-capture"
            ) {
                activity.busy = true;
                return Ok(activity);
            }
        }
    }
    Ok(activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        directory: PathBuf,
        paths: Paths,
        release: PathBuf,
        proc_root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = loop {
                let path = std::env::temp_dir().join(format!(
                    "vellum service fixture {} {}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("fixture directory: {error}"),
                }
            };
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let paths = Paths {
                root: directory.join("managed"),
                bin_dir: directory.join("bin"),
                config_dir: directory.join("custom config"),
                data_dir: directory.join("data"),
            };
            let release = paths.root.join("releases/test-build");
            let proc_root = directory.join("proc");
            fs::create_dir_all(release.join("generated/systemd/user")).unwrap();
            fs::create_dir_all(release.join("bin")).unwrap();
            fs::create_dir_all(paths.config_dir.join("systemd/user")).unwrap();
            fs::create_dir_all(&proc_root).unwrap();
            symlink("releases/test-build", paths.root.join("current")).unwrap();
            for binary in ["vellum", "vellum-tray", "vellum-ui"] {
                let path = release.join("bin").join(binary);
                fs::write(&path, b"fake binary bytes").unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            for name in UNITS {
                let unit = release.join("generated/systemd/user").join(name);
                fs::write(&unit, b"fake generated unit, never executed").unwrap();
                symlink(&unit, paths.config_dir.join("systemd/user").join(name)).unwrap();
            }
            write_receipt(&release, false);
            let fixture = Self {
                directory,
                paths,
                release,
                proc_root,
            };
            fixture.process(101, "vellum", "daemon");
            fixture.process(102, "vellum-ui", "shortcuts-service");
            fixture.process(103, "vellum-tray", "");
            fixture
        }
        fn process(&self, pid: u32, binary: &str, mode: &str) {
            let process = self.proc_root.join(pid.to_string());
            fs::create_dir(&process).unwrap();
            symlink(self.release.join("bin").join(binary), process.join("exe")).unwrap();
            fs::write(process.join("comm"), format!("{binary}\n")).unwrap();
            let bytes = [binary.as_bytes(), &[0], mode.as_bytes(), &[0]].concat();
            fs::write(process.join("cmdline"), bytes).unwrap();
        }
        fn adapter(&self) -> Adapter<Fake> {
            let mut states = BTreeMap::new();
            for (index, name) in UNITS.into_iter().enumerate() {
                states.insert(
                    name.to_string(),
                    UnitState {
                        fragment: self.paths.config_dir.join("systemd/user").join(name),
                        active: false,
                        enabled: false,
                        transition: false,
                        missing: false,
                        main_pid: 101 + index as u32,
                    },
                );
            }
            Adapter {
                runner: Fake {
                    reachable: true,
                    states,
                    calls: Vec::new(),
                    delete_link_on_disable: false,
                },
                proc_root: self.proc_root.clone(),
                uid: unsafe { libc::geteuid() },
                timeout: Duration::from_millis(150),
                ipc_busy: |_| false,
            }
        }
        fn desired(&self) -> ServiceSnapshot {
            ServiceSnapshot {
                reachable: true,
                busy: false,
                enabled: UNITS.iter().map(|s| s.to_string()).collect(),
                active: UNITS.iter().map(|s| s.to_string()).collect(),
            }
        }
        fn script(&self, body: &str) -> PathBuf {
            let script = self.directory.join(format!(
                "fake-command-{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            script
        }
    }
    // Fixtures are small and retained on disk to avoid deleting any externally
    // replaced test path. They never contain user settings or actual images.
    struct Fake {
        reachable: bool,
        states: BTreeMap<String, UnitState>,
        calls: Vec<Vec<String>>,
        delete_link_on_disable: bool,
    }
    impl Runner for Fake {
        fn run(&mut self, args: &[OsString], deadline: Instant) -> Result<CommandOutput, String> {
            if Instant::now() >= deadline {
                return Err("服务操作超过总时限".into());
            }
            let args: Vec<String> = args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            self.calls.push(args.clone());
            assert_eq!(&args[..3], ["--user", "--no-pager", "--no-ask-password"]);
            if args.iter().any(|a| a == "--property=Version") {
                return Ok(CommandOutput {
                    success: self.reachable,
                    stdout: b"fake-manager".to_vec(),
                });
            }
            if args[3] == "daemon-reload" {
                return Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                });
            }
            let name = args.last().unwrap();
            let state = self.states.get_mut(name).unwrap();
            match args[3].as_str() {
                "show" => {
                    return Ok(CommandOutput { success: true, stdout: format!("LoadState={}\nFragmentPath={}\nActiveState={}\nUnitFileState={}\nMainPID={}\n", if state.missing { "not-found" } else { "loaded" }, state.fragment.display(), if state.active { "active" } else { "inactive" }, if state.enabled { "enabled" } else { "disabled" }, state.main_pid).into_bytes() });
                }
                "enable" => state.enabled = true,
                "disable" => {
                    state.enabled = false;
                    if self.delete_link_on_disable {
                        fs::remove_file(&state.fragment).unwrap();
                    }
                }
                "restart" => state.active = true,
                "stop" => state.active = false,
                _ => panic!("unexpected fake operation"),
            }
            Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
            })
        }
    }
    fn write_receipt(directory: &Path, legacy: bool) {
        let id = directory.file_name().unwrap().to_str().unwrap().to_string();
        let receipt = manifest::Installed {
            format: "vellum-installed-v1".into(),
            build_id: id.clone(),
            release_id: id,
            legacy,
            files: ["vellum", "vellum-ui", "vellum-tray"]
                .iter()
                .map(|name| manifest::record(directory, &format!("bin/{name}")).unwrap())
                .collect(),
            generated: UNITS
                .iter()
                .map(|name| {
                    manifest::record(directory, &format!("generated/systemd/user/{name}")).unwrap()
                })
                .collect(),
        };
        fs::write(
            directory.join("installed.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    fn has_mutations(fake: &Fake) -> bool {
        fake.calls
            .iter()
            .any(|c| matches!(c[3].as_str(), "enable" | "disable" | "restart" | "stop"))
    }

    #[test]
    fn service_proc_skips_unrelated_unreadable_exe_without_reading_arguments() {
        let fixture = Fixture::new();
        let proc_root = fixture.directory.join("unrelated proc");
        let process = proc_root.join("900");
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), b"protected-browser\n").unwrap();
        // Neither an executable link nor cmdline is readable in this fake entry.
        // The injected reader proves we never even attempt the protected lookup.
        let activity = proc_activity_with_reader(
            &proc_root,
            unsafe { libc::geteuid() },
            Instant::now() + Duration::from_secs(1),
            |_| panic!("unrelated exe must not be read"),
        )
        .unwrap();
        assert!(!activity.busy && activity.daemon_pids.is_empty());
    }

    #[test]
    fn service_proc_confirmed_vellum_candidate_still_fails_closed_if_exe_is_unreadable() {
        let fixture = Fixture::new();
        let proc_root = fixture.directory.join("candidate proc");
        let process = proc_root.join("901");
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), b"vellum-ui\n").unwrap();
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidInput] {
            let error = proc_activity_with_reader(
                &proc_root,
                unsafe { libc::geteuid() },
                Instant::now() + Duration::from_secs(1),
                |_| Err(kind.into()),
            )
            .err()
            .unwrap();
            assert!(error.contains("候选进程"));
        }
        let exited = proc_activity_with_reader(
            &proc_root,
            unsafe { libc::geteuid() },
            Instant::now() + Duration::from_secs(1),
            |_| Err(io::ErrorKind::NotFound.into()),
        )
        .unwrap();
        assert!(!exited.busy && exited.daemon_pids.is_empty());
    }

    #[test]
    fn service_adapter_distinguishes_unreachable_from_absent_units() {
        let fixture = Fixture::new();
        // No daemon is running in this no-manager/no-units fixture.
        fs::rename(
            fixture.proc_root.join("101"),
            fixture.proc_root.join("retired-daemon"),
        )
        .unwrap();
        let mut adapter = fixture.adapter();
        adapter.runner.reachable = false;
        let snapshot = adapter.snapshot(&fixture.paths).unwrap();
        assert!(!snapshot.reachable && !snapshot.busy);
        assert_eq!(adapter.runner.calls.len(), 1);
        let mut adapter = fixture.adapter();
        for state in adapter.runner.states.values_mut() {
            state.missing = true;
            state.fragment = PathBuf::new();
            state.main_pid = 0;
        }
        let snapshot = adapter.snapshot(&fixture.paths).unwrap();
        assert!(snapshot.reachable && !snapshot.busy);
        assert!(snapshot.enabled.is_empty() && snapshot.active.is_empty());
        let mut fresh = fixture.paths.clone();
        fresh.config_dir = fixture.directory.join("fresh missing public units");
        let missing = adapter.snapshot(&fresh).unwrap();
        assert!(missing.reachable && !missing.busy);
    }
    #[test]
    fn service_adapter_interrupted_switch_accepts_receipted_old_units_only_in_same_root() {
        for legacy in [false, true] {
            let fixture = Fixture::new();
            write_receipt(&fixture.release, legacy);
            let next = fixture.paths.root.join("releases/next-build");
            fs::create_dir_all(next.join("bin")).unwrap();
            fs::create_dir_all(next.join("generated/systemd/user")).unwrap();
            for name in ["vellum", "vellum-ui", "vellum-tray"] {
                fs::copy(
                    fixture.release.join("bin").join(name),
                    next.join("bin").join(name),
                )
                .unwrap();
            }
            for name in UNITS {
                fs::copy(
                    fixture.release.join("generated/systemd/user").join(name),
                    next.join("generated/systemd/user").join(name),
                )
                .unwrap();
                let public = fixture.paths.config_dir.join("systemd/user").join(name);
                fs::rename(&public, public.with_extension("old-link")).unwrap();
                symlink(next.join("generated/systemd/user").join(name), public).unwrap();
            }
            write_receipt(&next, false);
            fs::rename(
                fixture.paths.root.join("current"),
                fixture.directory.join("old-current-link"),
            )
            .unwrap();
            symlink("releases/next-build", fixture.paths.root.join("current")).unwrap();
            let mut adapter = fixture.adapter();
            for (name, state) in &mut adapter.runner.states {
                state.fragment = fixture.release.join("generated/systemd/user").join(name);
                state.active = true;
            }
            let snapshot = adapter.snapshot(&fixture.paths).unwrap();
            assert!(snapshot.reachable && !snapshot.busy);
            assert_eq!(snapshot.active.len(), 3);
            let mut other_root = fixture.paths.clone();
            other_root.root = fixture.directory.join("other-install-root");
            assert!(adapter.snapshot(&other_root).is_err());
            fs::write(
                fixture
                    .release
                    .join("generated/systemd/user/vellum.service"),
                b"tampered old unit",
            )
            .unwrap();
            assert!(adapter.snapshot(&fixture.paths).is_err());
            assert!(!has_mutations(&adapter.runner));
        }
    }

    #[test]
    fn service_adapter_active_process_rejects_foreign_or_wrong_role_but_accepts_deleted_public_bin()
    {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        adapter
            .runner
            .states
            .get_mut("vellum.service")
            .unwrap()
            .active = true;
        assert!(!adapter.snapshot(&fixture.paths).unwrap().busy);
        let process = fixture.proc_root.join("101");
        let executable = process.join("exe");
        fs::rename(&executable, process.join("owned-exe")).unwrap();
        let foreign = fixture.directory.join("sleep");
        fs::write(&foreign, b"foreign non-Vellum program").unwrap();
        symlink(&foreign, &executable).unwrap();
        fs::write(
            process.join("cmdline"),
            [b"sleep".as_slice(), &[0], b"60", &[0]].concat(),
        )
        .unwrap();
        assert!(adapter.snapshot(&fixture.paths).is_err());
        fs::rename(&executable, process.join("foreign-exe")).unwrap();
        symlink(
            format!(
                "{} (deleted)",
                fixture.paths.bin_dir.join("vellum").display()
            ),
            &executable,
        )
        .unwrap();
        fs::write(
            process.join("cmdline"),
            [b"vellum".as_slice(), &[0], b"daemon", &[0]].concat(),
        )
        .unwrap();
        assert!(!adapter.snapshot(&fixture.paths).unwrap().busy);
        fs::rename(&executable, process.join("deleted-exe")).unwrap();
        symlink(fixture.release.join("bin/vellum"), &executable).unwrap();
        fs::write(
            process.join("cmdline"),
            [b"vellum".as_slice(), &[0], b"panel", &[0]].concat(),
        )
        .unwrap();
        assert!(adapter.snapshot(&fixture.paths).is_err());
        assert!(!has_mutations(&adapter.runner));
    }

    #[test]
    fn service_adapter_snapshot_rejects_foreign_loaded_unit_before_activation() {
        let fixture = Fixture::new();
        let global = fixture.directory.join("global units/vellum.service");
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(&global, b"foreign service must remain untouched").unwrap();
        for missing_public in [true, false] {
            let mut paths = fixture.paths.clone();
            if missing_public {
                paths.config_dir = fixture.directory.join("fresh config without units");
            }
            let mut adapter = fixture.adapter();
            adapter
                .runner
                .states
                .get_mut("vellum.service")
                .unwrap()
                .fragment = global.clone();
            let error = adapter.snapshot(&paths).unwrap_err();
            assert!(error.contains("同名服务"));
            assert!(!has_mutations(&adapter.runner));
            assert!(adapter.runner.calls.iter().all(|args| args[3] == "show"));
            assert_eq!(
                fs::read(&global).unwrap(),
                b"foreign service must remain untouched"
            );
        }
    }

    #[test]
    fn service_adapter_snapshot_accepts_own_legacy_regular_unit() {
        let fixture = Fixture::new();
        let public = fixture.paths.config_dir.join("systemd/user/vellum.service");
        // Replace only this test fixture's symlink with an old-style regular unit.
        fs::rename(&public, fixture.directory.join("saved fixture unit link")).unwrap();
        fs::write(&public, b"legacy user unit").unwrap();
        let mut adapter = fixture.adapter();
        let snapshot = adapter.snapshot(&fixture.paths).unwrap();
        assert!(snapshot.reachable && !snapshot.busy);
        assert!(!has_mutations(&adapter.runner));
    }

    #[test]
    fn service_adapter_tracked_idle_daemon_is_safe_but_untracked_idle_daemon_defers() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        // PID 101 is the known vellum.service MainPID. The shortcut daemon also
        // exists in fake /proc and must not be mistaken for a user's window.
        let tracked = adapter.snapshot(&fixture.paths).unwrap();
        assert!(tracked.reachable && !tracked.busy);
        fixture.process(201, "vellum", "daemon");
        let untracked = adapter.snapshot(&fixture.paths).unwrap();
        assert!(untracked.busy);
        assert!(
            adapter
                .apply(&fixture.paths, &fixture.release, &untracked)
                .is_err()
        );
        assert!(!has_mutations(&adapter.runner));
    }

    #[test]
    fn service_adapter_idle_daemon_without_manager_or_known_mainpid_defers() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        adapter.runner.reachable = false;
        let unreachable = adapter.snapshot(&fixture.paths).unwrap();
        assert!(!unreachable.reachable && unreachable.busy);
        adapter.runner.reachable = true;
        adapter
            .runner
            .states
            .get_mut("vellum.service")
            .unwrap()
            .main_pid = 0;
        assert!(adapter.snapshot(&fixture.paths).unwrap().busy);
        assert!(!has_mutations(&adapter.runner));
    }

    #[test]
    fn service_adapter_empty_snapshot_never_guesses_first_install_activation() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        let desired = ServiceSnapshot {
            reachable: true,
            ..ServiceSnapshot::default()
        };
        adapter
            .apply(&fixture.paths, &fixture.release, &desired)
            .unwrap();
        assert!(!has_mutations(&adapter.runner));
    }

    #[test]
    fn service_adapter_preserves_exact_enabled_and_active_sets() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        for state in adapter.runner.states.values_mut() {
            state.active = true;
            state.enabled = true;
        }
        let desired = ServiceSnapshot {
            reachable: true,
            busy: false,
            enabled: vec!["vellum.service".into()],
            active: vec!["vellum.service".into()],
        };
        adapter
            .apply(&fixture.paths, &fixture.release, &desired)
            .unwrap();
        for (name, state) in &adapter.runner.states {
            assert_eq!(state.active, name == "vellum.service");
            assert_eq!(state.enabled, name == "vellum.service");
        }
        assert_eq!(
            adapter
                .runner
                .calls
                .iter()
                .filter(|c| c[3] == "restart")
                .count(),
            1
        );
    }
    #[test]
    fn service_adapter_rejects_other_loaded_fragment_before_any_restart() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        let other = fixture.directory.join("unit from another config directory");
        fs::write(&other, b"not owned").unwrap();
        adapter
            .runner
            .states
            .get_mut("vellum-tray.service")
            .unwrap()
            .fragment = other;
        let error = adapter
            .apply(&fixture.paths, &fixture.release, &fixture.desired())
            .unwrap_err();
        assert!(error.contains("未加载目标服务资源"));
        assert!(!has_mutations(&adapter.runner));
    }
    #[test]
    fn service_adapter_gui_from_any_version_blocks_but_shortcut_daemon_does_not() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        assert!(!adapter.snapshot(&fixture.paths).unwrap().busy);
        let old_binary = fixture.directory.join("older version/bin/vellum-ui");
        fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
        fs::write(&old_binary, b"older fake UI").unwrap();
        let process = fixture.proc_root.join("200");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("comm"), b"vellum-ui\n").unwrap();
        symlink(&old_binary, process.join("exe")).unwrap();
        fs::write(
            process.join("cmdline"),
            [b"vellum-ui".as_slice(), &[0], b"panel", &[0]].concat(),
        )
        .unwrap();
        assert!(adapter.snapshot(&fixture.paths).unwrap().busy);
        assert!(
            adapter
                .apply(&fixture.paths, &fixture.release, &fixture.desired())
                .is_err()
        );
        assert!(!has_mutations(&adapter.runner));
    }
    #[test]
    fn service_adapter_rechecks_ipc_busy_without_stopping_processes() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        adapter.ipc_busy = |_| true;
        assert!(adapter.snapshot(&fixture.paths).unwrap().busy);
        assert!(adapter.stop(&fixture.paths, &fixture.desired()).is_err());
        assert!(!has_mutations(&adapter.runner));
    }
    #[test]
    fn service_adapter_restores_owned_link_removed_by_systemctl_disable() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        adapter.runner.delete_link_on_disable = true;
        for state in adapter.runner.states.values_mut() {
            state.enabled = true;
        }
        let empty = ServiceSnapshot {
            reachable: true,
            ..ServiceSnapshot::default()
        };
        adapter
            .apply(&fixture.paths, &fixture.release, &empty)
            .unwrap();
        for name in UNITS {
            assert_eq!(
                fixture
                    .paths
                    .config_dir
                    .join("systemd/user")
                    .join(name)
                    .canonicalize()
                    .unwrap(),
                fixture.release.join("generated/systemd/user").join(name)
            );
        }
    }
    #[test]
    fn service_adapter_wrong_mainpid_executable_never_reports_ready() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        adapter
            .runner
            .states
            .get_mut("vellum.service")
            .unwrap()
            .main_pid = 103; // tray, not daemon
        let error = adapter
            .apply(&fixture.paths, &fixture.release, &fixture.desired())
            .unwrap_err();
        assert!(error.contains("时限"));
    }
    #[test]
    fn service_adapter_rejects_unknown_snapshot_unit_and_unowned_stop() {
        let fixture = Fixture::new();
        let mut adapter = fixture.adapter();
        let mut desired = fixture.desired();
        desired.enabled.push("another-user.service".into());
        assert!(
            adapter
                .apply(&fixture.paths, &fixture.release, &desired)
                .is_err()
        );
        let mut paths = fixture.paths.clone();
        paths.root = fixture.directory.join("unknown-root");
        assert!(adapter.stop(&paths, &fixture.desired()).is_err());
        assert!(!has_mutations(&adapter.runner));
    }
    #[test]
    fn service_commands_have_deadline_and_byte_limits_without_raw_diagnostics() {
        let fixture = Fixture::new();
        let normal = fixture.script("printf fake-version");
        let output =
            bounded_command(&normal, &[], Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, b"fake-version");
        let failure = fixture.script("echo synthetic-private-endpoint >&2; exit 2");
        assert!(
            !bounded_command(&failure, &[], Instant::now() + Duration::from_secs(1))
                .unwrap()
                .success
        );
        let flood = fixture.script("/usr/bin/head -c 70000 /dev/zero");
        let error =
            bounded_command(&flood, &[], Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert!(error.contains("上限"));
        let hang = fixture.script("/usr/bin/sleep 60 & wait");
        let start = Instant::now();
        assert!(
            bounded_command(&hang, &[], start + Duration::from_millis(50))
                .unwrap_err()
                .contains("时限")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
