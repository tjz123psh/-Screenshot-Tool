//! Release-manager regressions use private temporary roots and fake services only.
//! No test invokes the desktop's real systemd instance or installed Vellum.
use super::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
struct Sandbox {
    dir: PathBuf,
    dev: u64,
    ino: u64,
    sentinels: Vec<(PathBuf, Vec<u8>)>,
}
impl Sandbox {
    fn new() -> Self {
        let dir = loop {
            let candidate = std::env::temp_dir().join(format!(
                "vellum release test {} {}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create isolated root: {e}"),
            }
        };
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let meta = fs::symlink_metadata(&dir).unwrap();
        let mut sandbox = Self {
            dir,
            dev: meta.dev(),
            ino: meta.ino(),
            sentinels: Vec::new(),
        };
        for (path, bytes) in [
            (
                "home/.config/vellum/config.toml",
                b"[user]\nvalue='preserve new settings'\n".as_slice(),
            ),
            (
                "home/.config/vellum/tray.json",
                b"{\"copy\":false}".as_slice(),
            ),
            (
                "home/.config/niri/config.kdl",
                b"// existing niri bindings, no installer ownership\n".as_slice(),
            ),
            (
                "home/.config/hypr/hyprland.conf",
                b"# existing compositor settings\n".as_slice(),
            ),
            (
                "home/Pictures/Screenshots/private.png",
                b"user screenshot sentinel".as_slice(),
            ),
            (
                "home/.local/state/vellum/recovery/private.png",
                b"recoverable image sentinel".as_slice(),
            ),
        ] {
            let path = sandbox.dir.join(path);
            put(&path, bytes, false);
            sandbox.sentinels.push((path, bytes.to_vec()));
        }
        sandbox
    }
    fn root(&self) -> PathBuf {
        self.dir.join("home/.local/lib/vellum")
    }
    fn bin(&self) -> PathBuf {
        self.dir.join("home/.local/bin")
    }
    fn config(&self) -> PathBuf {
        self.dir.join("home/.config")
    }
    fn data(&self) -> PathBuf {
        self.dir.join("home/.local/share")
    }
    fn assert_user_data_untouched(&self) {
        for (path, bytes) in &self.sentinels {
            assert_eq!(
                &fs::read(path).unwrap(),
                bytes,
                "user data changed: {}",
                path.display()
            );
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        // Delete only the exact directory this fixture created, never an injected
        // replacement, user HOME or a computed wildcard target.
        if let Ok(meta) = fs::symlink_metadata(&self.dir)
            && meta.is_dir()
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}
fn put(path: &Path, bytes: &[u8], executable: bool) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )
    .unwrap();
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn fixture_build(id: &str) -> serde_json::Value {
    serde_json::json!({
        "format":"vellum-build-info-v1", "version":"0.2.0", "build_id":id,
        "source_commit":"0000000000000000000000000000000000000000", "source_dirty":false,
        "source_digest":"0000000000000000000000000000000000000000000000000000000000000000",
        "target":"x86_64-unknown-linux-gnu", "rustc":"rustc synthetic-fixture", "profile":"release",
        "config_schema":1, "ipc_schema":1
    })
}
fn make_bundle(sandbox: &Sandbox, id: &str) -> PathBuf {
    let bundle = sandbox.dir.join(format!("bundle {id}"));
    fs::create_dir(&bundle).unwrap();
    let build = fixture_build(id);
    let encoded = serde_json::to_string(&build).unwrap();
    // Do not mimic an installer or daemon: executing anything except the
    // metadata probe fails closed without touching the filesystem.
    let script = format!(
        "#!/bin/sh\n# vellum-build-info-v1\n[ \"$#\" = 1 ] && [ \"$1\" = --build-info-json ] || exit 87\nprintf '%s\\n' '{encoded}'\n"
    );
    let mut files = Vec::new();
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        let path = format!("bin/{name}");
        put(&bundle.join(&path), script.as_bytes(), true);
        files.push(serde_json::json!({"path":path,"size":script.len(),"sha256":digest(script.as_bytes()),"executable":true}));
    }
    for (path, contents) in [
        (
            "resources/applications/ai.vellum.desktop",
            "[Desktop Entry]\nType=Application\nName=Vellum\nExec=@VELLUM_LAUNCHER@ area\n",
        ),
        (
            "resources/applications/ai.vellum-panel.desktop",
            "[Desktop Entry]\nType=Application\nName=Vellum Panel\nExec=@VELLUM_LAUNCHER@ panel\n",
        ),
        (
            "resources/autostart/ai.vellum-shortcuts.desktop",
            "[Desktop Entry]\nType=Application\nName=Vellum Shortcuts\nExec=@VELLUM_LAUNCHER@ shortcuts serve\n",
        ),
        (
            "resources/dbus-1/services/ai.vellum.Shortcuts.service",
            "[D-BUS Service]\nName=ai.vellum.Shortcuts\nExec=@VELLUM_LAUNCHER@ shortcuts serve\n",
        ),
        (
            "resources/systemd/user/vellum.service",
            "[Service]\nExecStart=@VELLUM_LAUNCHER@ daemon\n",
        ),
        (
            "resources/systemd/user/vellum-tray.service",
            "[Service]\nExecStart=@VELLUM_TRAY@\n",
        ),
        (
            "resources/systemd/user/vellum-shortcuts.service",
            "[Service]\nExecStart=@VELLUM_LAUNCHER@ shortcuts serve\n",
        ),
        (
            "resources/icons/hicolor/scalable/apps/ai.vellum.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
        ),
        (
            "resources/icons/hicolor/scalable/status/ai.vellum-symbolic.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
        ),
        (
            "resources/icons/hicolor/scalable/status/ai.vellum-recording-symbolic.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
        ),
        (
            "resources/icons/hicolor/scalable/status/ai.vellum-warning-symbolic.svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
        ),
        ("LICENSE", "Synthetic test license\n"),
        (
            "README.md",
            "Synthetic release, not installed on the desktop\n",
        ),
        ("install.sh", "#!/bin/sh\nexit 87\n"),
    ] {
        let executable = path == "install.sh";
        put(&bundle.join(path), contents.as_bytes(), executable);
        files.push(serde_json::json!({"path":path,"size":contents.len(),"sha256":digest(contents.as_bytes()),"executable":executable}));
    }
    put(&bundle.join("manifest.json"),&serde_json::to_vec_pretty(&serde_json::json!({"format":"vellum-release-v1","release_id":id,"version":"0.2.0","build":build,"files":files})).unwrap(),false);
    bundle
}
/// Resource donor for pre-build-info installations, never passed as a modern bundle.
fn make_legacy_fixture(sandbox: &Sandbox) -> PathBuf {
    let directory = make_bundle(sandbox, "legacy-original");
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        let script = format!("#!/bin/sh\n# retired legacy fixture {name}\nexit 87\n");
        put(&directory.join("bin").join(name), script.as_bytes(), true);
    }
    directory
}
fn staged_directories(sandbox: &Sandbox) -> Vec<PathBuf> {
    fs::read_dir(sandbox.root())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("stage-"))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn an_install_after_an_interrupted_preparation_heals_itself() {
    let sandbox = Sandbox::new();
    let bundle = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    let mut hooks = TraceHooks {
        fail: Some("file-copied"),
        ..TraceHooks::default()
    };
    assert!(
        execute(
            Operation::Install {
                bundle: bundle.clone(),
                adopt_legacy: false,
            },
            &sandbox.paths(),
            &mut services,
            &mut hooks,
        )
        .is_err(),
        "the injected failure did not interrupt the install"
    );
    assert!(sandbox.root().join("journal.json").exists());
    let staged = staged_directories(&sandbox);
    assert!(
        !staged.is_empty(),
        "the interrupted preparation left no staging copy to clean"
    );
    // The copy is cut short before the manifest lands, which is exactly the
    // shape a real interrupted preparation has.

    // A transaction that only prepared a candidate touched no public file, so
    // the next install rolls it back instead of demanding a manual repair.
    let report = install(&sandbox, &bundle, &mut services);
    assert_eq!(report.state, "ready");
    assert!(!sandbox.root().join("journal.json").exists());
    assert!(
        staged_directories(&sandbox).is_empty(),
        "the abandoned staging copy survived the recovery"
    );
}

