//! File-boundary tests use only newly-created private fixtures and sparse length.
use std::fs::{self, OpenOptions};
use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
use vellum_core::Rgb8;
use vellum_core::image::MAX_ENCODED_FILE_BYTES;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let directory = loop {
            let path = root.join(format!(
                "vellum-image-file-tests-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        };
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        Self(directory)
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Fixtures only create direct regular files, sockets, symlinks or FIFOs;
        // never follow a symlink or recursively remove an unknown directory.
        for entry in fs::read_dir(&self.0).unwrap() {
            let path = entry.unwrap().path();
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn encode(format: ImageFormat) -> Vec<u8> {
    let image = RgbImage::from_fn(8, 6, |x, y| {
        image::Rgb([(x * 29) as u8, (y * 41) as u8, 83])
    });
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut output, format)
        .unwrap();
    output.into_inner()
}

#[test]
fn ordinary_png_jpeg_and_webp_still_use_the_existing_decoder() {
    let fixture = Fixture::new();
    for (name, format) in [
        ("small.png", ImageFormat::Png),
        ("small.jpg", ImageFormat::Jpeg),
        ("small.webp", ImageFormat::WebP),
    ] {
        let bytes = encode(format);
        let path = fixture.file(name);
        fs::write(&path, &bytes).unwrap();
        let expected = Rgb8::from_encoded(&bytes).unwrap();
        assert_eq!(Rgb8::load(&path).unwrap(), expected);
    }
}

#[test]
fn selected_symlink_to_regular_image_is_supported() {
    let fixture = Fixture::new();
    let bytes = encode(ImageFormat::Png);
    fs::write(fixture.file("original.png"), &bytes).unwrap();
    symlink("original.png", fixture.file("chosen image.png")).unwrap();
    assert_eq!(
        Rgb8::load(&fixture.file("chosen image.png")).unwrap(),
        Rgb8::from_encoded(&bytes).unwrap()
    );
}

#[test]
fn sparse_over_limit_file_is_rejected_before_reading_or_allocating_its_length() {
    let fixture = Fixture::new();
    let path = fixture.file("oversized.png");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(MAX_ENCODED_FILE_BYTES + 1).unwrap();
    assert!(
        file.metadata().unwrap().blocks() * 512 < 1024 * 1024,
        "sparse test must not allocate a large real file"
    );
    let started = Instant::now();
    let error = Rgb8::load(&path).unwrap_err();
    assert!(error.contains("256 MiB"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn fifo_without_writer_and_symlink_to_fifo_are_rejected_without_hanging_open() {
    let fixture = Fixture::new();
    let fifo = fixture.file("input.fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the C string is valid and names only this private test FIFO.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    symlink("input.fifo", fixture.file("fifo-link.png")).unwrap();
    for path in [fifo, fixture.file("fifo-link.png")] {
        let (sender, receiver) = std::sync::mpsc::channel();
        let input = path.clone();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(Rgb8::load(&input));
        });
        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                // Unblock an old blocking-open regression without blocking this
                // test itself; no user FIFO or desktop process is touched.
                let rescue = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path);
                drop(rescue);
                panic!("image FIFO open was not bounded: {error}");
            }
        };
        worker.join().unwrap();
        assert!(result.unwrap_err().contains("普通文件"));
    }
}

#[test]
fn directory_socket_missing_and_corrupt_inputs_never_echo_paths_or_bytes() {
    let fixture = Fixture::new();
    assert!(Rgb8::load(&fixture.0).unwrap_err().contains("普通文件"));
    let socket = fixture.file("synthetic-secret.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let corrupt = fixture.file("synthetic-secret.png");
    fs::write(&corrupt, b"synthetic-private-header-bytes").unwrap();
    for path in [
        socket,
        corrupt,
        fixture.file("synthetic-secret-missing.png"),
    ] {
        let error = Rgb8::load(&path).unwrap_err();
        assert!(!error.contains("synthetic"));
        assert!(!error.contains(&fixture.0.to_string_lossy().to_string()));
    }
}

#[test]
fn transparent_png_and_webp_file_import_still_hide_invisible_rgb() {
    let fixture = Fixture::new();
    for (name, format) in [
        ("alpha.png", ImageFormat::Png),
        ("alpha.webp", ImageFormat::WebP),
    ] {
        let image = RgbaImage::from_raw(2, 1, vec![211, 19, 87, 0, 0, 128, 255, 128]).unwrap();
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, format)
            .unwrap();
        let path = fixture.file(name);
        fs::write(&path, bytes.get_ref()).unwrap();
        let imported = Rgb8::load(&path).unwrap();
        assert_eq!(imported.pixel(0, 0), [255, 255, 255]);
        assert_eq!(imported.pixel(1, 0), [127, 191, 255]);
    }
}
