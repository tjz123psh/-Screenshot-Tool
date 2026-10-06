//! Worker-side bridge to the installed release manager. No GTK or shell parsing.
use std::{process::Stdio, time::Duration};

pub(super) struct Status {
    pub enabled: bool,
    pub message: String,
}

pub(super) fn request(enabled: Option<bool>) -> Result<Status, String> {
    // Never fall back to another installation found on PATH.
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("vellum")))
        .filter(|path| vellum_core::proc::is_executable(path))
        .ok_or("未找到同版本管理程序，请检查安装")?;
    let mut command = vellum_core::proc::command(program);
    command.args(["release", "autostart", "--json"]);
    if let Some(enabled) = enabled {
        command.args(["--enabled", if enabled { "true" } else { "false" }]);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "无法启动自启管理程序")?;
    let output = vellum_core::proc::wait(child, Duration::from_secs(60))
        .ok_or("自启管理超时；状态未确认，请重新保存重试")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail: String = detail
            .chars()
            .filter(|c| !c.is_control())
            .take(240)
            .collect();
        return Err(if detail.is_empty() {
            "无法确认开机自启状态，请检查安装或登录桌面后重试".into()
        } else {
            detail
        });
    }
    decode(&output.stdout)
}

fn decode(bytes: &[u8]) -> Result<Status, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "自启管理程序返回格式无效")?;
    if value.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("自启管理未完成".into());
    }
    let enabled = match value.get("state").and_then(serde_json::Value::as_str) {
        Some("autostart-enabled" | "autostart-mixed") => true,
        Some("autostart-disabled") => false,
        _ => return Err("未确认自启状态，请先修复安装".into()),
    };
    Ok(Status {
        enabled,
        message: value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("自启状态已确认")
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_confirmed_startup_reports_are_accepted() {
        assert!(
            !decode(br#"{"success":true,"state":"autostart-disabled"}"#)
                .unwrap()
                .enabled
        );
        assert!(
            decode(br#"{"success":true,"state":"autostart-mixed"}"#)
                .unwrap()
                .enabled
        );
        assert!(decode(br#"{"success":true,"state":"installed-pending-activation"}"#).is_err());
        assert!(decode(br#"{"success":false,"state":"autostart-enabled"}"#).is_err());
    }
}
