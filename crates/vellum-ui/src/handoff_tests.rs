use super::*;

#[test]
fn editable_original_never_enters_persistent_recovery_payloads() {
    use crate::annotate::{Stroke, Tool};
    let source = vellum_core::Rgb8::from_raw(8, 8, [201, 31, 117].repeat(64));
    let document = crate::document::Document::from_selection(
        source,
        vec![Stroke {
            tool: Tool::Cover,
            color: (1.0, 1.0, 1.0),
            width: 4.0,
            points: vec![(0.0, 0.0), (8.0, 8.0)],
            text: String::new(),
            size: 12.0,
        }],
    )
    .unwrap();
    let snapshot = document.snapshot().unwrap();
    let (root, asset) = setup(&snapshot.image.to_png().unwrap());
    asset
        .write_context(&["preview-file", "--edit-session-stdin"])
        .unwrap();
    let image = vellum_core::Rgb8::load(&asset.image_path()).unwrap();
    assert!(image.data.iter().all(|byte| *byte == 0));
    let names: Vec<_> = fs::read_dir(asset.dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 3);
    for name in names {
        assert!(matches!(
            name.to_str(),
            Some("request" | "context" | "image.png")
        ));
    }
    let context = fs::read_to_string(asset.dir().join("context")).unwrap();
    assert!(!context.contains("edit-session"));
    assert_eq!(list_in(root.path()).unwrap(), [asset.id()]);
}

#[test]
fn receiver_is_consumed_by_initial_window_not_later_windows() {
    let (_root, asset) = setup(b"image");
    RECEIVER.with(|r| r.replace(Some(asset.clone())));
    let initial_window = take_receiver().expect("initial window receives context");
    assert!(
        take_receiver().is_none(),
        "a second hook before map cannot steal or duplicate the request"
    );
    initial_window.acknowledge().unwrap();
    asset.remove_confirmed().unwrap();
    assert!(
        take_receiver().is_none(),
        "a later translation window cannot ACK the removed source"
    );
    assert!(!asset.dir().exists());
}

#[test]
fn rejection_remains_available_before_window_hook_consumes_receiver() {
    let (_root, asset) = setup(b"image");
    RECEIVER.with(|r| r.replace(Some(asset.clone())));
    reject_current("dimensions");
    assert_eq!(asset.rejection_reason().unwrap(), Some("dimensions"));
    assert!(asset.image_path().exists());
    disarm();
    assert!(take_receiver().is_none());
}

#[test]
fn recovery_context_preserves_incomplete_and_independent_outputs() {
    let (root, asset) = setup(b"image");
    asset
        .write_context(&[
            "preview-file",
            "--incomplete",
            "--save-status",
            "done",
            "--copy-status",
            "failed",
        ])
        .unwrap();
    let reopened = Asset::open_in(root.path(), asset.id()).unwrap();
    let args = reopened.context_args().unwrap();
    assert!(args.iter().any(|arg| arg == "--incomplete"));
    let report = crate::preview::OutputReport::from_args(&args);
    assert_eq!(report.save, crate::preview::ExportState::Done);
    assert_eq!(report.copy, crate::preview::ExportState::Failed);
    reopened.discard().unwrap();
    assert!(!asset.dir().exists());
}

/// Lead-only native acceptance. It touches only owned test children and a
/// private TempDir. No capture, clipboard, API, settings, or existing windows.
#[test]
#[ignore = "requires a real Wayland session and explicit absolute VELLUM_TEST_UI"]
fn native_preview_ready_and_decode_rejection() {
    use std::process::Stdio;
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let executable = std::env::var_os("VELLUM_TEST_UI")
        .map(PathBuf::from)
        .expect("set VELLUM_TEST_UI to the verified absolute release/vellum-ui binary");
    assert!(executable.is_absolute() && executable.is_file());
    let sandbox = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .prefix("vellum-native-handoff-test-")
        .tempdir()
        .unwrap();
    let home = sandbox.path().join("home");
    let state = sandbox.path().join("state");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&state).unwrap();
    let recovery = state.join("vellum/recovery");
    let spawn = |asset: &Asset| {
        let mut command = Command::new(&executable);
        command
            .arg("preview-file")
            .arg(asset.image_path())
            .arg("--cleanup")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
            .env("XDG_DATA_HOME", sandbox.path().join("data"))
            .env("XDG_STATE_HOME", &state)
            .env_remove("VELLUM_UI_DEMO")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        asset.configure_child(&mut command);
        OwnedChild(command.spawn().expect("launch owned test preview"))
    };
    let png = vellum_core::Rgb8::new(40, 32).to_png().unwrap();
    let asset = Asset::create_in(&recovery, &png).unwrap();
    let mut child = spawn(&asset);
    assert!(
        asset.image_path().exists(),
        "spawn cannot consume the source file"
    );
    assert!(
        asset
            .wait_ready(&mut child.0, Duration::from_secs(15))
            .unwrap(),
        "real mapped-window ready receipt required"
    );
    assert!(
        asset.image_path().exists(),
        "child must never delete even after ready"
    );
    assert!(child.0.try_wait().unwrap().is_none());
    asset.remove_confirmed().unwrap();
    assert!(!asset.image_path().exists());
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "mapped viewer retains decoded pixels after file cleanup"
    );
    drop(child); // Kill/reap only the Child this test spawned.

    let damaged = Asset::create_in(&recovery, b"not a PNG").unwrap();
    let mut rejected = spawn(&damaged);
    assert!(
        !damaged
            .wait_ready(&mut rejected.0, Duration::from_secs(15))
            .unwrap()
    );
    assert_eq!(damaged.rejection_reason().unwrap(), Some("decode"));
    assert_eq!(fs::read(damaged.image_path()).unwrap(), b"not a PNG");
    assert!(damaged.remove_confirmed().is_err());
    drop(rejected);
}

