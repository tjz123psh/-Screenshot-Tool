use super::*;
use crate::prefs::{self, Preferences};

#[test]
fn settings_isolated_regression() {
    if std::env::var_os("VELLUM_SETTINGS_TEST_CHILD").is_none() {
        let root = std::env::temp_dir().join(format!(
            "vellum-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::settings_tests::settings_isolated_regression",
                "--nocapture",
            ])
            .env("VELLUM_SETTINGS_TEST_CHILD", "1")
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", &root)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "isolated settings child failed; assets at {}",
            root.display()
        );
        return;
    }
    if let Ok(field) = std::env::var("VELLUM_SETTINGS_TEST_WORKER") {
        for _ in 0..20 {
            prefs::update(|p| {
                if field == "save" {
                    p.save = !p.save
                } else {
                    p.copy = !p.copy
                }
            })
            .unwrap();
        }
        return;
    }
    let base = Config::default();
    let defaults = Preferences::default();
    base.save().unwrap();
    prefs::store(&defaults).unwrap();
    // Open A; toggle in tray B; A's capture resolves B and unrelated save keeps B.
    let tray = prefs::update(|p| p.save = false).unwrap();
    assert_eq!(prefs::load(), tray);
    let mut edited = base.clone();
    edited.llm.target_lang = "English".into();
    let (saved, output) = edited.save_settings(&base, &defaults, &defaults).unwrap();
    assert_eq!(output, tray);
    assert_eq!(prefs::load(), tray);
    assert_eq!(saved.llm.target_lang, "English");
    // A second stale panel edits a different leaf in the same section.
    let mut second = base.clone();
    second.llm.model = "test-model".into();
    let (merged, _) = second.save_settings(&base, &defaults, &defaults).unwrap();
    assert_eq!(merged.llm.target_lang, "English");
    assert_eq!(merged.llm.model, "test-model");
    second.llm.target_lang = "French".into();
    assert_eq!(
        second
            .save_settings(&base, &defaults, &defaults)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(Config::load(), merged);
    // Two concurrent read-modify-write operations must not lose unrelated fields.
    prefs::store(&defaults).unwrap();
    let a = std::thread::spawn(|| prefs::update(|p| p.save = false).unwrap());
    let b = std::thread::spawn(|| prefs::update(|p| p.copy = false).unwrap());
    a.join().unwrap();
    b.join().unwrap();
    assert_eq!(
        prefs::load(),
        Preferences {
            save: false,
            copy: false,
            ..Preferences::default()
        }
    );
    // Independent processes share the exact same lock and retain all 40 toggles.
    let spawn = |field: &str| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::settings_tests::settings_isolated_regression",
            ])
            .env("VELLUM_SETTINGS_TEST_WORKER", field)
            .spawn()
            .unwrap()
    };
    let mut a = spawn("save");
    let mut b = spawn("copy");
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(
        prefs::load(),
        Preferences {
            save: false,
            copy: false,
            ..Preferences::default()
        }
    );
    // Locked/stale lock-file cases: timeout is bounded; dropping lock recovers.
    let lock = settings_lock::Lock::acquire(&paths::config_path()).unwrap();
    assert_eq!(base.save().unwrap_err().kind(), io::ErrorKind::TimedOut);
    drop(lock);
    base.save().unwrap();
    // Invalid current documents must remain byte-identical; diagnostics never quote them.
    let config_path = paths::config_path();
    let prefs_path = prefs::path();
    let valid_config = std::fs::read(&config_path).unwrap();
    let valid_prefs = std::fs::read(&prefs_path).unwrap();
    for damaged in [
        "[api\nsecret_config_marker",
        "api = 'secret_config_marker'",
        "['secret_config_marker'",
    ] {
        std::fs::write(&config_path, damaged).unwrap();
        for error in [
            edited
                .save_settings(&base, &defaults, &defaults)
                .unwrap_err(),
            base.save().unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("secret_config_marker"));
        }
        assert_eq!(std::fs::read(&config_path).unwrap(), damaged.as_bytes());
        assert_eq!(std::fs::read(&prefs_path).unwrap(), valid_prefs);
    }
    std::fs::write(&config_path, &valid_config).unwrap();
    for damaged in [
        "{secret_prefs_marker",
        "[]",
        "null",
        "\"secret_prefs_marker\"",
        "{\"save\":\"secret_prefs_marker\"}",
    ] {
        std::fs::write(&prefs_path, damaged).unwrap();
        for error in [
            edited
                .save_settings(&base, &defaults, &defaults)
                .unwrap_err(),
            prefs::update(|p| p.save = !p.save).unwrap_err(),
            prefs::store(&defaults).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("secret_prefs_marker"));
        }
        assert_eq!(std::fs::read(&config_path).unwrap(), valid_config);
        assert_eq!(std::fs::read(&prefs_path).unwrap(), damaged.as_bytes());
    }
    std::fs::write(&prefs_path, &valid_prefs).unwrap();
    output_preferences_regression(&base, &defaults);
    // Malformed destination fails before either file is rewritten.
    let before = std::fs::read(paths::config_path()).unwrap();
    let preference_path = prefs::path();
    std::fs::rename(&preference_path, preference_path.with_extension("backup")).unwrap();
    std::fs::create_dir(&preference_path).unwrap();
    assert!(edited.save_settings(&base, &defaults, &defaults).is_err());
    assert_eq!(std::fs::read(paths::config_path()).unwrap(), before);
}

