//! Wayland clipboard through wl-clipboard, and screenshot saving.

mod atomic_file;
mod clipboard_process;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::image::Rgb8;

#[derive(Debug)]
pub enum ClipboardError {
    NotFound(String),
    Failed(String),
    NoImage,
    UnsupportedFormat,
    InvalidImage(String),
    Timeout,
    TooLarge,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(cmd) => write!(f, "{cmd} not found; install wl-clipboard"),
            Self::Failed(detail) => write!(f, "剪贴板命令失败：{detail}"),
            Self::NoImage => write!(f, "剪贴板没有图片"),
            Self::UnsupportedFormat => write!(f, "剪贴板图片格式不受支持"),
            Self::InvalidImage(detail) => write!(f, "剪贴板图片损坏或无法解码：{detail}"),
            Self::Timeout => write!(f, "剪贴板操作超时，请重试"),
            Self::TooLarge => write!(f, "剪贴板图片或文字超过大小限制"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Complete bytes are visible at this path, but crash durability was not confirmed.
#[derive(Debug)]
pub struct SaveDurabilityError {
    pub path: PathBuf,
    pub source: std::io::Error,
}
impl std::fmt::Display for SaveDurabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "图片已完整写入 {}，但持久化未确认：{}",
            self.path.display(),
            self.source
        )
    }
}
impl std::error::Error for SaveDurabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}
/// An ordinary error means no commit; this typed error means a complete image exists.
pub fn committed_save_path(error: &std::io::Error) -> Option<&Path> {
    error
        .get_ref()?
        .downcast_ref::<SaveDurabilityError>()
        .map(|e| e.path.as_path())
}

pub fn copy_image(img: &Rgb8) -> Result<(), ClipboardError> {
    let png = img.to_png().map_err(ClipboardError::Failed)?;
    copy_png(&png)
}

/// Copy an already encoded PNG so saving and preview handoff can share it.
pub fn copy_png(png: &[u8]) -> Result<(), ClipboardError> {
    run(&["wl-copy", "-t", "image/png"], png)
}

pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    run(&["wl-copy"], text.as_bytes())
}

/// Compatibility wrapper. New callers should preserve the typed error.
pub fn paste_image() -> Option<Rgb8> {
    paste_image_result().ok()
}

/// Read PNG/JPEG/WebP using one total deadline and bounded encoded output.
pub fn paste_image_result() -> Result<Rgb8, ClipboardError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let listed =
        clipboard_process::execute(&["wl-paste", "--list-types"], None, deadline, 64 * 1024)?;
    let mime = select_image_mime(&listed)?;
    let raw = clipboard_process::execute(
        &["wl-paste", "-t", mime],
        None,
        deadline,
        clipboard_process::MAX_BYTES,
    )?;
    decode_clipboard(&raw)
}
fn select_image_mime(listed: &[u8]) -> Result<&'static str, ClipboardError> {
    let types = String::from_utf8_lossy(listed);
    ["image/png", "image/jpeg", "image/webp"]
        .into_iter()
        .find(|candidate| types.lines().any(|line| line.trim() == *candidate))
        .ok_or_else(|| {
            if types.lines().any(|line| line.trim().starts_with("image/")) {
                ClipboardError::UnsupportedFormat
            } else {
                ClipboardError::NoImage
            }
        })
}
fn decode_clipboard(raw: &[u8]) -> Result<Rgb8, ClipboardError> {
    // Decoder allocation is bounded too: compressed byte size alone is insufficient.
    let reader = image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .map_err(|e| ClipboardError::InvalidImage(e.to_string()))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| ClipboardError::InvalidImage(e.to_string()))?;
    if u64::from(width) * u64::from(height) > (clipboard_process::MAX_BYTES / 3) as u64 {
        return Err(ClipboardError::TooLarge);
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .map_err(|e| ClipboardError::InvalidImage(e.to_string()))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(65_536);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => ClipboardError::TooLarge,
        other => ClipboardError::InvalidImage(other.to_string()),
    })?;
    Ok(Rgb8::from_dynamic_on_white(image))
}
fn run(cmd: &[&str], data: &[u8]) -> Result<(), ClipboardError> {
    run_with_deadline(cmd, data, std::time::Duration::from_secs(10))
}
fn run_with_deadline(
    cmd: &[&str],
    data: &[u8],
    timeout: std::time::Duration,
) -> Result<(), ClipboardError> {
    clipboard_process::execute(
        cmd,
        Some(data),
        std::time::Instant::now() + timeout,
        clipboard_process::MAX_BYTES,
    )
    .map(|_| ())
}

