//! Durable image handoff. A spawned process is not a receipt: only a decoded,
//! mapped window may acknowledge. Failed/late requests remain in private state
//! until the user explicitly opens or discards them. No generic path cleanup.
use gtk4::prelude::*;
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const PROTOCOL: &str = "vellum-image-handoff/1";
const ID_ENV: &str = "VELLUM_IMAGE_REQUEST";
const ROOT_ENV: &str = "VELLUM_IMAGE_RECOVERY_ROOT";
pub const READY_TIMEOUT: Duration = Duration::from_secs(8);

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid image recovery ownership or receipt",
    )
}
fn valid_id(id: &str) -> bool {
    id.starts_with("request-")
        && (24..=80).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn private_directory(path: &Path, create: bool) -> io::Result<()> {
    if create {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        match builder.create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err(invalid());
    }
    Ok(())
}
fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(invalid());
    }
    Ok(file)
}
fn small_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    private_file(path)?.take(256).read_to_string(&mut text)?;
    Ok(text)
}
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
pub fn root() -> PathBuf {
    vellum_core::paths::state_dir().join("recovery")
}

#[derive(Debug, Clone)]
pub struct Asset {
    root: PathBuf,
    id: String,
    image_identity: (u64, u64),
}
impl Asset {
    pub fn create(png: &[u8]) -> io::Result<Self> {
        Self::create_in(&root(), png)
    }
    fn create_in(root: &Path, png: &[u8]) -> io::Result<Self> {
        private_directory(root, true)?;
        let directory = tempfile::Builder::new()
            .prefix("request-")
            .permissions(fs::Permissions::from_mode(0o700))
            .rand_bytes(20)
            .tempdir_in(root)?;
        let id = directory
            .path()
            .file_name()
            .ok_or_else(invalid)?
            .to_string_lossy()
            .into_owned();
        write_private(&directory.path().join("image.png"), png)?;
        write_private(
            &directory.path().join("request"),
            format!("{PROTOCOL}\n{id}\n").as_bytes(),
        )?;
        File::open(directory.path())?.sync_all()?;
        // From this point neither Drop nor a failed spawn removes the only copy.
        let _ = directory.keep();
        File::open(root)?.sync_all()?;
        Self::open_in(root, &id)
    }
    pub fn open(id: &str) -> io::Result<Self> {
        Self::open_in(&root(), id)
    }
    fn open_in(root: &Path, id: &str) -> io::Result<Self> {
        if !valid_id(id) {
            return Err(invalid());
        }
        private_directory(root, false)?;
        let dir = root.join(id);
        private_directory(&dir, false)?;
        if small_text(&dir.join("request"))? != format!("{PROTOCOL}\n{id}\n") {
            return Err(invalid());
        }
        let metadata = private_file(&dir.join("image.png"))?.metadata()?;
        Ok(Self {
            root: root.to_path_buf(),
            id: id.into(),
            image_identity: (metadata.dev(), metadata.ino()),
        })
    }
    /// Keep only fixed display facts, never original CLI strings or user paths.
    pub fn write_context(&self, args: &[&str]) -> io::Result<()> {
        self.validate()?;
        let state = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1])
                .filter(|value| matches!(*value, "off" | "done" | "failed" | "uncertain"))
                .unwrap_or("off")
        };
        let incomplete = if args.contains(&"--incomplete") {
            "incomplete"
        } else {
            "complete"
        };
        write_private(
            &self.dir().join("context"),
            format!(
                "vellum-image-context/1\n{incomplete}\n{}\n{}\n",
                state("--save-status"),
                state("--copy-status")
            )
            .as_bytes(),
        )?;
        File::open(self.dir())?.sync_all()
    }
    pub fn context_args(&self) -> io::Result<Vec<String>> {
        self.validate()?;
        let text = match small_text(&self.dir().join("context")) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 4
            || lines[0] != "vellum-image-context/1"
            || !matches!(lines[1], "complete" | "incomplete")
            || !lines[2..]
                .iter()
                .all(|v| matches!(*v, "off" | "done" | "failed" | "uncertain"))
        {
            return Err(invalid());
        }
        let mut args = vec![
            "--save-status".into(),
            lines[2].into(),
            "--copy-status".into(),
            lines[3].into(),
        ];
        if lines[1] == "incomplete" {
            args.push("--incomplete".into());
        }
        Ok(args)
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    fn dir(&self) -> PathBuf {
        self.root.join(&self.id)
    }
    pub fn image_path(&self) -> PathBuf {
        self.dir().join("image.png")
    }
    fn validate(&self) -> io::Result<()> {
        let current = Self::open_in(&self.root, &self.id)?;
        if current.image_identity != self.image_identity {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn configure_child(&self, command: &mut Command) {
        command.env(ID_ENV, &self.id).env(ROOT_ENV, &self.root);
    }
    fn receipt_text(&self) -> String {
        format!("{PROTOCOL} ready {}\n", self.id)
    }
    fn acknowledge(&self) -> io::Result<()> {
        self.write_receipt(&self.receipt_text())
    }
    fn write_receipt(&self, receipt: &str) -> io::Result<()> {
        self.validate()?;
        let mut temp = tempfile::Builder::new()
            .prefix(".receipt-")
            .tempfile_in(self.dir())?;
        temp.write_all(receipt.as_bytes())?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(self.dir().join("ready")) {
            Ok(_) => File::open(self.dir())?.sync_all(),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
                if small_text(&self.dir().join("ready"))? == receipt {
                    Ok(())
                } else {
                    Err(invalid())
                }
            }
            Err(e) => Err(e.error),
        }
    }
    fn reject(&self, reason: &str) -> io::Result<()> {
        if !matches!(reason, "decode" | "dimensions" | "window" | "closed") {
            return Err(invalid());
        }
        self.write_receipt(&format!("{PROTOCOL} rejected {} {reason}\n", self.id))
    }
    pub fn rejection_reason(&self) -> io::Result<Option<&'static str>> {
        match small_text(&self.dir().join("ready")) {
            Ok(text) => {
                for reason in ["decode", "dimensions", "window", "closed"] {
                    if text == format!("{PROTOCOL} rejected {} {reason}\n", self.id) {
                        return Ok(Some(reason));
                    }
                }
                Ok(None)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn is_ready(&self) -> io::Result<bool> {
        self.validate()?;
        match small_text(&self.dir().join("ready")) {
            Ok(text) => {
                if text == self.receipt_text() {
                    Ok(true)
                } else {
                    Err(invalid())
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub fn wait_ready(&self, child: &mut Child, timeout: Duration) -> io::Result<bool> {
        self.wait_with(timeout, || Ok(child.try_wait()?.is_some()))
    }
    fn wait_with(
        &self,
        timeout: Duration,
        mut exited: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            // Even an exit(0) before map is not a successful handoff.
            if exited()? {
                return Ok(false);
            }
            if self.rejection_reason()?.is_some() {
                return Ok(false);
            }
            if self.is_ready()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    pub fn remove_confirmed(&self) -> io::Result<()> {
        if !self.is_ready()? {
            return Err(invalid());
        }
        self.discard()
    }
    /// Only invoked for a confirmed live handoff or explicit user discard by ID.
    /// Unexpected files or a replaced image cause refusal, never recursive delete.
    pub fn discard(&self) -> io::Result<()> {
        self.validate()?;
        let entries: Vec<_> = fs::read_dir(self.dir())?.collect::<io::Result<_>>()?;
        for entry in &entries {
            if !matches!(
                entry.file_name().to_str(),
                Some("request" | "image.png" | "ready" | "context")
            ) {
                return Err(invalid());
            }
            private_file(&entry.path())?;
        }
        // Validation precedes every deletion; none of these are user-selected paths.
        for name in ["ready", "context", "image.png", "request"] {
            let path = self.dir().join(name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && matches!(name, "ready" | "context") => {}
                Err(e) => return Err(e),
            }
        }
        fs::remove_dir(self.dir())?;
        File::open(&self.root)?.sync_all()
    }
}

pub fn list() -> io::Result<Vec<String>> {
    list_in(&root())
}
fn list_in(root: &Path) -> io::Result<Vec<String>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    private_directory(root, false)?;
    let mut ids = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if Asset::open_in(root, &id).is_ok() {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

thread_local! { static RECEIVER: RefCell<Option<Asset>> = const { RefCell::new(None) }; }
pub fn has_pending_receiver() -> bool {
    RECEIVER.with(|receiver| receiver.borrow().is_some())
}
pub fn disarm() {
    RECEIVER.with(|r| r.take());
}
/// Call only after decoding succeeded. An arbitrary --cleanup path is never owned.
pub fn arm_after_decode(path: &Path) -> io::Result<()> {
    RECEIVER.with(|r| r.take());
    let Some(id) = std::env::var_os(ID_ENV) else {
        return Ok(());
    };
    let root = std::env::var_os(ROOT_ENV).ok_or_else(invalid)?;
    let asset = Asset::open_in(Path::new(&root), &id.to_string_lossy())?;
    if asset.image_path() != path {
        return Err(invalid());
    }
    RECEIVER.with(|r| r.replace(Some(asset)));
    Ok(())
}
/// Send only a fixed negative reason, and only for an independently validated
/// asset. Invalid CLI paths cannot create receipts or authorize deletion.
pub fn reject(path: &Path, reason: &str) {
    let (Ok(id), Some(root)) = (std::env::var(ID_ENV), std::env::var_os(ROOT_ENV)) else {
        return;
    };
    if let Ok(asset) = Asset::open_in(Path::new(&root), &id)
        && asset.image_path() == path
    {
        let _ = asset.reject(reason);
    }
}
pub fn reject_current(reason: &str) {
    if let Some(asset) = RECEIVER.with(|r| r.borrow().clone()) {
        let _ = asset.reject(reason);
    }
}
fn take_receiver() -> Option<Asset> {
    RECEIVER.with(|r| r.take())
}
/// Wire before present, after the image and a usable window state were created.
/// Transfer the context to exactly one initial window; later translation/result
/// windows must not acknowledge an already accepted and removed source asset.
pub fn connect_ready(window: &impl IsA<gtk4::Widget>) {
    let Some(asset) = take_receiver() else {
        return;
    };
    if window.is_mapped() {
        let _ = asset.acknowledge();
    } else {
        let sent = std::cell::Cell::new(false);
        window.connect_map(move |_| {
            if sent.replace(true) {
                return;
            }
            if asset.acknowledge().is_err() {
                eprintln!("[vellum] image ready receipt failed; recovery image retained");
            }
        });
    }
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
