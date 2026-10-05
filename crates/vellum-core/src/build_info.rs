//! Shared, CLI-friendly identity of the exact compiled source and build inputs.
//! Runtime paths are deliberately not part of the public JSON or support report.
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const FORMAT: &str = "vellum-build-info-v1";
#[used]
static BUILD_INFO_MARKER: [u8; 20] = *b"vellum-build-info-v1";
const EMBEDDED_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/vellum-build-info.json"));
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;
const RECEIPT_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInfo {
    pub format: String,
    pub version: String,
    pub build_id: String,
    pub source_commit: String,
    #[serde(with = "dirty_status")]
    pub source_dirty: Option<bool>,
    pub source_digest: String,
    pub target: String,
    pub rustc: String,
    pub profile: String,
    pub config_schema: u32,
    pub ipc_schema: u32,
}

mod dirty_status {
    use serde::{
        Deserializer, Serializer,
        de::{Error, Visitor},
    };
    pub fn serialize<S: Serializer>(
        value: &Option<bool>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_bool(*value),
            None => serializer.serialize_str("unknown"),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<bool>, D::Error> {
        struct Dirty;
        impl Visitor<'_> for Dirty {
            type Value = Option<bool>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a boolean or the string unknown")
            }
            fn visit_bool<E: Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(Some(value))
            }
            fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "unknown" {
                    Ok(None)
                } else {
                    Err(E::custom("invalid source dirty state"))
                }
            }
        }
        deserializer.deserialize_any(Dirty)
    }
}

/// No environment, configuration, IPC or display access happens here.
pub fn current() -> BuildInfo {
    static VALUE: OnceLock<BuildInfo> = OnceLock::new();
    VALUE
        .get_or_init(|| {
            serde_json::from_str(EMBEDDED_JSON).expect("compiled build metadata is valid")
        })
        .clone()
}

/// The direct embedded string also retains the marker as contiguous ELF bytes.
pub fn json() -> String {
    std::hint::black_box(&BUILD_INFO_MARKER);
    EMBEDDED_JSON.to_owned()
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn executable_location(executable: &Path) -> Option<(PathBuf, String)> {
    if !executable.is_absolute() {
        return None;
    }
    let name = executable.file_name()?.to_str()?;
    if !matches!(name, "vellum" | "vellumctl" | "vellum-ui" | "vellum-tray") {
        return None;
    }
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let release = bin.parent()?;
    let id = release.file_name()?.to_str()?;
    if !safe_component(id) || release.parent()?.file_name()? != "releases" {
        return None;
    }
    Some((release.to_path_buf(), id.to_string()))
}

fn actual_executable() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

/// True even for a damaged managed installation, so callers do not silently
/// treat missing receipts as development mode or fall back to another PATH bin.
pub fn is_managed_location() -> bool {
    std::env::current_exe().ok().is_some_and(|raw| {
        suspected_managed_location(&raw)
            || raw
                .canonicalize()
                .ok()
                .is_some_and(|path| suspected_managed_location(&path))
    })
}

fn suspected_managed_location(executable: &Path) -> bool {
    if executable_location(executable).is_some() {
        return true;
    }
    // Linux exposes this suffix for an unlinked running executable. Keep it
    // classified as damaged managed state, never silently as a development run.
    let Some(name) = executable
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(" (deleted)"))
    else {
        return false;
    };
    let mut original = executable.to_path_buf();
    original.set_file_name(name);
    executable_location(&original).is_some()
}