/// Only the child GTK window is snapshotted; never capture the real desktop.
/// A regular file stands in for an unwritable state tree, so this cannot fill
/// disks or change permissions on any existing directory.
#[test]
#[ignore = "requires a real Wayland session and explicit absolute VELLUM_TEST_UI"]
fn native_memory_recovery_when_state_root_is_a_file() {
    use std::process::Stdio;
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let executable = std::env::var_os("VELLUM_TEST_UI")
        .map(PathBuf::from)
        .expect("set VELLUM_TEST_UI to the verified absolute release/vellum-ui binary");
    assert!(executable.is_absolute() && executable.is_file());
    let sandbox = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .prefix("vellum-native-memory-recovery-")
        .tempdir()
        .unwrap();
    let home = sandbox.path().join("home");
    fs::create_dir(&home).unwrap();
    let blocked_state = sandbox.path().join("state-is-a-file");
    let sentinel = b"intentional test-only blocked state root";
    write_private(&blocked_state, sentinel).unwrap();
    let original = sandbox.path().join("original.png");
    let png = vellum_core::Rgb8::new(40, 32).to_png().unwrap();
    write_private(&original, &png).unwrap();
    let snapshot = sandbox.path().join("memory-window.png");
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(sandbox.path().join("child.log"))
        .unwrap();
    let mut command = Command::new(&executable);
    command
        .arg("demo-longshot-result")
        .arg(&original)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.path().join("data"))
        .env("XDG_STATE_HOME", &blocked_state)
        .env("VELLUM_UI_DEMO", "1")
        .env("VELLUM_UI_SNAPSHOT", &snapshot)
        .env_remove(ID_ENV)
        .env_remove(ROOT_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    let mut child = OwnedChild(command.spawn().expect("launch owned memory-recovery demo"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let rendered = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "memory recovery must keep the image process alive"
        );
        if let Ok(image) = vellum_core::Rgb8::load(&snapshot) {
            break image;
        }
        assert!(
            Instant::now() < deadline,
            "memory recovery did not map and render its GTK window snapshot"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(rendered.width > 0 && rendered.height > 0);
    let child_log = fs::read_to_string(sandbox.path().join("child.log")).unwrap();
    assert!(
        child_log.contains("无法建立恢复副本"),
        "the test must actually exercise failed persistent staging"
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "rendered recovery window must remain alive for saving"
    );
    assert_eq!(
        fs::read(&original).unwrap(),
        png,
        "source test image must never be deleted or changed"
    );
    assert_eq!(fs::read(&blocked_state).unwrap(), sentinel);
    assert!(!blocked_state.join("vellum/recovery").exists());
    eprintln!(
        "native memory fallback rendered {}x{} and retained its original image",
        rendered.width, rendered.height
    );
    drop(child); // Only the exact Child owned by this test is killed/reaped.
}

fn setup(bytes: &[u8]) -> (tempfile::TempDir, Asset) {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let asset = Asset::create_in(root.path(), bytes).unwrap();
    (root, asset)
}
#[test]
fn unavailable_storage_is_reported_without_creating_a_false_recovery_asset() {
    let root = tempfile::tempdir().unwrap();
    let blocked = root.path().join("not-a-directory");
    fs::write(&blocked, b"unrelated file").unwrap();
    assert!(Asset::create_in(&blocked.join("recovery"), b"image").is_err());
    assert_eq!(fs::read(&blocked).unwrap(), b"unrelated file");
    assert!(!blocked.join("recovery").exists());
}

#[test]
fn assets_are_private_unique_and_survive_parent_exit() {
    let (root, first) = setup(b"first image");
    let second = Asset::create_in(root.path(), b"second image").unwrap();
    assert_ne!(first.id(), second.id());
    for asset in [&first, &second] {
        assert_eq!(
            fs::metadata(asset.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(asset.image_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let id = first.id().to_owned();
    drop(first);
    assert_eq!(
        fs::read(Asset::open_in(root.path(), &id).unwrap().image_path()).unwrap(),
        b"first image"
    );
    assert_eq!(list_in(root.path()).unwrap().len(), 2);
}
#[test]
fn spawn_success_without_map_is_not_a_receipt() {
    let (_root, asset) = setup(b"image");
    assert!(!asset.wait_with(Duration::ZERO, || Ok(false)).unwrap());
    assert!(asset.remove_confirmed().is_err());
    assert!(asset.image_path().exists());
}
#[test]
fn early_exit_even_successful_and_timeout_keep_original() {
    let (_root, asset) = setup(b"image");
    assert!(
        !asset
            .wait_with(Duration::from_secs(10), || Ok(true))
            .unwrap()
    );
    assert!(!asset.wait_with(Duration::ZERO, || Ok(false)).unwrap());
    assert_eq!(fs::read(asset.image_path()).unwrap(), b"image");
}
#[test]
fn decode_rejection_is_bounded_and_recoverable() {
    let (root, asset) = setup(b"not a PNG");
    assert!(vellum_core::Rgb8::load(&asset.image_path()).is_err());
    asset.reject("decode").unwrap();
    assert_eq!(asset.rejection_reason().unwrap(), Some("decode"));
    assert!(
        !asset
            .wait_with(Duration::from_secs(10), || Ok(false))
            .unwrap()
    );
    assert!(asset.remove_confirmed().is_err());
    assert_eq!(list_in(root.path()).unwrap(), [asset.id()]);
}
#[test]
fn only_matching_ready_receipt_authorizes_exact_asset_cleanup() {
    let (root, first) = setup(b"first");
    let second = Asset::create_in(root.path(), b"second").unwrap();
    first.acknowledge().unwrap();
    first.acknowledge().unwrap();
    assert!(
        first
            .wait_with(Duration::from_secs(1), || Ok(false))
            .unwrap()
    );
    first.remove_confirmed().unwrap();
    assert!(!first.dir().exists());
    assert!(second.image_path().exists());
    assert!(first.remove_confirmed().is_err());
}
#[test]
fn late_acknowledgment_never_cleans_a_timed_out_request() {
    let (root, asset) = setup(b"late");
    assert!(!asset.wait_with(Duration::ZERO, || Ok(false)).unwrap());
    asset.acknowledge().unwrap();
    let id = asset.id().to_owned();
    drop(asset);
    assert!(
        Asset::open_in(root.path(), &id)
            .unwrap()
            .image_path()
            .exists()
    );
}
#[test]
fn forged_receipts_and_protocol_mismatches_are_rejected() {
    let (_root, asset) = setup(b"image");
    write_private(
        &asset.dir().join("ready"),
        b"vellum-image-handoff/9 ready another-id\n",
    )
    .unwrap();
    assert!(
        asset
            .wait_with(Duration::from_secs(1), || Ok(false))
            .is_err()
    );
    assert!(asset.remove_confirmed().is_err());
    assert!(asset.image_path().exists());
}
#[test]
fn path_traversal_and_unknown_files_are_never_deleted() {
    let (root, asset) = setup(b"image");
    assert!(Asset::open_in(root.path(), "../outside").is_err());
    let unrelated = asset.dir().join("user-file");
    fs::write(&unrelated, b"keep").unwrap();
    asset.acknowledge().unwrap();
    assert!(asset.remove_confirmed().is_err());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    assert!(asset.image_path().exists());
}
#[test]
fn image_replacement_or_symlink_cannot_authorize_cleanup() {
    let (root, asset) = setup(b"original");
    let original_handle = File::open(asset.image_path()).unwrap();
    fs::remove_file(asset.image_path()).unwrap();
    write_private(&asset.image_path(), b"replacement").unwrap();
    assert!(asset.discard().is_err());
    assert_eq!(fs::read(asset.image_path()).unwrap(), b"replacement");
    drop(original_handle);
    let victim = root.path().join("victim");
    fs::write(&victim, b"private user image").unwrap();
    fs::remove_file(asset.image_path()).unwrap();
    std::os::unix::fs::symlink(&victim, asset.image_path()).unwrap();
    assert!(asset.discard().is_err());
    assert_eq!(fs::read(victim).unwrap(), b"private user image");
}
#[test]
fn explicit_discard_removes_only_selected_recovery_id() {
    let (root, first) = setup(b"one");
    let second = Asset::create_in(root.path(), b"two").unwrap();
    let opened = Asset::open_in(root.path(), first.id()).unwrap();
    // A normal recovery open leaves the asset; explicit discard is independent
    // of receipt presence and does not attempt to clean any other request.
    assert_eq!(fs::read(opened.image_path()).unwrap(), b"one");
    opened.discard().unwrap();
    assert_eq!(list_in(root.path()).unwrap(), [second.id()]);
}
#[test]
fn insecure_and_symlink_recovery_roots_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let insecure = root.path().join("public");
    fs::create_dir(&insecure).unwrap();
    fs::set_permissions(&insecure, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Asset::create_in(&insecure, b"image").is_err());
    let link = root.path().join("link");
    std::os::unix::fs::symlink(root.path(), &link).unwrap();
    assert!(Asset::create_in(&link, b"image").is_err());
}
