//! User-scoped, versioned release management. No command edits user preferences.
#[path = "release_manifest.rs"]
pub mod manifest;
#[path = "release_paths.rs"]
pub mod paths;
#[path = "release_services.rs"]
pub mod services;
#[path = "release_store.rs"]
mod store;

use clap::{Args, Subcommand};
pub use paths::Paths;
use serde::Serialize;
pub use services::{ServiceSnapshot, Services};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Operation {
    Install { bundle: PathBuf, adopt_legacy: bool },
    Status,
    Rollback,
    Repair,
    Uninstall { confirmed: bool },
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub success: bool,
    pub state: String,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub candidate: Option<String>,
    pub message: String,
    pub preserved: Vec<PathBuf>,
    pub repair_program: Option<PathBuf>,
    pub repair_args: Vec<String>,
}
pub trait Hooks {
    fn checkpoint(&mut self, _stage: &str) -> Result<(), String> {
        Ok(())
    }
    /// Override in isolated tests; production conservatively checks owned processes.
    fn version_in_use(&mut self, directory: &std::path::Path) -> Result<bool, String> {
        store::version_in_use(directory)
    }
}
pub struct NoHooks;
impl Hooks for NoHooks {}

#[derive(Args, Debug)]
pub struct PathOptions {
    #[arg(long)]
    pub root: Option<PathBuf>,
    #[arg(long)]
    pub bin_dir: Option<PathBuf>,
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}
impl PathOptions {
    fn paths(&self) -> Result<Paths, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let executable = std::env::current_exe().ok();
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        self.resolve(
            home.as_deref(),
            executable.as_deref(),
            config.as_deref(),
            data.as_deref(),
        )
    }
    /// Pure environment injection; only the chosen root's bounded state file is
    /// consulted. Missing/corrupt managed identity never redirects to HOME.
    pub fn resolve(
        &self,
        home: Option<&std::path::Path>,
        executable: Option<&std::path::Path>,
        config_home: Option<&std::path::Path>,
        data_home: Option<&std::path::Path>,
    ) -> Result<Paths, String> {
        if let (Some(root), Some(bin_dir), Some(config_dir), Some(data_dir)) =
            (&self.root, &self.bin_dir, &self.config_dir, &self.data_dir)
        {
            let paths = Paths {
                root: root.clone(),
                bin_dir: bin_dir.clone(),
                config_dir: config_dir.clone(),
                data_dir: data_dir.clone(),
            };
            paths.validate()?;
            return Ok(paths);
        }
        let inferred = executable.and_then(managed_execution_root);
        let root = self
            .root
            .clone()
            .or_else(|| inferred.clone())
            .or_else(|| home.map(|home| home.join(".local/lib/vellum")))
            .ok_or("无法确定安装根，请明确--root")?;
        paths::check_chain(&root)?;
        #[derive(serde::Deserialize)]
        struct StoredPaths {
            format: String,
            paths: Paths,
        }
        let recorded = match std::fs::symlink_metadata(root.join("state.json")) {
            Ok(_) => {
                let stored: StoredPaths = manifest::read_json(&root.join("state.json"))
                    .map_err(|_| "安装路径记录损坏，请明确提供同一实例的四个路径")?;
                if stored.format != "vellum-release-state-v1" || stored.paths.root != root {
                    return Err("安装路径记录属于另一实例，拒绝回落默认目录".into());
                }
                stored.paths.validate()?;
                Some(stored.paths)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("无法读取安装路径记录".into()),
        };
        if inferred.as_ref() == Some(&root) && recorded.is_none() {
            return Err("当前程序位于受管版本布局，但路径记录缺失；请提供四个明确路径，不会管理HOME下另一实例".into());
        }
        let paths = Paths {
            root,
            bin_dir: self
                .bin_dir
                .clone()
                .or_else(|| recorded.as_ref().map(|p| p.bin_dir.clone()))
                .or_else(|| home.map(|home| home.join(".local/bin")))
                .ok_or("需要--bin-dir")?,
            config_dir: self
                .config_dir
                .clone()
                .or_else(|| recorded.as_ref().map(|p| p.config_dir.clone()))
                .or_else(|| config_home.map(std::path::Path::to_path_buf))
                .or_else(|| home.map(|home| home.join(".config")))
                .ok_or("需要--config-dir")?,
            data_dir: self
                .data_dir
                .clone()
                .or_else(|| recorded.as_ref().map(|p| p.data_dir.clone()))
                .or_else(|| data_home.map(std::path::Path::to_path_buf))
                .or_else(|| home.map(|home| home.join(".local/share")))
                .ok_or("需要--data-dir")?,
        };
        paths.validate()?;
        Ok(paths)
    }
}
fn managed_execution_root(executable: &std::path::Path) -> Option<PathBuf> {
    if !executable.is_absolute() {
        return None;
    }
    let name = executable.file_name()?.to_str()?;
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    if !paths::BINARIES.contains(&name) {
        return None;
    }
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let version = bin.parent()?;
    let releases = version.parent()?;
    if releases.file_name()? != "releases" {
        return None;
    }
    Some(releases.parent()?.to_path_buf())
}