fn bounded_json(path: &Path, limit: u64) -> Option<serde_json::Value> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > limit {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn verified_release(executable: &Path, build: &BuildInfo) -> Option<PathBuf> {
    let canonical = executable.canonicalize().ok()?;
    let (root, id) = executable_location(&canonical)?;
    if id != build.build_id {
        return None;
    }
    let manifest = bounded_json(&root.join("manifest.json"), MANIFEST_LIMIT)?;
    if manifest.get("format")?.as_str()? != "vellum-release-v1"
        || manifest.get("release_id")?.as_str()? != id
        || manifest.get("version")?.as_str()? != build.version
    {
        return None;
    }
    let identity: BuildInfo = serde_json::from_value(manifest.get("build")?.clone()).ok()?;
    if identity != *build {
        return None;
    }
    // A downloaded bundle alone is not an installed release. This marker is
    // produced only by the release manager after version preparation.
    let receipt = bounded_json(&root.join("installed.json"), RECEIPT_LIMIT)?;
    if receipt.get("format")?.as_str()? != "vellum-installed-v1"
        || receipt.get("build_id")?.as_str()? != id
        || receipt.get("release_id")?.as_str()? != id
        || !receipt.get("generated")?.is_array()
    {
        return None;
    }
    let generated = root.join("generated");
    let metadata = fs::symlink_metadata(&generated).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    Some(root)
}

pub fn managed_release_id() -> Option<String> {
    let build = current();
    verified_release(&actual_executable()?, &build)?;
    Some(build.build_id)
}

/// True when this program is a managed release that an update has replaced.
///
/// A background service started by D-Bus activation is not restarted by the
/// version manager, so it would otherwise keep serving the old build after
/// `current` moved. Such a process stands down; the next request activates the
/// current release instead. Anything unknown - a development build, a
/// downloaded bundle, or a missing link - is never treated as superseded.
pub fn is_superseded() -> bool {
    actual_executable().is_some_and(|executable| superseded_at(&executable))
}

fn superseded_at(executable: &Path) -> bool {
    let build = current();
    let Some(release) = verified_release(executable, &build) else {
        return false;
    };
    // Layout is <installation root>/releases/<id>; require it explicitly rather
    // than assuming how many levels up the installation root lives.
    let Some(releases) = release.parent() else {
        return false;
    };
    if releases.file_name().and_then(|name| name.to_str()) != Some("releases") {
        return false;
    }
    let Some(root) = releases.parent() else {
        return false;
    };
    let Ok(target) = fs::read_link(root.join("current")) else {
        return false;
    };
    let resolved = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    let (Ok(current), Ok(own)) = (resolved.canonicalize(), release.canonicalize()) else {
        return false;
    };
    current != own
}

/// Anchored to the immutable actual executable's release, never the current
/// symlink. Old processes keep the matching generated resources after an update.
pub fn versioned_resource_root() -> Option<PathBuf> {
    verified_release(&actual_executable()?, &current()).map(|root| root.join("generated"))
}

#[cfg(test)]
#[path = "../build.rs"]
#[allow(dead_code)]
mod build_script;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        root: PathBuf,
        release: PathBuf,
        executable: PathBuf,
        build: BuildInfo,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = std::env::temp_dir().canonicalize().unwrap();
            let root = loop {
                let path = temp.join(format!(
                    "vellum-build-info-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("{error}"),
                }
            };
            let build = current();
            let release = root.join("releases").join(&build.build_id);
            let executable = release.join("bin/vellum");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, b"synthetic binary").unwrap();
            fs::create_dir_all(release.join("generated/icons")).unwrap();
            let manifest = serde_json::json!({"format":"vellum-release-v1","release_id":build.build_id,"version":build.version,"build":build,"files":[]});
            fs::write(
                release.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            Self {
                root,
                release,
                executable,
                build,
            }
        }
        fn install_marker(&self) {
            fs::write(self.release.join("installed.json"),serde_json::to_vec(&serde_json::json!({"format":"vellum-installed-v1","build_id":self.build.build_id,"release_id":self.build.build_id,"generated":[]})).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.root.canonicalize().ok().as_ref() == Some(&self.root)
                && self
                    .root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("vellum-build-info-test-")
            {
                let _ = fs::remove_dir_all(&self.root);
            }
        }
    }

    #[test]
    fn compiled_identity_and_marker_are_consistent() {
        let build = current();
        assert_eq!(build.format, FORMAT);
        assert_eq!(build.version, crate::VERSION);
        assert!(safe_component(&build.build_id));
        assert_eq!(build.source_digest.len(), 64);
        assert_eq!(serde_json::from_str::<BuildInfo>(&json()).unwrap(), build);
        assert!(json().contains(std::str::from_utf8(&BUILD_INFO_MARKER).unwrap()));
    }
    #[test]
    fn unknown_dirty_round_trips_as_explicit_unknown() {
        let mut build = current();
        build.source_dirty = None;
        let value = serde_json::to_value(&build).unwrap();
        assert_eq!(value["source_dirty"], "unknown");
        assert_eq!(serde_json::from_value::<BuildInfo>(value).unwrap(), build);
        build.source_dirty = Some(false);
        assert_eq!(serde_json::to_value(&build).unwrap()["source_dirty"], false);
    }
    #[test]
    fn a_downloaded_bundle_is_not_a_managed_install() {
        let f = Fixture::new();
        assert!(executable_location(&f.executable).is_some());
        assert!(verified_release(&f.executable, &f.build).is_none());
        f.install_marker();
        assert_eq!(
            verified_release(&f.executable, &f.build),
            Some(f.release.clone())
        );
    }
    #[test]
    fn wrong_manifest_or_marker_identity_is_rejected() {
        let f = Fixture::new();
        f.install_marker();
        let mut wrong = f.build.clone();
        wrong.source_digest = "0".repeat(64);
        assert!(verified_release(&f.executable, &wrong).is_none());
        fs::write(
            f.release.join("installed.json"),
            br#"{"format":"vellum-installed-v1","build_id":"other"}"#,
        )
        .unwrap();
        assert!(verified_release(&f.executable, &f.build).is_none());
    }
    #[test]
    fn oversized_or_symbolic_install_marker_is_rejected() {
        let f = Fixture::new();
        fs::write(
            f.release.join("installed.json"),
            vec![b' '; RECEIPT_LIMIT as usize + 1],
        )
        .unwrap();
        assert!(verified_release(&f.executable, &f.build).is_none());
        fs::remove_file(f.release.join("installed.json")).unwrap();
        std::os::unix::fs::symlink("manifest.json", f.release.join("installed.json")).unwrap();
        assert!(verified_release(&f.executable, &f.build).is_none());
    }
    #[test]
    fn a_release_replaced_by_an_update_stands_down() {
        let f = Fixture::new();
        f.install_marker();
        let link = f.root.join("current");
        std::os::unix::fs::symlink(f.release.strip_prefix(&f.root).unwrap(), &link).unwrap();
        assert!(
            !superseded_at(&f.executable),
            "the running release is still the current one"
        );
        fs::create_dir_all(f.root.join("releases/another-build")).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("releases/another-build", &link).unwrap();
        assert!(
            superseded_at(&f.executable),
            "an update moved current away from this release"
        );
    }

    #[test]
    fn supersession_is_never_guessed_from_partial_state() {
        let f = Fixture::new();
        assert!(
            !superseded_at(&f.executable),
            "a bundle or development build is not a managed release"
        );
        f.install_marker();
        assert!(
            !superseded_at(&f.executable),
            "without a current link the state is unknown"
        );
        std::os::unix::fs::symlink("releases/missing", f.root.join("current")).unwrap();
        assert!(
            !superseded_at(&f.executable),
            "an unresolvable current link must not stop a service"
        );
    }

    #[test]
    fn resource_identity_does_not_follow_current_switch() {
        let f = Fixture::new();
        f.install_marker();
        let link = f.root.join("current");
        std::os::unix::fs::symlink(f.release.strip_prefix(&f.root).unwrap(), &link).unwrap();
        assert_eq!(
            verified_release(&f.executable, &f.build),
            Some(f.release.clone())
        );
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("releases/another-build", &link).unwrap();
        assert_eq!(
            verified_release(&f.executable, &f.build),
            Some(f.release.clone())
        );
    }
    #[test]
    fn deleted_managed_executable_fails_closed_instead_of_becoming_development() {
        let f = Fixture::new();
        f.install_marker();
        fs::remove_file(&f.executable).unwrap();
        assert!(suspected_managed_location(&f.executable));
        assert!(suspected_managed_location(
            &f.executable.with_file_name("vellum (deleted)")
        ));
        assert!(verified_release(&f.executable, &f.build).is_none());
        assert!(!suspected_managed_location(Path::new(
            "/tmp/target/debug/vellum (deleted)"
        )));
    }

    #[test]
    fn dev_paths_and_unsafe_components_are_not_managed() {
        assert!(executable_location(Path::new("/tmp/target/debug/vellum")).is_none());
        assert!(executable_location(Path::new("releases/id/bin/vellum")).is_none());
        for value in ["..", ".hidden", "a/b", "a\nb", ""] {
            assert!(!safe_component(value));
        }
    }
}
