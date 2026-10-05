use super::*;

#[test]
#[ignore = "subprocess helper; launched only by private_session_survives_exec"]
fn child_receiver_process() {
    assert_eq!(
        std::env::var("VELLUM_TEST_SESSION_CHILD").as_deref(),
        Ok("1")
    );
    let bytes = read_stdin().expect("sealed anonymous stdin survives exec");
    assert_eq!(
        unsafe { libc::fcntl(0, libc::F_GETFD) },
        -1,
        "owned session stdin closes after reading"
    );
    let document = crate::document::Document::decode_session(&bytes).unwrap();
    let snapshot = document.snapshot().unwrap();
    assert_eq!((snapshot.image.width, snapshot.image.height), (4, 3));
    assert!(snapshot.image.data.iter().all(|byte| *byte == 0));
    println!("PRIVATE_SESSION_RECEIVED");
}
#[test]
fn private_session_survives_exec() {
    let document = crate::document::Document::from_raster(vellum_core::Rgb8::new(4, 3)).unwrap();
    let file = prepare(&document.encode_session().unwrap()).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session_transfer::tests::child_receiver_process",
            "--ignored",
            "--nocapture",
        ])
        .env("VELLUM_TEST_SESSION_CHILD", "1")
        .stdin(std::process::Stdio::from(file))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PRIVATE_SESSION_RECEIVED"));
}

#[test]
fn session_roundtrip_exposes_only_matching_cover_composite_to_consumers() {
    use crate::annotate::{Stroke, Tool};
    use crate::document::Document;
    let source = vellum_core::Rgb8::from_raw(8, 8, [203, 71, 29].repeat(64));
    let cover = Stroke {
        tool: Tool::Cover,
        color: (1.0, 0.0, 0.0),
        width: 4.0,
        points: vec![(0.0, 0.0), (8.0, 8.0)],
        text: String::new(),
        size: 12.0,
    };
    let document = Document::from_selection(source.clone(), vec![cover]).unwrap();
    let rendered = document.snapshot().unwrap();
    assert!(rendered.image.data.iter().all(|byte| *byte == 0));
    let bytes = document.encode_session().unwrap();
    let bytes = read_file(prepare(&bytes).unwrap(), crate::document::MAX_SESSION_BYTES).unwrap();
    assert!(crate::decode_edit_document(&bytes, &rendered.image).is_some());
    assert!(
        crate::decode_edit_document(&bytes, &source).is_none(),
        "mismatching raster must not adopt a private session"
    );
    assert!(crate::decode_edit_document(b"broken", &rendered.image).is_none());
}

#[test]
fn anonymous_session_is_sealed_unlinked_and_round_trips() {
    let mut file = prepare_with_limit(b"private editable data", 128).unwrap();
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    assert!(file.write_all(b"cannot mutate").is_err());
    assert!(file.set_len(1).is_err());
    assert_eq!(read_file(file, 128).unwrap(), b"private editable data");
}
#[test]
fn zero_oversize_and_unsealed_inputs_are_rejected() {
    assert!(prepare_with_limit(b"", 128).is_err());
    assert!(prepare_with_limit(&[0; 129], 128).is_err());
    let fd =
        unsafe { libc::memfd_create(NAME.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    assert!(fd >= 0);
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(b"untrusted mutable source").unwrap();
    assert!(read_file(file, 128).is_err());
    let file = prepare_with_limit(&[0; 129], 256).unwrap();
    assert!(read_file(file, 128).is_err());
}
#[test]
fn regular_file_stdin_is_not_a_private_session() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"ordinary disk file").unwrap();
    assert!(read_file(file, 128).is_err());
}
#[test]
fn pipe_stdin_is_rejected_without_waiting_for_eof() {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let read = unsafe { File::from_raw_fd(fds[0]) };
    let _write = unsafe { File::from_raw_fd(fds[1]) };
    assert!(read_file(read, 128).is_err());
}