#[test]
fn uninstall_removes_staging_copies_from_earlier_failed_attempts() {
    let sandbox = Sandbox::new();
    let bundle = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    install(&sandbox, &bundle, &mut services);

    // Mimic a copy left behind by a failed attempt: it looks like our own
    // staging area, but no journal refers to it any more.
    let orphan = sandbox.root().join("stage-4242-deadbeef");
    fs::create_dir(&orphan).unwrap();
    fs::copy(bundle.join("manifest.json"), orphan.join("manifest.json")).unwrap();

    let report = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(report.state, "uninstalled");
    assert!(
        !orphan.exists(),
        "an abandoned staging copy survived an uninstall"
    );
}

fn change_manifest(bundle: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = bundle.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    put(&path, &serde_json::to_vec_pretty(&manifest).unwrap(), false);
}

/// Snapshot all immutable members without following injected symlinks.
fn tree_snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, (u32, String)> {
    fn walk(root: &Path, at: &Path, out: &mut std::collections::BTreeMap<PathBuf, (u32, String)>) {
        let metadata = fs::symlink_metadata(at).unwrap();
        let relative = at.strip_prefix(root).unwrap().to_owned();
        let kind = if metadata.file_type().is_symlink() {
            format!("link:{}", fs::read_link(at).unwrap().display())
        } else if metadata.is_file() {
            format!("file:{}", digest(&fs::read(at).unwrap()))
        } else {
            "directory".to_owned()
        };
        out.insert(relative, (metadata.mode() & 0o777, kind));
        if metadata.is_dir() {
            for entry in fs::read_dir(at).unwrap() {
                walk(root, &entry.unwrap().path(), out);
            }
        }
    }
    let mut snapshot = std::collections::BTreeMap::new();
    walk(root, root, &mut snapshot);
    snapshot
}