/// Save with a microsecond timestamp and an exclusive create, so two detached
/// windows finishing at once can never overwrite each other's screenshot.
pub fn save_image(img: &Rgb8, prefix: &str) -> std::io::Result<PathBuf> {
    let png = img.to_png().map_err(std::io::Error::other)?;
    save_default_bytes(prefix, &png)
}

/// Shared automatic output for region, long screenshots and pinned images.
/// Explicit Save As targets continue to use their own path, not this default.
pub fn save_default_bytes(prefix: &str, png: &[u8]) -> std::io::Result<PathBuf> {
    let prefs = crate::prefs::load_checked()?;
    prefs.check_output_directory()?;
    let dir = prefs.resolved_output_dir();
    std::fs::create_dir_all(&dir)?;
    let now = chrono::Local::now();
    let name = crate::prefs::render_filename_template(&prefs.filename_template, prefix, &now)?;
    atomic_file::unique(&dir, &name, &now.format("%6f").to_string(), png)
}

/// Timestamp format is part of the user-visible contract:
/// `vellum-YYYY-MM-DD_HH-MM-SS-ffffff.png`.
pub fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d_%H-%M-%S-%6f")
        .to_string()
}

pub fn save_bytes(dir: &Path, prefix: &str, png: &[u8]) -> std::io::Result<PathBuf> {
    atomic_file::unique(dir, prefix, &timestamp(), png)
}

/// Replace a target only after the complete image has reached disk.
pub(crate) fn replace_image_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_file::replace(path, bytes)
}

/// Desktop notification. Best-effort: a missing notify-send is not an error.
pub fn notify(title: &str, body: &str, urgency: &str) {
    if let Ok(child) = Command::new("notify-send")
        .arg("--app-name=Vellum")
        .arg(format!("--urgency={urgency}"))
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        crate::proc::reap_in_background(child);
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;
    #[test]
    fn clipboard_alpha_conversion_matches_file_import_without_hidden_rgb() {
        for format in [image::ImageFormat::Png, image::ImageFormat::WebP] {
            for hidden in [[0, 0, 0], [91, 183, 247]] {
                let rgba = image::RgbaImage::from_raw(
                    3,
                    1,
                    vec![
                        hidden[0], hidden[1], hidden[2], 0, 0, 128, 255, 128, 7, 11, 13, 255,
                    ],
                )
                .unwrap();
                let mut out = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgba8(rgba)
                    .write_to(&mut out, format)
                    .unwrap();
                let decoded = decode_clipboard(out.get_ref()).unwrap();
                assert_eq!(decoded.data, vec![255, 255, 255, 127, 191, 255, 7, 11, 13]);
                assert_eq!(decoded, Rgb8::from_encoded(out.get_ref()).unwrap());
            }
        }
    }
    #[test]
    fn each_advertised_mime_decodes_an_independent_fixture() {
        for (mime, bytes) in [
            (
                "image/png",
                include_bytes!("../tests/fixtures/clipboard.png").as_slice(),
            ),
            (
                "image/jpeg",
                include_bytes!("../tests/fixtures/clipboard.jpg").as_slice(),
            ),
            (
                "image/webp",
                include_bytes!("../tests/fixtures/clipboard.webp").as_slice(),
            ),
        ] {
            assert_eq!(select_image_mime(mime.as_bytes()).unwrap(), mime);
            let image = decode_clipboard(bytes).unwrap();
            assert_eq!((image.width, image.height), (16, 12));
        }
    }
    #[test]
    fn missing_unsupported_and_corrupt_images_are_distinct() {
        assert!(matches!(
            select_image_mime(b"text/plain"),
            Err(ClipboardError::NoImage)
        ));
        assert!(matches!(
            select_image_mime(b"image/tiff"),
            Err(ClipboardError::UnsupportedFormat)
        ));
        assert!(matches!(
            decode_clipboard(b"not an image"),
            Err(ClipboardError::InvalidImage(_))
        ));
        assert_eq!(
            select_image_mime(b"image/webp\nimage/png\n").unwrap(),
            "image/png"
        );
    }

    #[test]
    fn an_unresponsive_clipboard_writer_is_bounded() {
        let started = std::time::Instant::now();
        let error = run_with_deadline(
            &["sh", "-c", "sleep 2"],
            &vec![0; 1024 * 1024],
            std::time::Duration::from_millis(50),
        );
        assert!(error.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
    #[test]
    fn a_successful_writer_receives_eof() {
        assert!(
            run_with_deadline(
                &["sh", "-c", "wc -c >/dev/null"],
                b"fixture",
                std::time::Duration::from_secs(1)
            )
            .is_ok()
        );
    }
}
