//! Build identity contains hashes and allowlisted facts, never local paths/env.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const FORMAT: &str = "vellum-build-info-v1";
const SOURCE_DIRS: &[&str] = &["crates", "contrib", "resources", "assets", ".cargo"];
const ROOT_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    "build.rs",
    "README.md",
    "LICENSE",
];
const EXTERNAL_INPUTS: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "CARGO_BUILD_TARGET",
    "SOURCE_DATE_EPOCH",
];

#[derive(Clone, Debug)]
struct Provenance {
    commit: String,
    dirty: Option<bool>,
}

fn main() {
    if let Err(message) = generate() {
        panic!("vellum build metadata: {message}");
    }
}

fn generate() -> Result<(), String> {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest directory")?);
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("invalid workspace layout")?;
    if !root.join("Cargo.toml").is_file() {
        return Err("workspace manifest is unavailable".into());
    }
    // Never watch the workspace root: it contains target/OUT_DIR. Watching only
    // build inputs prevents our generated output from retriggering this script.
    for relative in ROOT_FILES.iter().chain(SOURCE_DIRS.iter()) {
        if root.join(relative).exists() {
            println!("cargo:rerun-if-changed=../../{relative}");
        }
    }
    watch_git(root);
    for key in EXTERNAL_INPUTS {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let inputs = compiler_inputs(env::vars_os());
    for key in inputs
        .keys()
        .filter(|key| key.starts_with("CARGO_PROFILE_"))
    {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let digest = source_digest(root)?;
    let source = provenance(root);
    let version = env::var("CARGO_PKG_VERSION").map_err(|_| "missing package version")?;
    let target = safe_fact(&env::var("TARGET").unwrap_or_default());
    let profile = safe_fact(&env::var("PROFILE").unwrap_or_default());
    let rustc = rustc_version();
    let info = metadata(
        &version, &digest, &source, &target, &rustc, &profile, &inputs,
    )?;
    let bytes = serde_json::to_vec(&info).map_err(|_| "cannot encode build metadata")?;
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing build output directory")?)
        .join("vellum-build-info.json");
    if fs::read(&output).ok().as_deref() != Some(bytes.as_slice()) {
        fs::write(output, bytes).map_err(|_| "cannot write build metadata")?;
    }
    Ok(())
}

fn source_input(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if matches!(name, "Cargo.lock" | "rust-toolchain") {
        return true;
    }
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "rs" | "toml"
                | "css"
                | "svg"
                | "xml"
                | "json"
                | "desktop"
                | "service"
                | "sh"
                | "py"
                | "yml"
                | "yaml"
                | "html"
                | "txt"
                | "csv"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "gif"
                | "ico"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
                | "bin"
        )
    )
}