impl Sandbox {
    fn paths(&self) -> Paths {
        Paths {
            root: self.root(),
            bin_dir: self.bin(),
            config_dir: self.config(),
            data_dir: self.data(),
        }
    }
}
#[derive(Default)]
struct FakeServices {
    unreachable: bool,
    busy: bool,
    fail_activation: bool,
    activations: usize,
    restorations: usize,
    stops: usize,
    policy: Option<(Vec<String>, Vec<String>)>,
    applied: Vec<ServiceSnapshot>,
}
impl Services for FakeServices {
    fn set_enabled(&mut self, _: &Paths, _: &Path, enabled: &[String]) -> Result<(), String> {
        if self.fail_activation {
            return Err("synthetic startup failure".into());
        }
        let active = self
            .policy
            .as_ref()
            .map(|p| p.1.clone())
            .unwrap_or_else(|| vec!["vellum.service".into()]);
        self.policy = Some((enabled.to_vec(), active));
        Ok(())
    }
    fn snapshot(&mut self, _: &Paths) -> Result<ServiceSnapshot, String> {
        Ok(ServiceSnapshot {
            reachable: !self.unreachable,
            busy: self.busy,
            enabled: self
                .policy
                .as_ref()
                .map(|p| p.0.clone())
                .unwrap_or_else(|| vec!["vellum.service".into()]),
            active: self
                .policy
                .as_ref()
                .map(|p| p.1.clone())
                .unwrap_or_else(|| vec!["vellum.service".into()]),
        })
    }
    fn activate(&mut self, _: &Paths, _: &Path, previous: &ServiceSnapshot) -> Result<(), String> {
        self.activations += 1;
        self.applied.push(previous.clone());
        if self.fail_activation {
            Err("synthetic activation failure".into())
        } else {
            Ok(())
        }
    }
    fn restore(&mut self, _: &Paths, _: Option<&Path>, _: &ServiceSnapshot) -> Result<(), String> {
        self.restorations += 1;
        Ok(())
    }
    fn stop(&mut self, _: &Paths, _: &ServiceSnapshot) -> Result<(), String> {
        self.stops += 1;
        Ok(())
    }
}
#[derive(Default)]
struct TraceHooks {
    seen: Vec<String>,
    fail: Option<&'static str>,
    fail_at_occurrence: usize,
    in_use: Vec<PathBuf>,
}
impl Hooks for TraceHooks {
    fn version_in_use(&mut self, directory: &Path) -> Result<bool, String> {
        Ok(self.in_use.iter().any(|p| p == directory))
    }
    fn checkpoint(&mut self, stage: &str) -> Result<(), String> {
        self.seen.push(stage.to_owned());
        if self.fail == Some(stage)
            && (self.fail_at_occurrence == 0
                || self.seen.iter().filter(|s| s.as_str() == stage).count()
                    == self.fail_at_occurrence)
        {
            Err(format!("injected {stage}"))
        } else {
            Ok(())
        }
    }
}
fn install(sandbox: &Sandbox, bundle: &Path, services: &mut FakeServices) -> Report {
    execute(
        Operation::Install {
            bundle: bundle.to_owned(),
            adopt_legacy: false,
        },
        &sandbox.paths(),
        services,
        &mut TraceHooks::default(),
    )
    .unwrap()
}
fn current_id(sandbox: &Sandbox) -> Option<String> {
    fs::read_link(sandbox.root().join("current"))
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
}
fn populate_legacy_resources(sandbox: &Sandbox, bundle: &Path) {
    for entry in sandbox.paths().entries() {
        if let Some(relative) = entry.relative.strip_prefix("generated/") {
            put(
                &entry.public,
                &fs::read(bundle.join("resources").join(relative)).unwrap(),
                false,
            );
        }
    }
}
#[test]
fn pending_fresh_install_can_uninstall_without_repair_or_activation() {
    for no_activate in [false, true] {
        let sandbox = Sandbox::new();
        let bundle = make_bundle(&sandbox, "release-a");
        let mut services = FakeServices {
            unreachable: !no_activate,
            ..Default::default()
        };
        let pending = if no_activate {
            assert_eq!(
                run_with(
                    ReleaseCommand::Install {
                        bundle,
                        adopt_legacy: false,
                        no_activate: true,
                        paths: cli_paths(&sandbox)
                    },
                    &mut services,
                    &mut TraceHooks::default()
                )
                .unwrap(),
                0
            );
            execute(
                Operation::Status,
                &sandbox.paths(),
                &mut services,
                &mut TraceHooks::default(),
            )
            .unwrap()
        } else {
            install(&sandbox, &bundle, &mut services)
        };
        assert_eq!(pending.state, "installed-pending-activation");
        assert!(
            execute(
                Operation::Uninstall { confirmed: false },
                &sandbox.paths(),
                &mut services,
                &mut TraceHooks::default()
            )
            .is_err()
        );
        assert!(
            old_release(&sandbox).exists(),
            "unconfirmed cancellation removed pending code"
        );
        assert_eq!(services.activations, 0);
        let removed = execute(
            Operation::Uninstall { confirmed: true },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(removed.state, "uninstalled");
        assert_eq!(
            services.activations, 0,
            "uninstall implicitly activated a pending release"
        );
        assert!(current_id(&sandbox).is_none());
        assert!(!old_release(&sandbox).exists());
        for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
            assert!(fs::symlink_metadata(sandbox.bin().join(name)).is_err());
        }
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn pending_upgrade_can_uninstall_without_first_finishing_the_upgrade() {
    for deferred_by_busy in [false, true] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        services.unreachable = !deferred_by_busy;
        services.busy = deferred_by_busy;
        assert_eq!(
            install(&sandbox, &b, &mut services).state,
            "installed-pending-activation"
        );
        let activations = services.activations;
        // Closing the mock UI permits removal, but does not repair/activate B.
        services.busy = false;
        let removed = execute(
            Operation::Uninstall { confirmed: true },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(removed.state, "uninstalled");
        assert_eq!(services.activations, activations);
        assert!(current_id(&sandbox).is_none());
        assert!(!old_release(&sandbox).exists());
        assert!(!sandbox.root().join("releases/release-b").exists());
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn modern_regular_deployment_is_not_misclassified_as_legacy_or_probed() {
    let sandbox = Sandbox::new();
    let modern = make_bundle(&sandbox, "release-a");
    let next = make_bundle(&sandbox, "release-b");
    populate_legacy_resources(&sandbox, &modern);
    let probe = sandbox.dir.join("must-not-probe-modern-regular");
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        let script = format!(
            "#!/bin/sh\n# vellum-build-info-v1\nprintf executed > '{}'\nexit 87\n",
            probe.display()
        );
        put(&sandbox.bin().join(name), script.as_bytes(), true);
    }
    let before = tree_snapshot(&sandbox.bin());
    let resources: Vec<_> = sandbox
        .paths()
        .entries()
        .into_iter()
        .filter(|e| e.relative.starts_with("generated/"))
        .map(|e| {
            let bytes = fs::read(&e.public).unwrap();
            (e.public, bytes)
        })
        .collect();
    let mut services = FakeServices::default();
    assert!(
        execute(
            Operation::Install {
                bundle: next,
                adopt_legacy: true
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(tree_snapshot(&sandbox.bin()), before);
    assert!(
        !probe.exists(),
        "modern regular entry was executed before refusal"
    );
    assert!(current_id(&sandbox).is_none());
    assert_eq!(services.activations, 0);
    assert_eq!(services.stops, 0);
    for (path, bytes) in resources {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    sandbox.assert_user_data_untouched();
}

#[test]
fn pending_legacy_adoption_cancellation_keeps_original_regular_entries() {
    let sandbox = Sandbox::new();
    let legacy = make_legacy_fixture(&sandbox);
    let next = make_bundle(&sandbox, "release-b");
    populate_legacy_resources(&sandbox, &legacy);
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        put(
            &sandbox.bin().join(name),
            &fs::read(legacy.join("bin").join(name)).unwrap(),
            true,
        );
    }
    let originals: Vec<_> = sandbox
        .paths()
        .entries()
        .into_iter()
        .map(|entry| {
            let bytes = fs::read(&entry.public).unwrap();
            (entry.public, bytes)
        })
        .collect();
    let mut services = FakeServices {
        busy: true,
        ..Default::default()
    };
    let pending = execute(
        Operation::Install {
            bundle: next,
            adopt_legacy: true,
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(pending.state, "installed-pending-activation");
    assert!(current_id(&sandbox).is_none());
    let removed = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(removed.state, "cancelled-pending-install");
    assert!(removed.current.is_none());
    assert!(removed.candidate.is_none());
    assert_eq!(services.activations, 0);
    assert_eq!(services.stops, 0);
    assert_eq!(services.restorations, 0);
    assert!(!sandbox.root().join("releases/release-b").exists());
    for (path, bytes) in originals {
        assert!(
            removed.preserved.contains(&path),
            "legacy entry not reported as preserved"
        );
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "cancellation silently adopted a legacy entry"
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    sandbox.assert_user_data_untouched();
}

fn default_path_options() -> PathOptions {
    PathOptions {
        root: None,
        bin_dir: None,
        config_dir: None,
        data_dir: None,
        json: true,
    }
}
#[test]
fn path_defaults_follow_managed_executable_state_not_another_home_instance() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    install(&sandbox, &a, &mut FakeServices::default());
    let executable = old_release(&sandbox).join("bin/vellum");
    let foreign_home = sandbox.dir.join("different home");
    let foreign_config = foreign_home.join("custom config");
    let foreign_data = foreign_home.join("custom data");
    let resolved = default_path_options()
        .resolve(
            Some(&foreign_home),
            Some(&executable),
            Some(&foreign_config),
            Some(&foreign_data),
        )
        .unwrap();
    assert_eq!(resolved, sandbox.paths());
    assert!(
        !foreign_home.exists(),
        "resolution created another install instance"
    );
    sandbox.assert_user_data_untouched();
}
#[test]
fn path_explicit_root_and_individual_overrides_take_precedence_over_executable() {
    let source = Sandbox::new();
    let a = make_bundle(&source, "release-a");
    install(&source, &a, &mut FakeServices::default());
    let selected = Sandbox::new();
    let b = make_bundle(&selected, "release-b");
    install(&selected, &b, &mut FakeServices::default());
    let executable = old_release(&source).join("bin/vellum");
    let manual_bin = selected.dir.join("explicit bin path");
    let mut options = default_path_options();
    options.root = Some(selected.root());
    options.bin_dir = Some(manual_bin.clone());
    let resolved = options
        .resolve(None, Some(&executable), None, None)
        .unwrap();
    let mut expected = selected.paths();
    expected.bin_dir = manual_bin;
    assert_eq!(resolved, expected);
    source.assert_user_data_untouched();
    selected.assert_user_data_untouched();
}
#[test]
fn path_all_explicit_options_work_without_home_or_managed_metadata() {
    let sandbox = Sandbox::new();
    let options = cli_paths(&sandbox);
    let plausible_but_uninstalled = sandbox.root().join("releases/missing/bin/vellum");
    assert_eq!(
        options
            .resolve(None, Some(&plausible_but_uninstalled), None, None)
            .unwrap(),
        sandbox.paths()
    );
    assert_eq!(
        options.resolve(None, None, None, None).unwrap(),
        sandbox.paths()
    );
    assert!(!sandbox.root().exists());
    sandbox.assert_user_data_untouched();
}
#[test]
fn path_damaged_marker_still_recovers_from_state_but_missing_state_never_falls_back() {
    for damage in [
        "marker-corrupt",
        "marker-missing",
        "state-corrupt",
        "both-missing",
    ] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        install(&sandbox, &a, &mut FakeServices::default());
        let executable = old_release(&sandbox).join("bin/vellum");
        let marker = old_release(&sandbox).join("installed.json");
        let state = sandbox.root().join("state.json");
        match damage {
            "marker-corrupt" => put(&marker, b"{broken receipt", false),
            "marker-missing" => fs::remove_file(&marker).unwrap(),
            "state-corrupt" => put(&state, b"{broken state", false),
            "both-missing" => {
                fs::remove_file(&state).unwrap();
                fs::remove_file(&marker).unwrap();
            }
            _ => unreachable!(),
        }
        let foreign_home = sandbox.dir.join("must not create foreign home");
        let result =
            default_path_options().resolve(Some(&foreign_home), Some(&executable), None, None);
        if damage.starts_with("marker-") {
            assert_eq!(result.unwrap(), sandbox.paths());
        } else {
            assert!(
                result.is_err(),
                "{damage} silently switched to a HOME-derived instance"
            );
        }
        assert!(!foreign_home.exists());
        sandbox.assert_user_data_untouched();
    }
}

fn cli_paths(sandbox: &Sandbox) -> PathOptions {
    PathOptions {
        root: Some(sandbox.root()),
        bin_dir: Some(sandbox.bin()),
        config_dir: Some(sandbox.config()),
        data_dir: Some(sandbox.data()),
        json: true,
    }
}

#[test]
fn cli_requested_rollback_success_returns_zero() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    install(&sandbox, &b, &mut services);
    let code = run_with(
        ReleaseCommand::Rollback(cli_paths(&sandbox)),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn cli_failed_install_with_successful_compensation_returns_one() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    services.fail_activation = true;
    let code = run_with(
        ReleaseCommand::Install {
            bundle: b,
            adopt_legacy: false,
            no_activate: false,
            paths: cli_paths(&sandbox),
        },
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(code, 1);
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn cli_failed_rollback_target_with_compensation_also_returns_one() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    install(&sandbox, &b, &mut services);
    services.fail_activation = true;
    let code = run_with(
        ReleaseCommand::Rollback(cli_paths(&sandbox)),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(
        code, 1,
        "operation name rollback cannot override failed activation"
    );
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-b"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn cli_repair_success_returns_zero_for_forward_or_compensation_recovery() {
    for phase in ["journal-prepared", "rollback-restored"] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        services.fail_activation = phase == "rollback-restored";
        let mut hooks = TraceHooks {
            fail: Some(phase),
            ..Default::default()
        };
        assert!(
            execute(
                Operation::Install {
                    bundle: b,
                    adopt_legacy: false
                },
                &sandbox.paths(),
                &mut services,
                &mut hooks
            )
            .is_err()
        );
        services.fail_activation = false;
        let code = run_with(
            ReleaseCommand::Repair(cli_paths(&sandbox)),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(code, 0, "repair completed successfully at {phase}");
        assert_current_complete(&sandbox);
        assert_eq!(
            current_id(&sandbox).as_deref(),
            Some(if phase == "rollback-restored" {
                "release-a"
            } else {
                "release-b"
            })
        );
        sandbox.assert_user_data_untouched();
    }
}

fn old_release(sandbox: &Sandbox) -> PathBuf {
    sandbox.root().join("releases/release-a")
}
fn assert_current_complete(sandbox: &Sandbox) {
    let id = current_id(sandbox).expect("current must identify a complete release");
    let directory = sandbox.root().join("releases").join(&id);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap();
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        let path = format!("bin/{name}");
        let bytes = fs::read(directory.join(&path)).unwrap();
        let entry = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap();
        assert_eq!(entry["sha256"], digest(&bytes));
        assert_eq!(entry["size"], bytes.len());
    }
}

#[test]
fn absent_installation_status_and_repair_do_not_touch_user_data() {
    let sandbox = Sandbox::new();
    let mut services = FakeServices::default();
    for op in [Operation::Status, Operation::Repair] {
        let report = execute(
            op,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(report.state, "not-installed");
        assert!(
            !sandbox.root().exists(),
            "read-only empty status/repair created install root"
        );
    }
    assert_eq!(services.activations, 0);
    sandbox.assert_user_data_untouched();
}

#[test]
fn install_upgrade_and_rollback_preserve_complete_versions_and_user_data() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    assert_eq!(install(&sandbox, &a, &mut services).state, "ready");
    let unit =
        fs::read_to_string(old_release(&sandbox).join("generated/systemd/user/vellum.service"))
            .unwrap();
    let expected = old_release(&sandbox).join("bin/vellum");
    assert!(
        unit.contains(expected.to_str().unwrap()),
        "service must bind the immutable version"
    );
    assert!(
        !unit.contains("/current/"),
        "service follows mutable current pointer"
    );
    assert!(
        unit.lines().any(|line| line.starts_with("ExecStart=\"")),
        "space-containing executable path is not quoted"
    );
    let original = tree_snapshot(&old_release(&sandbox));
    assert_eq!(install(&sandbox, &b, &mut services).state, "ready");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-b"));
    assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
    let result = execute(
        Operation::Rollback,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(result.state, "rolled-back");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
    assert!(sandbox.root().join("releases/release-b").is_dir());
    sandbox.assert_user_data_untouched();
}
#[test]
fn rollback_never_restores_stale_user_configuration() {
    let mut sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    install(&sandbox, &b, &mut services);
    let config = sandbox.config().join("vellum/config.toml");
    let newer = b"[user]\nvalue='edited after upgrade'\n";
    put(&config, newer, false);
    sandbox
        .sentinels
        .iter_mut()
        .find(|(p, _)| p == &config)
        .unwrap()
        .1 = newer.to_vec();
    execute(
        Operation::Rollback,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    sandbox.assert_user_data_untouched();
}
#[test]
fn unreachable_service_manager_is_pending_not_ready_and_repair_activates() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices {
        unreachable: true,
        ..Default::default()
    };
    let report = install(&sandbox, &a, &mut services);
    assert_eq!(report.state, "installed-pending-activation");
    assert_eq!(services.activations, 0);
    let status = execute(
        Operation::Status,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(status.state, "installed-pending-activation");
    services.unreachable = false;
    let repaired = execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(repaired.state, "ready");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert!(services.activations > 0);
    sandbox.assert_user_data_untouched();
}
#[test]
fn busy_gui_defers_upgrade_without_stopping_running_services() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    services.busy = true;
    let before = (services.activations, services.restorations, services.stops);
    let report = install(&sandbox, &b, &mut services);
    assert_ne!(report.state, "ready");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert_eq!(
        (services.activations, services.restorations, services.stops),
        before
    );
    let deferred = execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(deferred.state, "installed-pending-activation");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    services.busy = false;
    let report = execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(report.state, "ready");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-b"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn startup_query_is_read_only_and_toggle_preserves_active_work() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    services.busy = true; // An open settings window must not block startup-only edits.
    let before = (services.activations, services.stops, services.restorations);
    let query = execute(
        Operation::Autostart { enabled: None },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(query.state, "autostart-mixed");
    assert!(!sandbox.config().join("vellum/autostart.json").exists());
    for (enabled, status) in [(false, "autostart-disabled"), (true, "autostart-enabled")] {
        let report = execute(
            Operation::Autostart {
                enabled: Some(enabled),
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(report.state, status);
        assert_eq!(
            vellum_core::autostart::load_at(&sandbox.config()).unwrap(),
            Some(enabled)
        );
        assert_eq!(services.policy.as_ref().unwrap().1, vec!["vellum.service"]);
    }
    assert_eq!(
        (services.activations, services.stops, services.restorations),
        before
    );
    sandbox.assert_user_data_untouched();
}

#[test]
fn startup_opt_out_survives_fresh_install_upgrade_and_deferred_repair() {
    let sandbox = Sandbox::new();
    vellum_core::autostart::save_at(&sandbox.config(), false).unwrap();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let applied = services.applied.last().unwrap();
    assert!(applied.enabled.is_empty() && applied.active.is_empty());
    assert!(
        !sandbox
            .config()
            .join("autostart/ai.vellum-shortcuts.desktop")
            .exists()
    );
    services.busy = true;
    install(&sandbox, &b, &mut services);
    services.busy = false;
    services.policy = Some((vec!["vellum-tray.service".into()], Vec::new()));
    execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    let applied = services.applied.last().unwrap();
    assert!(applied.enabled.is_empty() && applied.active.is_empty());
    assert_eq!(
        vellum_core::autostart::load_at(&sandbox.config()).unwrap(),
        Some(false)
    );
    execute(
        Operation::Rollback,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert!(
        !sandbox
            .config()
            .join("autostart/ai.vellum-shortcuts.desktop")
            .exists()
    );
    assert!(services.applied.last().unwrap().enabled.is_empty());
    sandbox.assert_user_data_untouched();
}

#[test]
fn upgrade_does_not_restore_a_deleted_desktop_startup_entry() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let desktop = sandbox
        .config()
        .join("autostart/ai.vellum-shortcuts.desktop");
    assert!(desktop.is_symlink());
    fs::remove_file(&desktop).unwrap(); // Only this fixture's owned login entry.
    services.policy = Some((Vec::new(), Vec::new()));
    install(&sandbox, &b, &mut services);
    assert!(!desktop.exists());
    assert!(services.applied.last().unwrap().enabled.is_empty());
    let query = execute(
        Operation::Autostart { enabled: None },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(query.state, "autostart-disabled");
    let report = execute(
        Operation::Autostart {
            enabled: Some(true),
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(report.state, "autostart-enabled");
    assert!(desktop.is_symlink());
    sandbox.assert_user_data_untouched();
}

#[test]
fn startup_failures_do_not_claim_success_or_forget_opt_out() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    services.unreachable = true;
    assert!(
        execute(
            Operation::Autostart {
                enabled: Some(false)
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert!(!sandbox.config().join("vellum/autostart.json").exists());
    services.unreachable = false;
    services.fail_activation = true;
    assert!(
        execute(
            Operation::Autostart {
                enabled: Some(false)
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(
        vellum_core::autostart::load_at(&sandbox.config()).unwrap(),
        Some(false)
    );
    services.fail_activation = false;
    let report = execute(
        Operation::Autostart {
            enabled: Some(false),
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(report.state, "autostart-disabled");
    sandbox.assert_user_data_untouched();
}

#[test]
fn deferred_repair_preserves_new_service_policy_instead_of_the_old_snapshot() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    services.busy = true;
    install(&sandbox, &b, &mut services);
    services.busy = false;
    services.policy = Some((vec!["vellum-tray.service".into()], Vec::new()));
    let result = execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(result.state, "ready");
    let applied = services.applied.last().expect("repair did not activate");
    assert_eq!(applied.enabled, vec!["vellum-tray.service"]);
    assert!(
        applied.active.is_empty(),
        "repair resurrected a service disabled during deferral"
    );
    sandbox.assert_user_data_untouched();
}

#[test]
fn busy_uninstall_defers_without_removing_entries_or_program_bytes() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let before = tree_snapshot(&old_release(&sandbox));
    let stops = services.stops;
    services.busy = true;
    let result = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(result.state, "installed-pending-activation");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert_eq!(tree_snapshot(&old_release(&sandbox)), before);
    assert!(sandbox.bin().join("vellumctl").exists());
    assert_eq!(services.stops, stops);
    sandbox.assert_user_data_untouched();
}

#[test]
fn uninstall_retains_a_complete_version_reported_in_use_without_real_proc_scanning() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    install(&sandbox, &b, &mut services);
    let original = tree_snapshot(&old_release(&sandbox));
    let mut hooks = TraceHooks {
        in_use: vec![old_release(&sandbox)],
        ..Default::default()
    };
    let result = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut hooks,
    )
    .unwrap();
    assert_eq!(result.state, "uninstalled");
    assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
    assert!(result.preserved.contains(&old_release(&sandbox)));
    assert!(!sandbox.root().join("releases/release-b").exists());
    sandbox.assert_user_data_untouched();
}

#[test]
fn uninstall_removes_verified_versions_but_retains_modified_and_unknown_members() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let c = make_bundle(&sandbox, "release-c");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    install(&sandbox, &b, &mut services);
    install(&sandbox, &c, &mut services);
    let altered = old_release(&sandbox);
    put(
        &altered.join("README.md"),
        b"user-modified release readme",
        false,
    );
    let unknown = sandbox.root().join("releases/release-b");
    put(
        &unknown.join("user-data.bin"),
        b"not in ownership manifest",
        false,
    );
    let before_altered = tree_snapshot(&altered);
    let before_unknown = tree_snapshot(&unknown);
    let result = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(result.state, "uninstalled");
    assert!(
        !sandbox.root().join("releases/release-c").exists(),
        "verified unused program bytes not removed"
    );
    assert_eq!(tree_snapshot(&altered), before_altered);
    assert_eq!(tree_snapshot(&unknown), before_unknown);
    assert!(result.preserved.contains(&altered));
    assert!(result.preserved.contains(&unknown));
    sandbox.assert_user_data_untouched();
}

#[test]
fn first_activation_failure_never_reports_ready_and_keeps_the_complete_candidate() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices {
        fail_activation: true,
        ..Default::default()
    };
    let result = execute(
        Operation::Install {
            bundle: a.clone(),
            adopt_legacy: false,
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    );
    assert!(!matches!(&result,Ok(report) if report.state=="ready"));
    assert_eq!(current_id(&sandbox), None);
    assert!(old_release(&sandbox).join("bin/vellum").is_file());
    services.fail_activation = false;
    execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(install(&sandbox, &a, &mut services).state, "ready");
    sandbox.assert_user_data_untouched();
}

#[test]
fn repair_refuses_tampered_journal_paths_without_touching_old_release() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let original = tree_snapshot(&old_release(&sandbox));
    let mut hooks = TraceHooks {
        fail: Some("journal-prepared"),
        ..Default::default()
    };
    assert!(
        execute(
            Operation::Install {
                bundle: b,
                adopt_legacy: false
            },
            &sandbox.paths(),
            &mut services,
            &mut hooks
        )
        .is_err()
    );
    let path = sandbox.root().join("journal.json");
    let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    journal["target"] = "../../.config".into();
    put(&path, &serde_json::to_vec_pretty(&journal).unwrap(), false);
    assert!(
        execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
    sandbox.assert_user_data_untouched();
}

#[test]
fn activation_failure_restores_previous_release_and_services() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let original = tree_snapshot(&old_release(&sandbox));
    services.fail_activation = true;
    let result = execute(
        Operation::Install {
            bundle: b,
            adopt_legacy: false,
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    );
    assert!(!matches!(&result,Ok(report) if report.state=="ready"));
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
    assert!(services.restorations > 0);
    sandbox.assert_user_data_untouched();
}
#[test]
fn invalid_bundles_never_change_current_or_existing_release() {
    for corruption in [
        "hash",
        "missing",
        "extra",
        "symlink",
        "traversal",
        "release-id",
        "mode",
        "metadata",
    ] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let binary = b.join("bin/vellum-ui");
        match corruption {
            "hash" => put(&binary, b"corrupt", true),
            "missing" => fs::remove_file(&binary).unwrap(),
            "extra" => put(&b.join("unlisted.txt"), b"unlisted", false),
            "symlink" => {
                fs::remove_file(&binary).unwrap();
                symlink(&sandbox.sentinels[0].0, &binary).unwrap();
            }
            "traversal" => change_manifest(&b, |m| m["files"][0]["path"] = "../escape".into()),
            "release-id" => change_manifest(&b, |m| m["release_id"] = "../../outside".into()),
            "mode" => fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap(),
            "metadata" => {
                let content = fs::read_to_string(&binary)
                    .unwrap()
                    .replace("release-b", "different-release");
                put(&binary, content.as_bytes(), true);
                change_manifest(&b, |m| {
                    let entry = m["files"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|f| f["path"] == "bin/vellum-ui")
                        .unwrap();
                    entry["size"] = content.len().into();
                    entry["sha256"] = digest(content.as_bytes()).into();
                });
            }
            _ => unreachable!(),
        }
        assert!(
            execute(
                Operation::Install {
                    bundle: b,
                    adopt_legacy: false
                },
                &sandbox.paths(),
                &mut services,
                &mut TraceHooks::default()
            )
            .is_err(),
            "accepted {corruption}"
        );
        assert_eq!(
            current_id(&sandbox).as_deref(),
            Some("release-a"),
            "changed current for {corruption}"
        );
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn mid_binary_and_resource_copy_failures_preserve_previous_release() {
    for nth in [2, 5, 12, 18] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let mut hooks = TraceHooks {
            fail: Some("file-copied"),
            fail_at_occurrence: nth,
            ..Default::default()
        };
        assert!(
            execute(
                Operation::Install {
                    bundle: b,
                    adopt_legacy: false
                },
                &sandbox.paths(),
                &mut services,
                &mut hooks
            )
            .is_err()
        );
        assert_eq!(
            hooks
                .seen
                .iter()
                .filter(|s| s.as_str() == "file-copied")
                .count(),
            nth
        );
        assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
        execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        sandbox.assert_user_data_untouched();
    }
}

#[test]
fn ordinary_stage_failures_leave_old_release_intact_and_can_be_repaired() {
    for phase in [
        "stage-created",
        "file-copied",
        "staged",
        "links-ready",
        "journal-prepared",
    ] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let mut hooks = TraceHooks {
            fail: Some(phase),
            ..Default::default()
        };
        let result = execute(
            Operation::Install {
                bundle: b,
                adopt_legacy: false,
            },
            &sandbox.paths(),
            &mut services,
            &mut hooks,
        );
        assert!(
            hooks.seen.iter().any(|s| s == phase),
            "unreached fault {phase}"
        );
        assert!(result.is_err(), "fault not reported at {phase}");
        assert_eq!(
            current_id(&sandbox).as_deref(),
            Some("release-a"),
            "precommit fault switched {phase}"
        );
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn postcommit_faults_repair_to_a_complete_release_without_rewriting_old_bytes() {
    for phase in ["current-switched", "activated", "committed"] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let mut hooks = TraceHooks {
            fail: Some(phase),
            ..Default::default()
        };
        let _ = execute(
            Operation::Install {
                bundle: b,
                adopt_legacy: false,
            },
            &sandbox.paths(),
            &mut services,
            &mut hooks,
        );
        assert!(hooks.seen.iter().any(|s| s == phase), "unreached {phase}");
        execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_current_complete(&sandbox);
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn existing_unmanaged_version_directory_is_not_overwritten() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let collision = sandbox.root().join("releases/release-b/unmanaged.txt");
    put(&collision, b"unmanaged release directory", false);
    assert!(
        execute(
            Operation::Install {
                bundle: b,
                adopt_legacy: false
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(fs::read(collision).unwrap(), b"unmanaged release directory");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn upgrade_refuses_user_replaced_entrypoints_instead_of_adopting_them_silently() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let b = make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let replacement = sandbox.bin().join("vellum-ui");
    fs::remove_file(&replacement).unwrap();
    put(&replacement, b"my custom launcher", true);
    assert!(
        execute(
            Operation::Install {
                bundle: b,
                adopt_legacy: false
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(fs::read(replacement).unwrap(), b"my custom launcher");
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    sandbox.assert_user_data_untouched();
}
#[test]
fn symlink_install_root_is_rejected_without_following_it_into_user_configuration() {
    let sandbox = Sandbox::new();
    let bundle = make_bundle(&sandbox, "release-a");
    let link = sandbox.dir.join("root link");
    symlink(sandbox.config(), &link).unwrap();
    let mut paths = sandbox.paths();
    paths.root = link;
    assert!(
        execute(
            Operation::Install {
                bundle,
                adopt_legacy: false
            },
            &paths,
            &mut FakeServices::default(),
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert!(!sandbox.config().join("releases").exists());
    sandbox.assert_user_data_untouched();
}
#[test]
fn legacy_adoption_requires_explicit_permission_and_retains_a_rollback_version() {
    let sandbox = Sandbox::new();
    let legacy = make_legacy_fixture(&sandbox);
    populate_legacy_resources(&sandbox, &legacy);
    let next = make_bundle(&sandbox, "release-b");
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        put(
            &sandbox.bin().join(name),
            &fs::read(legacy.join("bin").join(name)).unwrap(),
            true,
        );
    }
    let original: Vec<_> = ["vellum", "vellumctl", "vellum-ui", "vellum-tray"]
        .into_iter()
        .map(|name| (name, fs::read(sandbox.bin().join(name)).unwrap()))
        .collect();
    let mut services = FakeServices::default();
    assert!(
        execute(
            Operation::Install {
                bundle: next.clone(),
                adopt_legacy: false
            },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    for (name, bytes) in &original {
        assert_eq!(&fs::read(sandbox.bin().join(name)).unwrap(), bytes);
    }
    let report = execute(
        Operation::Install {
            bundle: next,
            adopt_legacy: true,
        },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(report.state, "ready");
    assert!(
        report.previous.is_some(),
        "adoption did not retain a rollback release"
    );
    execute(
        Operation::Rollback,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    for (name, bytes) in original {
        assert_eq!(fs::read(sandbox.bin().join(name)).unwrap(), bytes);
    }
    sandbox.assert_user_data_untouched();
}
#[test]
fn uninstall_requires_confirmation_and_keeps_user_replacements() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    assert!(
        execute(
            Operation::Uninstall { confirmed: false },
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default()
        )
        .is_err()
    );
    assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
    let replacement = sandbox.bin().join("vellum-ui");
    let unknown = sandbox.root().join("unknown-user-file.txt");
    put(&unknown, b"unowned install root data", false);
    let unrelated = sandbox.bin().join("user-command");
    put(&unrelated, b"unrelated command", true);
    fs::remove_file(&replacement).unwrap();
    put(&replacement, b"user-owned replacement", true);
    let report = execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_ne!(report.state, "ready");
    assert_eq!(fs::read(&replacement).unwrap(), b"user-owned replacement");
    assert!(report.preserved.iter().any(|p| p == &replacement));
    assert_eq!(fs::read(unknown).unwrap(), b"unowned install root data");
    assert_eq!(fs::read(unrelated).unwrap(), b"unrelated command");
    assert!(
        !old_release(&sandbox).exists(),
        "uninstall retained a complete unused nonlegacy release"
    );
    assert!(fs::symlink_metadata(sandbox.bin().join("vellumctl")).is_err());
    sandbox.assert_user_data_untouched();
}

const HELPER_TEST: &str = "release::tests::process_fault_helper";
struct ProcessHook {
    stage: String,
    mode: String,
    root: PathBuf,
}
impl Hooks for ProcessHook {
    fn version_in_use(&mut self, _: &Path) -> Result<bool, String> {
        Ok(false)
    }
    fn checkpoint(&mut self, stage: &str) -> Result<(), String> {
        if stage != self.stage {
            return Ok(());
        }
        put(
            &self.root.join("checkpoint-reached"),
            stage.as_bytes(),
            false,
        );
        if self.mode == "kill" {
            // This is the private test subprocess, never an installed process.
            // SAFETY: raising SIGKILL targets only this helper's own PID.
            unsafe {
                libc::raise(libc::SIGKILL);
            }
            unreachable!("SIGKILL unexpectedly returned");
        }
        if self.mode == "hold" {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !self.root.join("release-lock").exists() {
                if std::time::Instant::now() >= deadline {
                    return Err("test hold deadline".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        Ok(())
    }
}
#[test]
fn process_fault_helper() {
    let Some(root) = std::env::var_os("VELLUM_RELEASE_FIXTURE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    // The helper refuses arbitrary paths even when run manually as a test.
    assert!(root.is_absolute());
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("vellum release test ")
    );
    assert_eq!(
        fs::read(root.join("fixture-owner")).unwrap(),
        b"vellum-release-test-v1"
    );
    let paths = Paths {
        root: root.join("home/.local/lib/vellum"),
        bin_dir: root.join("home/.local/bin"),
        config_dir: root.join("home/.config"),
        data_dir: root.join("home/.local/share"),
    };
    let mode = std::env::var("VELLUM_RELEASE_FIXTURE_MODE").unwrap();
    let mut services = FakeServices::default();
    if mode == "reject-bundle" {
        let result = execute(
            Operation::Install {
                bundle: root.join("bundle release-b"),
                adopt_legacy: false,
            },
            &paths,
            &mut services,
            &mut TraceHooks::default(),
        );
        assert!(
            result.is_err(),
            "malformed special/oversized file was accepted"
        );
        return;
    }
    if mode == "contend" {
        let result = execute(
            Operation::Repair,
            &paths,
            &mut services,
            &mut TraceHooks::default(),
        );
        assert!(result.is_err(), "concurrent transaction was not excluded");
        return;
    }
    let mut hooks = ProcessHook {
        stage: std::env::var("VELLUM_RELEASE_FIXTURE_STAGE").unwrap(),
        mode,
        root: root.clone(),
    };
    let operation = match hooks.stage.as_str() {
        "rollback-restored" => {
            services.fail_activation = true;
            Operation::Install {
                bundle: root.join("bundle release-b"),
                adopt_legacy: false,
            }
        }
        "uninstall-prepared"
        | "uninstall-links-removed"
        | "program-file-removed"
        | "program-version-removed" => Operation::Uninstall { confirmed: true },
        stage => Operation::Install {
            bundle: root.join("bundle release-b"),
            adopt_legacy: stage == "adoption-backed-up",
        },
    };
    execute(operation, &paths, &mut services, &mut hooks).unwrap();
}
struct TestChild(std::process::Child);
impl std::ops::Deref for TestChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for TestChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for TestChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn spawn_helper(sandbox: &Sandbox, mode: &str, stage: &str) -> TestChild {
    put(
        &sandbox.dir.join("fixture-owner"),
        b"vellum-release-test-v1",
        false,
    );
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER_TEST, "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", sandbox.dir.join("home"))
        .env("XDG_CONFIG_HOME", sandbox.config())
        .env("XDG_DATA_HOME", sandbox.data())
        .env("XDG_STATE_HOME", sandbox.dir.join("home/.local/state"))
        .env("XDG_RUNTIME_DIR", sandbox.dir.join("home/runtime"))
        .env("VELLUM_RELEASE_FIXTURE_ROOT", &sandbox.dir)
        .env("VELLUM_RELEASE_FIXTURE_MODE", mode)
        .env("VELLUM_RELEASE_FIXTURE_STAGE", stage)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    TestChild(child)
}
fn wait_helper(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("private release helper exceeded deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
#[test]
fn killed_transaction_helpers_repair_without_losing_old_or_user_data() {
    use std::os::unix::process::ExitStatusExt;
    for phase in [
        "stage-created",
        "file-copied",
        "staged",
        "links-ready",
        "journal-prepared",
        "current-switched",
        "activated",
        "committed",
    ] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let mut helper = spawn_helper(&sandbox, "kill", phase);
        let status = wait_helper(&mut helper);
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "stage {phase} not killed: {status}"
        );
        assert_eq!(
            fs::read(sandbox.dir.join("checkpoint-reached")).unwrap(),
            phase.as_bytes()
        );
        if matches!(phase, "journal-prepared" | "current-switched" | "activated") {
            let report = execute(
                Operation::Status,
                &sandbox.paths(),
                &mut services,
                &mut TraceHooks::default(),
            )
            .unwrap();
            assert_eq!(
                report.state, "interrupted/needs-repair",
                "missing interrupted state at {phase}"
            );
        }
        execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        assert_current_complete(&sandbox);
        assert_eq!(
            tree_snapshot(&old_release(&sandbox)),
            original,
            "old release changed after {phase}"
        );
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn killed_rollback_and_uninstall_helpers_leave_repairable_consistent_state() {
    use std::os::unix::process::ExitStatusExt;
    for phase in [
        "rollback-restored",
        "uninstall-prepared",
        "uninstall-links-removed",
        "program-file-removed",
        "program-version-removed",
    ] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        if phase != "rollback-restored" {
            install(&sandbox, &b, &mut services);
        }
        let original = tree_snapshot(&old_release(&sandbox));
        let mut helper = spawn_helper(&sandbox, "kill", phase);
        assert_eq!(
            wait_helper(&mut helper).signal(),
            Some(libc::SIGKILL),
            "stage {phase} was not reached"
        );
        let report = execute(
            Operation::Repair,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        if current_id(&sandbox).is_some() {
            assert_current_complete(&sandbox);
        } else {
            assert!(
                matches!(report.state.as_str(), "uninstalled" | "not-installed"),
                "unexpected empty state {}",
                report.state
            );
            assert!(fs::symlink_metadata(sandbox.bin().join("vellumctl")).is_err());
        }
        if phase == "rollback-restored" {
            assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        } else {
            assert_eq!(report.state, "uninstalled");
            assert!(!old_release(&sandbox).exists());
            assert!(!sandbox.root().join("releases/release-b").exists());
        }
        sandbox.assert_user_data_untouched();
    }
}
#[test]
fn killed_legacy_backup_never_destroys_original_regular_binaries() {
    use std::os::unix::process::ExitStatusExt;
    let sandbox = Sandbox::new();
    let legacy = make_legacy_fixture(&sandbox);
    populate_legacy_resources(&sandbox, &legacy);
    make_bundle(&sandbox, "release-b");
    let mut originals = Vec::new();
    for name in ["vellum", "vellumctl", "vellum-ui", "vellum-tray"] {
        let bytes = fs::read(legacy.join("bin").join(name)).unwrap();
        put(&sandbox.bin().join(name), &bytes, true);
        originals.push((name, bytes));
    }
    let mut helper = spawn_helper(&sandbox, "kill", "adoption-backed-up");
    assert_eq!(wait_helper(&mut helper).signal(), Some(libc::SIGKILL));
    let mut services = FakeServices::default();
    execute(
        Operation::Repair,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    // Completing the new release is legitimate if the previous full legacy
    // version remains recoverable. Verify the actual rollback, not a chosen
    // recovery direction, then verify uninstall retains that backup too.
    let status = execute(
        Operation::Status,
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    let legacy_id = if status.current.as_deref() == Some("release-b") {
        let previous = status.previous.expect("legacy rollback copy was lost");
        execute(
            Operation::Rollback,
            &sandbox.paths(),
            &mut services,
            &mut TraceHooks::default(),
        )
        .unwrap();
        previous
    } else {
        current_id(&sandbox).expect("legacy release absent after repair")
    };
    for (name, bytes) in &originals {
        assert_eq!(&fs::read(sandbox.bin().join(name)).unwrap(), bytes);
    }
    let backup = sandbox.root().join("releases").join(legacy_id);
    let before = tree_snapshot(&backup);
    execute(
        Operation::Uninstall { confirmed: true },
        &sandbox.paths(),
        &mut services,
        &mut TraceHooks::default(),
    )
    .unwrap();
    assert_eq!(tree_snapshot(&backup), before);
    for (name, bytes) in originals {
        assert_eq!(fs::read(backup.join("bin").join(name)).unwrap(), bytes);
    }
    sandbox.assert_user_data_untouched();
}
#[test]
fn fifo_and_misdeclared_sparse_members_are_rejected_before_blocking_io() {
    use std::os::unix::ffi::OsStrExt;
    for bad in ["manifest-fifo", "binary-fifo", "sparse-binary"] {
        let sandbox = Sandbox::new();
        let a = make_bundle(&sandbox, "release-a");
        let b = make_bundle(&sandbox, "release-b");
        let mut services = FakeServices::default();
        install(&sandbox, &a, &mut services);
        let original = tree_snapshot(&old_release(&sandbox));
        let target = b.join(if bad == "manifest-fifo" {
            "manifest.json"
        } else {
            "bin/vellum-ui"
        });
        assert!(target.is_absolute() && target.starts_with(&b));
        if bad == "sparse-binary" {
            let file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&target)
                .unwrap();
            file.set_len(2 * 1024 * 1024 * 1024).unwrap();
            assert!(
                file.metadata().unwrap().blocks() < 1024,
                "fixture must remain sparse"
            );
        } else {
            fs::remove_file(&target).unwrap();
            let name = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
            // SAFETY: create only the exact removed member inside this fixture.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        let started = std::time::Instant::now();
        let mut helper = spawn_helper(&sandbox, "reject-bundle", "");
        assert!(
            wait_helper(&mut helper).success(),
            "helper failed for {bad}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{bad} was read/hash-scanned before size/type rejection"
        );
        assert_eq!(current_id(&sandbox).as_deref(), Some("release-a"));
        assert_eq!(tree_snapshot(&old_release(&sandbox)), original);
        sandbox.assert_user_data_untouched();
    }
}

#[test]
fn concurrent_manager_is_locked_out_and_owner_can_finish() {
    let sandbox = Sandbox::new();
    let a = make_bundle(&sandbox, "release-a");
    make_bundle(&sandbox, "release-b");
    let mut services = FakeServices::default();
    install(&sandbox, &a, &mut services);
    let mut owner = spawn_helper(&sandbox, "hold", "journal-prepared");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sandbox.dir.join("checkpoint-reached").exists() {
        if owner.try_wait().unwrap().is_some() {
            panic!("lock owner exited before checkpoint");
        }
        if std::time::Instant::now() >= deadline {
            let _ = owner.kill();
            let _ = owner.wait();
            panic!("lock owner did not reach checkpoint");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut contender = spawn_helper(&sandbox, "contend", "");
    let status = wait_helper(&mut contender);
    put(&sandbox.dir.join("release-lock"), b"resume", false);
    let owner_status = wait_helper(&mut owner);
    assert!(
        status.success(),
        "second transaction must report lock contention"
    );
    assert!(owner_status.success());
    assert_current_complete(&sandbox);
    sandbox.assert_user_data_untouched();
}
