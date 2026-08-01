//! Screen capture. Shells out to `grim`, exactly like the Python original.
//!
//! `grim` is kept as the capture backend on purpose: it is the reference
//! behaviour under niri, and its ~30-40 ms per grab is dominated by the
//! compositor copy, not by process startup. The win over the Python version
//! comes from decoding straight into a packed RGB buffer (no PIL round-trip)
//! and from `-t ppm`, which skips PNG compression on the capture hot path
//! entirely.

use std::io::Read;
use std::process::{Command, Stdio};

use crate::geom::Rect;
use crate::image::Rgb8;

#[derive(Debug)]
pub enum CaptureError {
    NotFound,
    Failed(String),
    Decode(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "grim not found; install grim"),
            Self::Failed(detail) => write!(f, "grim failed: {detail}"),
            Self::Decode(detail) => write!(f, "cannot decode capture: {detail}"),
        }
    }
}

impl std::error::Error for CaptureError {}

pub fn grab_full() -> Result<Rgb8, CaptureError> {
    grim(&[])
}

pub fn grab_output(name: &str) -> Result<Rgb8, CaptureError> {
    grim(&["-o".to_string(), name.to_string()])
}

pub fn grab_region(rect: Rect) -> Result<Rgb8, CaptureError> {
    if !rect.valid() {
        return Err(CaptureError::Failed(format!(
            "invalid region: {}x{}",
            rect.w, rect.h
        )));
    }
    grim(&[
        "-g".to_string(),
        format!("{},{} {}x{}", rect.x, rect.y, rect.w, rect.h),
    ])
}

/// Capture using PPM output. PPM is uncompressed, so grim skips its PNG encode
/// and we skip a decode; on the long-shot hot path this removes real per-frame
/// work (PNG encode of a 900x700 frame is several ms on its own).
fn grim(extra: &[String]) -> Result<Rgb8, CaptureError> {
    let mut child = Command::new("grim")
        .args(["-t", "ppm"])
        .args(extra)
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CaptureError::NotFound
            } else {
                CaptureError::Failed(e.to_string())
            }
        })?;

    let mut stdout = Vec::new();
    if let Some(pipe) = child.stdout.as_mut() {
        pipe.read_to_end(&mut stdout)
            .map_err(|e| CaptureError::Failed(e.to_string()))?;
    }
    let mut stderr = String::new();
    if let Some(pipe) = child.stderr.as_mut() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child
        .wait()
        .map_err(|e| CaptureError::Failed(e.to_string()))?;
    if !status.success() {
        return Err(CaptureError::Failed(stderr.trim().to_string()));
    }
    decode_ppm(&stdout).map_err(CaptureError::Decode)
}

/// Parse binary PPM (P6). Written by hand because the `image` crate's PNM
/// decoder pulls a full dynamic-image round trip for what is a header plus a
/// packed RGB blob.
pub fn decode_ppm(data: &[u8]) -> Result<Rgb8, String> {
    let mut cursor = 0usize;
    let mut fields: Vec<usize> = Vec::with_capacity(3);

    if data.len() < 2 || &data[0..2] != b"P6" {
        return Err("not a P6 PPM stream".to_string());
    }
    cursor += 2;

    while fields.len() < 3 {
        // Skip whitespace and comments between header fields.
        while cursor < data.len() {
            match data[cursor] {
                b' ' | b'\t' | b'\r' | b'\n' => cursor += 1,
                b'#' => {
                    while cursor < data.len() && data[cursor] != b'\n' {
                        cursor += 1;
                    }
                }
                _ => break,
            }
        }
        let start = cursor;
        while cursor < data.len() && data[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if start == cursor {
            return Err("truncated PPM header".to_string());
        }
        let text = std::str::from_utf8(&data[start..cursor]).map_err(|e| e.to_string())?;
        fields.push(text.parse::<usize>().map_err(|e| e.to_string())?);
    }
    // Exactly one whitespace byte separates the header from the raster.
    if cursor >= data.len() {
        return Err("missing PPM raster".to_string());
    }
    cursor += 1;

    let (width, height, maxval) = (fields[0], fields[1], fields[2]);
    if maxval != 255 {
        return Err(format!("unsupported PPM maxval {maxval}"));
    }
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "PPM dimensions overflow the raster size".to_string())?;
    let end = cursor
        .checked_add(expected)
        .ok_or_else(|| "PPM raster offset overflow".to_string())?;
    let raster = data
        .get(cursor..end)
        .ok_or_else(|| "PPM raster shorter than header claims".to_string())?;
    Ok(Rgb8::from_raw(width, height, raster.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_minimal_ppm() {
        let mut data = b"P6\n2 1\n255\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let img = decode_ppm(&data).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixel(0, 0), [1, 2, 3]);
        assert_eq!(img.pixel(1, 0), [4, 5, 6]);
    }

    #[test]
    fn decodes_ppm_with_comments_and_spaces() {
        let mut data = b"P6 # grim\n 2  2 \n255 ".to_vec();
        data.extend_from_slice(&[9; 12]);
        let img = decode_ppm(&data).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
    }

    #[test]
    fn rejects_dimensions_that_overflow_the_raster_size() {
        let data = format!("P6\n{} 2\n255\n", usize::MAX);
        assert!(decode_ppm(data.as_bytes()).is_err());
    }

    #[test]
    fn rejects_truncated_raster() {
        let mut data = b"P6\n4 4\n255\n".to_vec();
        data.extend_from_slice(&[0; 10]);
        assert!(decode_ppm(&data).is_err());
    }

    #[test]
    fn rejects_wrong_magic() {
        assert!(decode_ppm(b"P3\n1 1\n255\n0 0 0").is_err());
    }
}