fn collect_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn walk(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let dir = fs::read_dir(root.join(relative)).map_err(|_| "cannot enumerate build inputs")?;
        for entry in dir {
            let entry = entry.map_err(|_| "cannot inspect build input")?;
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some("target" | ".git" | "node_modules" | ".venv" | "__pycache__")
            ) {
                continue;
            }
            let path = relative.join(name);
            let kind = entry
                .file_type()
                .map_err(|_| "cannot inspect build input type")?;
            if kind.is_symlink() {
                if source_input(&path) || entry.path().is_dir() {
                    return Err("symbolic build inputs are not supported".into());
                }
            } else if kind.is_dir() {
                walk(root, &path, files)?;
            } else if kind.is_file() && source_input(&path) {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    for file in ROOT_FILES {
        if root.join(file).is_file() {
            files.push(PathBuf::from(file));
        }
    }
    for dir in SOURCE_DIRS {
        if root.join(dir).is_dir() {
            walk(root, Path::new(dir), &mut files)?;
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn framed(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn source_digest(root: &Path) -> Result<String, String> {
    let mut hash = Sha256::new();
    framed(&mut hash, b"vellum-source-v1");
    for relative in collect_sources(root)? {
        framed(&mut hash, relative.as_os_str().as_encoded_bytes());
        let mut file =
            fs::File::open(root.join(&relative)).map_err(|_| "cannot read build source")?;
        let length = file
            .metadata()
            .map_err(|_| "cannot measure build source")?
            .len();
        hash.update(length.to_le_bytes());
        let mut buffer = [0u8; 64 * 1024];
        let mut read = 0u64;
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| "cannot hash build source")?;
            if count == 0 {
                break;
            }
            read += count as u64;
            hash.update(&buffer[..count]);
        }
        if read != length {
            return Err("build source changed while hashing; retry once sources are stable".into());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_clear()
        .env(
            "PATH",
            env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn repository_is_workspace(root: &Path) -> bool {
    let Some(top) = git(root, &["rev-parse", "--show-toplevel"])
        .and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return false;
    };
    root.canonicalize()
        .ok()
        .is_some_and(|root| Path::new(top.trim()).canonicalize().ok().as_ref() == Some(&root))
}

fn provenance(root: &Path) -> Provenance {
    if !repository_is_workspace(root) {
        return Provenance {
            commit: "unknown".into(),
            dirty: None,
        };
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD"])
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    let status = git(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    let dirty = if commit.is_some() {
        status.and_then(|status| {
            if !status.is_empty() {
                return Some(true);
            }
            // Git ignores must not make an untracked compiled input appear to
            // belong to a clean tag. Generated target/ files are excluded above.
            let tracked = git(root, &["ls-files", "--cached", "-z"])?;
            let tracked: std::collections::BTreeSet<&[u8]> =
                tracked.split(|byte| *byte == 0).collect();
            let sources = collect_sources(root).ok()?;
            Some(
                sources
                    .iter()
                    .any(|path| !tracked.contains(path.as_os_str().as_encoded_bytes())),
            )
        })
    } else {
        None
    };
    Provenance {
        commit: commit.unwrap_or_else(|| "unknown".into()),
        dirty,
    }
}

fn watch_git(root: &Path) {
    if !repository_is_workspace(root) {
        return;
    }
    for query in ["--absolute-git-dir", "--git-common-dir"] {
        if let Some(bytes) = git(root, &["rev-parse", query])
            && let Ok(value) = String::from_utf8(bytes)
        {
            let value = value.trim();
            if value.contains(['\n', '\r']) {
                continue;
            }
            let path = Path::new(value);
            let directory = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            for entry in ["HEAD", "index", "refs"] {
                let file = directory.join(entry);
                if file.exists() {
                    println!("cargo:rerun-if-changed={}", file.display());
                }
            }
        }
    }
}

fn safe_fact(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        value.into()
    } else {
        "unknown".into()
    }
}

fn sanitize_rustc(value: &str) -> String {
    let mut fields = value.split_whitespace();
    if fields.next() != Some("rustc") {
        return "unknown".into();
    }
    let version = fields
        .next()
        .map(safe_fact)
        .unwrap_or_else(|| "unknown".into());
    let (number, channel) = version
        .split_once('-')
        .map_or((version.as_str(), None), |(number, channel)| {
            (number, Some(channel))
        });
    let parts: Vec<_> = number.split('.').collect();
    let numeric = parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    let known_channel = channel.is_none_or(|channel| {
        matches!(channel, "nightly" | "beta" | "dev")
            || channel.strip_prefix("beta.").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
    });
    if !numeric || !known_channel {
        return "unknown".into();
    }
    let mut output = format!("rustc {version}");
    if let (Some(hash), Some(date)) = (fields.next(), fields.next()) {
        let hash = hash.strip_prefix('(').unwrap_or_default();
        let date = date.strip_suffix(')').unwrap_or_default();
        if (7..=40).contains(&hash.len())
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            && date.len() == 10
            && date
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
        {
            output.push_str(&format!(" ({hash} {date})"));
        }
    }
    output
}

fn rustc_version() -> String {
    let Some(rustc) = env::var_os("RUSTC") else {
        return "unknown".into();
    };
    Command::new(rustc)
        .arg("--version")
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| sanitize_rustc(&output))
        .unwrap_or_else(|| "unknown".into())
}

fn compiler_inputs(
    values: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, Vec<u8>> {
    values
        .into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            if key.len() > 128
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return None;
            }
            (EXTERNAL_INPUTS.contains(&key)
                || matches!(key, "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG")
                || key.starts_with("CARGO_CFG_")
                || key.starts_with("CARGO_FEATURE_")
                || key.starts_with("CARGO_PROFILE_"))
            .then(|| (key.to_string(), value.as_encoded_bytes().to_vec()))
        })
        .collect()
}

fn metadata(
    version: &str,
    source_digest: &str,
    source: &Provenance,
    target: &str,
    rustc: &str,
    profile: &str,
    inputs: &BTreeMap<String, Vec<u8>>,
) -> Result<serde_json::Value, String> {
    if safe_fact(version) == "unknown" {
        return Err("package version is not a safe release component".into());
    }
    let mut hash = Sha256::new();
    framed(&mut hash, b"vellum-build-v1");
    for field in [
        version,
        source_digest,
        &source.commit,
        target,
        rustc,
        profile,
        "config-schema-1",
        "ipc-schema-1",
    ] {
        framed(&mut hash, field.as_bytes());
    }
    let dirty = match source.dirty {
        Some(true) => "dirty",
        Some(false) => "clean",
        None => "unknown",
    };
    framed(&mut hash, dirty.as_bytes());
    for (key, value) in inputs {
        framed(&mut hash, key.as_bytes());
        framed(&mut hash, value);
    }
    let build_id = format!("{version}-{dirty}-{:x}", hash.finalize());
    if build_id.len() > 96 {
        return Err("release identity exceeds safe component limit".into());
    }
    let source_dirty = source
        .dirty
        .map(serde_json::Value::Bool)
        .unwrap_or_else(|| serde_json::Value::String("unknown".into()));
    Ok(
        serde_json::json!({"format":FORMAT,"version":version,"build_id":build_id,
        "source_commit":source.commit,"source_dirty":source_dirty,"source_digest":source_digest,
        "target":target,"rustc":rustc,"profile":profile,"config_schema":1,"ipc_schema":1}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let temp = env::temp_dir().canonicalize().unwrap();
            loop {
                let path = temp.join(format!(
                    "vellum-build-fixture-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            }
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only this freshly-created, still-real private fixture may be removed.
            if self.0.canonicalize().ok().as_ref() == Some(&self.0)
                && self
                    .0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("vellum-build-fixture-")
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }
    #[test]
    fn sources_are_location_independent_and_target_is_excluded() {
        let a = Fixture::new();
        let b = Fixture::new();
        for fixture in [&a, &b] {
            fixture.write("Cargo.toml", b"[workspace]");
            fixture.write("crates/a/src/main.rs", b"fn main() {}");
            fixture.write("crates/a/theme.css", b"color:red");
        }
        let original = source_digest(&a.0).unwrap();
        assert_eq!(original, source_digest(&b.0).unwrap());
        a.write("target/debug/generated.rs", b"not a source");
        a.write("crates/a/target/generated.rs", b"also excluded");
        assert_eq!(original, source_digest(&a.0).unwrap());
        a.write("crates/a/src/new.rs", b"untracked source matters");
        assert_ne!(original, source_digest(&a.0).unwrap());
    }
    #[test]
    fn css_and_manifest_changes_affect_source_identity() {
        let f = Fixture::new();
        f.write("Cargo.toml", b"one");
        f.write("crates/a/theme.css", b"red");
        let first = source_digest(&f.0).unwrap();
        f.write("crates/a/theme.css", b"blue");
        let second = source_digest(&f.0).unwrap();
        assert_ne!(first, second);
        f.write("Cargo.toml", b"two");
        assert_ne!(second, source_digest(&f.0).unwrap());
    }
    #[test]
    fn compiler_inputs_affect_only_the_hash_not_public_details() {
        let source = Provenance {
            commit: "unknown".into(),
            dirty: None,
        };
        let inputs = BTreeMap::new();
        let first = metadata(
            "0.2.0",
            "abc",
            &source,
            "x86_64-unknown-linux-gnu",
            "rustc 1.97.0",
            "release",
            &inputs,
        )
        .unwrap();
        let mut secret = inputs.clone();
        secret.insert(
            "RUSTFLAGS".into(),
            b"--remap-path-prefix=/private/person/token=secret=/src".to_vec(),
        );
        let second = metadata(
            "0.2.0",
            "abc",
            &source,
            "x86_64-unknown-linux-gnu",
            "rustc 1.97.0",
            "release",
            &secret,
        )
        .unwrap();
        assert_ne!(first["build_id"], second["build_id"]);
        assert!(!second.to_string().contains("/private"));
        assert!(!second.to_string().contains("secret"));
        assert_eq!(second["source_dirty"], "unknown");
    }
    #[test]
    fn env_allowlist_drops_credentials_and_target_output_paths() {
        let values = compiler_inputs([
            ("API_KEY".into(), "secret".into()),
            ("OUT_DIR".into(), "/private/target".into()),
            ("RUSTFLAGS".into(), "-Copt-level=2".into()),
        ]);
        assert_eq!(values.len(), 1);
        assert!(values.contains_key("RUSTFLAGS"));
    }
    #[test]
    fn rustc_output_does_not_leak_wrapper_diagnostics() {
        assert_eq!(
            sanitize_rustc("rustc 1.97.0 (abcdef012 2026-01-02)"),
            "rustc 1.97.0 (abcdef012 2026-01-02)"
        );
        assert_eq!(
            sanitize_rustc("wrapper failed at /private?token=secret"),
            "unknown"
        );
        assert_eq!(sanitize_rustc("rustc /private/toolchain secret"), "unknown");
        assert_eq!(sanitize_rustc("rustc SECRET_TOKEN"), "unknown");
        assert_eq!(
            sanitize_rustc("rustc 1.97.0-nightly"),
            "rustc 1.97.0-nightly"
        );
    }
    #[test]
    fn archives_do_not_inherit_an_unrelated_parent_repository() {
        let f = Fixture::new();
        assert!(git(&f.0, &["init", "--quiet"]).is_some());
        assert!(
            git(
                &f.0,
                &[
                    "-c",
                    "user.name=Build fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "fixture"
                ]
            )
            .is_some()
        );
        let own = provenance(&f.0);
        assert_ne!(own.commit, "unknown");
        assert_eq!(own.dirty, Some(false));
        f.write("archived-project/Cargo.toml", b"[workspace]");
        let archive = provenance(&f.0.join("archived-project"));
        assert_eq!(archive.commit, "unknown");
        assert_eq!(archive.dirty, None);
    }

    #[test]
    fn ignored_untracked_compiled_source_is_still_a_dirty_build() {
        let f = Fixture::new();
        assert!(git(&f.0, &["init", "--quiet"]).is_some());
        f.write(".gitignore", b"crates/**\n");
        assert!(git(&f.0, &["add", ".gitignore"]).is_some());
        assert!(
            git(
                &f.0,
                &[
                    "-c",
                    "user.name=Build fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-m",
                    "fixture"
                ]
            )
            .is_some()
        );
        assert_eq!(provenance(&f.0).dirty, Some(false));
        f.write("crates/a/src/ignored.rs", b"fn compiled() {} ");
        assert!(git(&f.0, &["status", "--porcelain=v1"]).unwrap().is_empty());
        assert_eq!(provenance(&f.0).dirty, Some(true));
    }

    #[test]
    fn release_readme_changes_the_content_addressed_identity() {
        let f = Fixture::new();
        f.write("Cargo.toml", b"[workspace]");
        f.write("README.md", b"old release notes");
        let old = source_digest(&f.0).unwrap();
        f.write("README.md", b"new release notes");
        assert_ne!(old, source_digest(&f.0).unwrap());
    }

    #[test]
    fn unknown_provenance_never_becomes_a_clean_build() {
        let source = Provenance {
            commit: "unknown".into(),
            dirty: None,
        };
        let info = metadata(
            "0.2.0",
            "abc",
            &source,
            "target",
            "rustc 1.97.0",
            "debug",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(info["source_dirty"], "unknown");
        assert!(info["build_id"].as_str().unwrap().contains("-unknown-"));
    }
}