fn output_preferences_regression(base: &Config, defaults: &Preferences) {
    use std::os::unix::fs::PermissionsExt as _;
    let original_config = std::fs::read(paths::config_path()).unwrap();
    let original_prefs = std::fs::read(prefs::path()).unwrap();
    // Older two-key JSON remains valid, and arbitrary future keys survive a toggle.
    std::fs::write(
        prefs::path(),
        r#"{"save":false,"copy":true,"future":{"nested":[1,"x",null]}}"#,
    )
    .unwrap();
    let old = prefs::load_checked().unwrap();
    assert!(!old.save && old.copy && !old.always_preview);
    assert_eq!(old.filename_template, prefs::DEFAULT_FILENAME_TEMPLATE);
    prefs::update(|p| p.copy = false).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(prefs::path()).unwrap()).unwrap();
    assert_eq!(json["future"], serde_json::json!({"nested":[1,"x",null]}));
    // A stale panel changes only API values, not a newer output directory/template.
    prefs::store(defaults).unwrap();
    let custom = prefs::update(|p| {
        p.output_dir = paths::home()
            .join("custom-output")
            .to_str()
            .unwrap()
            .to_owned();
        p.filename_template = "review-{kind}-{date}_{time}".into();
        p.always_preview = true;
    })
    .unwrap();
    let mut api = base.clone();
    api.api.timeout_s = 42;
    let (_, saved) = api.save_settings(base, defaults, defaults).unwrap();
    assert_eq!(saved, custom);
    let mut other = defaults.clone();
    other.output_dir = paths::home()
        .join("different-output")
        .to_str()
        .unwrap()
        .into();
    assert_eq!(
        base.save_settings(base, defaults, &other)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    let png = include_bytes!("../tests/fixtures/clipboard.png");
    let first = crate::io::save_default_bytes("vellum-long", png).unwrap();
    assert_eq!(first.parent().unwrap(), custom.resolved_output_dir());
    assert!(
        first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("review-vellum-long-")
    );
    assert_eq!(first.extension().unwrap(), "png");
    assert_eq!(std::fs::read(&first).unwrap(), png);
    // All automatic callers use the same latest prefs, not their opening snapshot.
    let changed = prefs::update(|p| {
        p.output_dir = paths::home().join("newer-output").to_str().unwrap().into()
    })
    .unwrap();
    let jobs: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || crate::io::save_default_bytes("vellum-pin", png).unwrap())
        })
        .collect();
    let files: std::collections::HashSet<_> =
        jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(files.len(), 8);
    for file in files {
        assert_eq!(file.parent().unwrap(), changed.resolved_output_dir());
        assert_eq!(std::fs::read(file).unwrap(), png);
    }
    assert_eq!(std::fs::read(&first).unwrap(), png);
    // A destination that is a file (or is not writable) never drops input bytes.
    let blocker = paths::home().join("not-a-directory");
    std::fs::write(&blocker, b"keep").unwrap();
    prefs::update(|p| p.output_dir = blocker.to_str().unwrap().into()).unwrap();
    assert!(crate::io::save_default_bytes("vellum", png).is_err());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"keep");
    let locked = paths::home().join("read-only-output");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    prefs::update(|p| p.output_dir = locked.to_str().unwrap().into()).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(
            crate::io::save_default_bytes("vellum", png)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    // New known fields of an unknown/wrong type must not be overwritten by defaults.
    for bad in [
        r#"{"output_dir":42}"#,
        r#"{"filename_template":[]}"#,
        r#"{"always_preview":"yes"}"#,
    ] {
        std::fs::write(prefs::path(), bad).unwrap();
        let before = std::fs::read(paths::config_path()).unwrap();
        assert!(base.save_settings(base, defaults, defaults).is_err());
        assert!(prefs::update(|p| p.copy = !p.copy).is_err());
        assert_eq!(std::fs::read(paths::config_path()).unwrap(), before);
        assert_eq!(std::fs::read(prefs::path()).unwrap(), bad.as_bytes());
    }
    std::fs::write(paths::config_path(), original_config).unwrap();
    std::fs::write(prefs::path(), original_prefs).unwrap();
}

#[test]
fn settings_output_template_is_bounded_and_not_a_chrono_program() {
    for invalid in [
        "",
        "../escape",
        "folder\\name",
        "nul\0byte",
        "line\nend",
        "{unknown}",
        "{date",
        "date}",
        ".",
        "..",
    ] {
        assert!(prefs::validate_filename_template(invalid).is_err());
    }
    assert!(prefs::validate_filename_template(&"x".repeat(121)).is_err());
    let now = chrono::Local::now();
    let literal = prefs::render_filename_template("literal-%Q-{kind}", "vellum", &now).unwrap();
    assert_eq!(literal, "literal-%Q-vellum");
    assert!(prefs::render_filename_template("{kind}", "../bad", &now).is_err());
    assert!(prefs::render_filename_template("{kind}", &"x".repeat(41), &now).is_err());
    let rendered =
        prefs::render_filename_template(prefs::DEFAULT_FILENAME_TEMPLATE, "vellum", &now).unwrap();
    assert_eq!(
        rendered,
        format!("vellum-{}", now.format("%Y-%m-%d_%H-%M-%S"))
    );
}
