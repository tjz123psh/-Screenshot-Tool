//! JSON-over-Unix-socket control protocol.
//!
//! Wire format is one JSON object per line in each direction, matching the
//! Python service so the debugging habits (`socat`, `nc -U`) carry over.
//! Framing on a newline also keeps the client able to stop reading as soon as
//! the reply is complete, which matters on the hotkey path.

use serde::{Deserialize, Serialize};

/// Largest accepted request/response line. The Python service used the same
/// bound; it exists so a stuck peer cannot make the daemon allocate freely.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Set in the environment of spawned action processes so they run the work
/// directly instead of bouncing the request back into the daemon.
pub const BYPASS_ENV: &str = "VELLUM_BYPASS_SERVICE";

/// Actions the daemon is allowed to launch on behalf of a hotkey.
///
/// Keeping this a closed enum (rather than the Python string set) means an
/// unknown action is rejected during deserialization, before any process spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Region,
    Long,
    PinLast,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Region => "region",
            Action::Long => "long",
            Action::PinLast => "pin-last",
        }
    }

    /// `region`/`long` may not overlap: only one selector can own the screen.
    /// `pin-last` is not exclusive and is not tracked as the active child.
    pub fn is_exclusive(self) -> bool {
        matches!(self, Action::Region | Action::Long)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Action::Region => "区域截图",
            Action::Long => "长截图",
            Action::PinLast => "钉图",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "region" => Some(Action::Region),
            "long" => Some(Action::Long),
            "pin-last" => Some(Action::PinLast),
            _ => None,
        }
    }
}

/// Requests accepted by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    Status,
    Action {
        action: Action,
        #[serde(default)]
        args: Vec<String>,
    },
    Shutdown,
}

/// Daemon lifecycle state as reported by `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Busy,
    Stopped,
}

/// Single response shape for every command.
///
/// `ok` says the daemon understood the request; `accepted` says an action was
/// actually started (or toggled). A rejected-but-understood request is
/// `ok: true, accepted: false`, which is how the client distinguishes "busy"
/// from "daemon is not there".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<f64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub accepted: bool,
    /// True when an exclusive action was already running.
    #[serde(default, skip_serializing_if = "is_false")]
    pub busy: bool,
    /// True when this request finished an in-flight long shot instead of
    /// starting a new capture.
    #[serde(default, skip_serializing_if = "is_false")]
    pub toggled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for Response {
    fn default() -> Self {
        Self {
            ok: true,
            running: false,
            state: None,
            action: None,
            action_pid: None,
            pid: None,
            version: None,
            started_at: None,
            last_event: None,
            last_event_at: None,
            accepted: false,
            busy: false,
            toggled: false,
            message: None,
        }
    }
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            ..Self::default()
        }
    }

    /// Understood, but the action was not started.
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            ..Self::default()
        }
    }

    pub fn stopped() -> Self {
        Self {
            ok: false,
            state: Some(State::Stopped),
            version: Some(vellum_core::VERSION.to_string()),
            message: Some("截图服务未运行".to_string()),
            ..Self::default()
        }
    }

    pub fn is_running(&self) -> bool {
        self.ok && self.running
    }
}

/// Encode a value as one newline-terminated JSON line.
pub fn encode_line<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    buf.push(b'\n');
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_round_trips_through_kebab_case() {
        let json = serde_json::to_string(&Action::PinLast).unwrap();
        assert_eq!(json, "\"pin-last\"");
        assert_eq!(
            serde_json::from_str::<Action>("\"pin-last\"").unwrap(),
            Action::PinLast
        );
    }

    #[test]
    fn unknown_action_is_rejected_before_spawn() {
        let raw = r#"{"command":"action","action":"rm-rf","args":[]}"#;
        assert!(serde_json::from_str::<Request>(raw).is_err());
    }

    #[test]
    fn action_request_defaults_to_empty_args() {
        let raw = r#"{"command":"action","action":"region"}"#;
        match serde_json::from_str::<Request>(raw).unwrap() {
            Request::Action { action, args } => {
                assert_eq!(action, Action::Region);
                assert!(args.is_empty());
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn exclusivity_matches_the_python_semantics() {
        assert!(Action::Region.is_exclusive());
        assert!(Action::Long.is_exclusive());
        assert!(!Action::PinLast.is_exclusive());
    }

    #[test]
    fn response_omits_empty_fields() {
        let line = String::from_utf8(encode_line(&Response {
            accepted: true,
            ..Response::default()
        }))
        .unwrap();
        assert_eq!(line, "{\"ok\":true,\"accepted\":true}\n");
    }

    #[test]
    fn ping_and_shutdown_parse_without_payload() {
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"command":"ping"}"#).unwrap(),
            Request::Ping
        ));
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"command":"shutdown"}"#).unwrap(),
            Request::Shutdown
        ));
    }
}