#[derive(Subcommand, Debug)]
pub enum ReleaseCommand {
    Install {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        adopt_legacy: bool,
        #[arg(long)]
        no_activate: bool,
        #[command(flatten)]
        paths: PathOptions,
    },
    Status(PathOptions),
    Rollback(PathOptions),
    Repair(PathOptions),
    Uninstall {
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        paths: PathOptions,
    },
}

pub fn execute(
    operation: Operation,
    paths: &Paths,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> Result<Report, String> {
    paths.validate()?;
    let repairing = matches!(operation, Operation::Repair);
    let mut report = store::execute(operation, paths, services, hooks)?;
    if repairing && report.state == "rolled-back" {
        report.success = true;
        report.message = format!("修复已完成：{}", report.message);
    }
    if matches!(
        report.state.as_str(),
        "installed-pending-activation" | "interrupted/needs-repair"
    ) {
        report.repair_args = vec![
            "release".into(),
            "repair".into(),
            "--root".into(),
            paths.root.to_string_lossy().into_owned(),
            "--bin-dir".into(),
            paths.bin_dir.to_string_lossy().into_owned(),
            "--config-dir".into(),
            paths.config_dir.to_string_lossy().into_owned(),
            "--data-dir".into(),
            paths.data_dir.to_string_lossy().into_owned(),
            "--json".into(),
        ];
        if let Some(id) = &report.candidate {
            let directory = paths.release(id)?;
            if manifest::Manifest::read(&directory).is_ok() {
                report.repair_program = Some(directory.join("bin/vellum"));
            }
        }
    }
    Ok(report)
}
pub fn run(command: ReleaseCommand) -> anyhow::Result<i32> {
    let mut real = services::RealServices::default();
    run_with(command, &mut real, &mut NoHooks)
}
pub fn exit_code(report: &Report) -> i32 {
    if !report.success {
        1
    } else if report.state == "interrupted/needs-repair" {
        2
    } else {
        0
    }
}
pub fn run_with(
    command: ReleaseCommand,
    services: &mut dyn Services,
    hooks: &mut dyn Hooks,
) -> anyhow::Result<i32> {
    let (operation, options, no_activate) = match command {
        ReleaseCommand::Install {
            bundle,
            adopt_legacy,
            no_activate,
            paths,
        } => (
            Operation::Install {
                bundle,
                adopt_legacy,
            },
            paths,
            no_activate,
        ),
        ReleaseCommand::Status(paths) => (Operation::Status, paths, false),
        ReleaseCommand::Rollback(paths) => (Operation::Rollback, paths, false),
        ReleaseCommand::Repair(paths) => (Operation::Repair, paths, false),
        ReleaseCommand::Uninstall { yes, paths } => {
            (Operation::Uninstall { confirmed: yes }, paths, false)
        }
    };
    let paths = options.paths().map_err(anyhow::Error::msg)?;
    let mut report = if no_activate {
        let mut deferred = DeferredServices(services);
        execute(operation, &paths, &mut deferred, hooks)
    } else {
        execute(operation, &paths, services, hooks)
    }
    .map_err(anyhow::Error::msg)?;
    if report.repair_program.is_none() && !report.repair_args.is_empty() {
        report.repair_program = std::env::current_exe().ok();
    }
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}: {}", report.state, report.message);
        if let Some(id) = &report.current {
            println!("当前版本：{id}");
        }
        if let Some(id) = &report.candidate {
            println!("待激活版本：{id}");
        }
        if let Some(program) = &report.repair_program {
            println!("可用恢复入口：{}", program.display());
            println!(
                "恢复参数(JSON数组)：{}",
                serde_json::to_string(&report.repair_args)?
            );
        }
        if !report.preserved.is_empty() {
            println!(
                "保留了 {} 个在用版本、备份或改动/非自有项；用户设置与图片未删除",
                report.preserved.len()
            );
        }
    }
    Ok(exit_code(&report))
}
struct DeferredServices<'a>(&'a mut dyn Services);
impl Services for DeferredServices<'_> {
    fn snapshot(&mut self, paths: &Paths) -> Result<ServiceSnapshot, String> {
        let mut state = self.0.snapshot(paths)?;
        state.reachable = false;
        Ok(state)
    }
    fn activate(
        &mut self,
        _: &Paths,
        _: &std::path::Path,
        _: &ServiceSnapshot,
    ) -> Result<(), String> {
        Err("激活被明确禁用".into())
    }
    fn restore(
        &mut self,
        _: &Paths,
        _: Option<&std::path::Path>,
        _: &ServiceSnapshot,
    ) -> Result<(), String> {
        Ok(())
    }
    fn stop(&mut self, _: &Paths, _: &ServiceSnapshot) -> Result<(), String> {
        Ok(())
    }
}
#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
