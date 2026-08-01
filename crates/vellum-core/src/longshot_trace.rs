//! Privacy-safe, opt-in tracing primitives for long-shot diagnosis.
//!
//! The trace deliberately accepts only numbers, booleans and `&'static str`
//! values. There is no API for owned/user-provided text, image bytes, window
//! titles, OCR output or backend error strings, so adding an event cannot
//! accidentally copy private content into the service log.

use std::ffi::OsStr;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// Opt-in switch understood by the hotkey client, daemon and UI process.
pub const TRACE_ENV: &str = "VELLUM_LONGSHOT_TRACE";
/// Internal argv marker used to carry the opt-in through the daemon boundary.
pub const TRACE_ARG: &str = "--longshot-trace";
/// Internal argv prefix carrying the daemon-generated correlation id.
pub const TRACE_SESSION_ARG_PREFIX: &str = "--longshot-trace-session=";
/// Prefix used for machine-readable lines in the existing private service log.
pub const TRACE_LINE_PREFIX: &str = "[vellum-longshot-trace] ";

const SCHEMA: &str = "vellum-longshot-trace/v1";
const SESSION_BYTES: usize = 16;
const SESSION_HEX_LEN: usize = SESSION_BYTES * 2;

/// A field type that cannot hold arbitrary runtime text or image data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TraceField {
    U64(u64),
    I64(i64),
    Bool(bool),
    F64(f64),
    Static(&'static str),
}

#[derive(Debug)]
struct Inner {
    session: String,
    source: &'static str,
    started: Instant,
    sequence: AtomicU64,
}

/// Cloneable per-process trace emitter. Disabled traces are allocation-free.
#[derive(Clone, Default)]
pub struct LongshotTrace {
    inner: Option<Arc<Inner>>,
}

impl LongshotTrace {
    /// Builds a trace when argv or the environment explicitly requests one.
    pub fn from_args(source: &'static str, args: &[String]) -> Self {
        if !trace_requested(args) && !env_enabled() {
            return Self::default();
        }

        let session = match session_from_args(args) {
            Some(session) => session.to_string(),
            None => match generate_session_id() {
                Ok(session) => session,
                Err(error) => {
                    eprintln!(
                        "[vellum] long-shot trace unavailable: cannot create session id: {error}"
                    );
                    return Self::default();
                }
            },
        };
        Self::with_session(source, session)
    }

    /// Builds an enabled trace from an already validated correlation id.
    pub fn with_session(source: &'static str, session: String) -> Self {
        if !valid_session_id(&session) {
            return Self::default();
        }
        Self {
            inner: Some(Arc::new(Inner {
                session,
                source,
                started: Instant::now(),
                sequence: AtomicU64::new(0),
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn session(&self) -> Option<&str> {
        self.inner.as_ref().map(|inner| inner.session.as_str())
    }

    /// Encodes one JSON line. Callers that own a structured logger can record
    /// this directly; [`Self::emit`] writes the same line to stderr.
    pub fn line(
        &self,
        event: &'static str,
        fields: &[(&'static str, TraceField)],
    ) -> Option<String> {
        let inner = self.inner.as_ref()?;
        let sequence = inner.sequence.fetch_add(1, Ordering::Relaxed);
        let elapsed_us = inner.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;

        let mut object = Map::new();
        object.insert("schema".into(), Value::String(SCHEMA.into()));
        object.insert("session".into(), Value::String(inner.session.clone()));
        object.insert("source".into(), Value::String(inner.source.into()));
        object.insert("source_seq".into(), Value::from(sequence));
        object.insert("elapsed_us".into(), Value::from(elapsed_us));
        object.insert("unix_ms".into(), Value::from(unix_ms));
        object.insert("pid".into(), Value::from(u64::from(std::process::id())));
        object.insert("event".into(), Value::String(event.into()));

        for &(key, field) in fields {
            if reserved_key(key) {
                continue;
            }
            let value = match field {
                TraceField::U64(value) => Value::from(value),
                TraceField::I64(value) => Value::from(value),
                TraceField::Bool(value) => Value::from(value),
                TraceField::F64(value) if value.is_finite() => Value::from(value),
                TraceField::F64(_) => Value::Null,
                TraceField::Static(value) => Value::String(value.into()),
            };
            object.insert(key.into(), value);
        }

        Some(Value::Object(object).to_string())
    }

    pub fn emit(&self, event: &'static str, fields: &[(&'static str, TraceField)]) {
        if let Some(line) = self.line(event, fields) {
            eprintln!("{TRACE_LINE_PREFIX}{line}");
        }
    }
}

/// `=1` is intentional: inherited values such as `0` must not enable a noisy
/// per-frame trace by mere presence.
pub fn env_enabled() -> bool {
    std::env::var_os(TRACE_ENV).as_deref() == Some(OsStr::new("1"))
}

pub fn trace_requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == TRACE_ARG) || session_from_args(args).is_some()
}

/// Adds the internal opt-in marker once. The caller decides whether the
/// current action is a long shot before calling this helper.
pub fn ensure_trace_arg(args: &mut Vec<String>) {
    if !trace_requested(args) {
        args.push(TRACE_ARG.to_string());
    }
}

pub fn session_from_args(args: &[String]) -> Option<&str> {
    args.iter()
        .filter_map(|arg| arg.strip_prefix(TRACE_SESSION_ARG_PREFIX))
        .find(|session| valid_session_id(session))
}

/// Ensures a traced daemon launch has exactly one valid session id argument.
/// `Ok(None)` means tracing was not requested.
pub fn ensure_session_arg(args: &mut Vec<String>) -> io::Result<Option<String>> {
    if !trace_requested(args) {
        return Ok(None);
    }

    let session = session_from_args(args)
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(generate_session_id)?;
    args.retain(|arg| !arg.starts_with(TRACE_SESSION_ARG_PREFIX));
    args.push(format!("{TRACE_SESSION_ARG_PREFIX}{session}"));
    Ok(Some(session))
}

pub fn valid_session_id(value: &str) -> bool {
    value.len() == SESSION_HEX_LEN
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Uses Linux `getrandom(2)` rather than timestamps or paths. A failed entropy
/// query is reported as unavailable instead of being mislabeled as random.
pub fn generate_session_id() -> io::Result<String> {
    let mut bytes = [0u8; SESSION_BYTES];
    let mut filled = 0usize;
    while filled < bytes.len() {
        // SAFETY: the slice is initialized, writable, and remains live for the
        // syscall. A successful return never exceeds the supplied length.
        let read = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if read > 0 {
            filled += read as usize;
            continue;
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned zero bytes",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }

    let mut encoded = String::with_capacity(SESSION_HEX_LEN);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

fn reserved_key(key: &str) -> bool {
    matches!(
        key,
        "schema" | "session" | "source" | "source_seq" | "elapsed_us" | "unix_ms" | "pid" | "event"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn session_ids_are_random_shaped_and_distinct() {
        let first = generate_session_id().expect("kernel random id");
        let second = generate_session_id().expect("kernel random id");
        assert!(valid_session_id(&first));
        assert!(valid_session_id(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn a_trace_request_gets_exactly_one_valid_session_argument() {
        let mut args = vec![
            TRACE_ARG.to_string(),
            format!("{TRACE_SESSION_ARG_PREFIX}not-valid"),
        ];
        let session = ensure_session_arg(&mut args)
            .expect("session generation")
            .expect("tracing requested");

        assert!(valid_session_id(&session));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with(TRACE_SESSION_ARG_PREFIX))
                .count(),
            1
        );
        assert_eq!(session_from_args(&args), Some(session.as_str()));
    }

    #[test]
    fn an_unrequested_launch_is_left_untraced() {
        let mut args = vec!["--no-copy".to_string()];
        assert_eq!(ensure_session_arg(&mut args).unwrap(), None);
        assert_eq!(args, ["--no-copy"]);
    }

    #[test]
    fn the_internal_trace_marker_is_idempotent() {
        let mut args = vec!["--no-copy".to_string()];
        ensure_trace_arg(&mut args);
        ensure_trace_arg(&mut args);
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == TRACE_ARG).count(),
            1
        );
    }

    #[test]
    fn trace_lines_are_structured_and_have_no_arbitrary_text_channel() {
        let trace = LongshotTrace::with_session("ui", SESSION.to_string());
        let line = trace
            .line(
                "stitch_frame",
                &[
                    ("frame", TraceField::U64(7)),
                    ("shift", TraceField::I64(-20)),
                    ("accepted", TraceField::Bool(true)),
                    ("diff", TraceField::F64(1.25)),
                    ("decision", TraceField::Static("accepted")),
                    // Reserved fields cannot replace correlation metadata.
                    ("session", TraceField::Static("not-user-data")),
                ],
            )
            .expect("enabled trace");
        let value: Value = serde_json::from_str(&line).expect("valid JSON");

        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["session"], SESSION);
        assert_eq!(value["source"], "ui");
        assert_eq!(value["event"], "stitch_frame");
        assert_eq!(value["frame"], 7);
        assert_eq!(value["shift"], -20);
        assert_eq!(value["decision"], "accepted");
        assert!(value.get("pixels").is_none());
        assert!(value.get("title").is_none());
        assert!(value.get("text").is_none());
    }

    #[test]
    fn non_finite_metrics_are_json_null_not_invalid_json() {
        let trace = LongshotTrace::with_session("ui", SESSION.to_string());
        let line = trace
            .line("score", &[("diff", TraceField::F64(f64::INFINITY))])
            .unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value["diff"].is_null());
    }

    #[test]
    fn malformed_supplied_session_disables_the_trace() {
        let trace = LongshotTrace::with_session("ui", "../../private".to_string());
        assert!(!trace.enabled());
        assert!(trace.line("anything", &[]).is_none());
    }
}
