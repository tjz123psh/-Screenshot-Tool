//! Version gates for action IPC. This is compatibility isolation, not peer
//! authentication: the user-private Unix socket remains the trust boundary.

use super::{PeerIdentity, Response};

pub const IDENTITY_UNAVAILABLE: &str =
    "服务身份无法确认；请运行版本修复或重启服务后重试，未继续自动执行或重试";

pub fn current_identity() -> PeerIdentity {
    let build = vellum_core::build_info::current();
    let managed = vellum_core::build_info::is_managed_location();
    let verified = !managed
        || vellum_core::build_info::managed_release_id().as_deref()
            == Some(build.build_id.as_str());
    PeerIdentity {
        build_id: verified.then_some(build.build_id),
        ipc_schema: Some(build.ipc_schema),
        managed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    UnknownManaged,
    InstallMode,
    Build,
    Schema,
}

impl IdentityError {
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownManaged => "managed-identity-required",
            Self::InstallMode => "install-mode-mismatch",
            Self::Build => "build-mismatch",
            Self::Schema => "ipc-schema-mismatch",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::UnknownManaged => {
                "托管版本身份缺失或安装校验失败；请修复安装或重启服务，未继续自动执行或重试"
            }
            Self::InstallMode => {
                "开发目录与托管安装不能共用截图任务；请使用同一安装版本或停止旧服务"
            }
            Self::Build => {
                "客户端与截图服务版本不一致；请完成版本切换或重启服务，未继续自动执行或重试"
            }
            Self::Schema => {
                "客户端与截图服务协议不兼容；请修复安装或重启服务，未继续自动执行或重试"
            }
        }
    }
    pub fn response(self) -> Response {
        refusal(self.code(), self.message())
    }
}

/// Keep running=true in compatibility refusals for pre-v1 ctl clients, which
/// only know that bit and would otherwise silently exec a different version.
/// Status/Ping still report the daemon's actual lifecycle state independently.
pub fn refusal(code: &str, message: &str) -> Response {
    Response {
        ok: false,
        running: true,
        no_fallback: true,
        error_code: Some(code.into()),
        message: Some(message.into()),
        ..Response::default()
    }
}

fn known_managed(identity: &PeerIdentity) -> bool {
    identity.build_id.as_deref().is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 96
            && id != "unknown"
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }) && identity.ipc_schema.is_some()
}

/// Development-to-development is intentionally legacy-compatible. Either
/// managed endpoint makes mode, known identity, build and schema exact gates.
pub fn check_action_identity(
    local: &PeerIdentity,
    peer: Option<&PeerIdentity>,
) -> Result<(), IdentityError> {
    if local.managed && !known_managed(local) {
        return Err(IdentityError::UnknownManaged);
    }
    let Some(peer) = peer else {
        return if local.managed {
            Err(IdentityError::UnknownManaged)
        } else {
            Ok(())
        };
    };
    if local.managed != peer.managed {
        return Err(IdentityError::InstallMode);
    }
    if local.managed {
        if !known_managed(peer) {
            return Err(IdentityError::UnknownManaged);
        }
        if local.build_id != peer.build_id {
            return Err(IdentityError::Build);
        }
    }
    // Old development peers without schema still work; identified incompatible
    // schemas do not. No environment toggle can relax a managed gate.
    if let (Some(local), Some(peer)) = (local.ipc_schema, peer.ipc_schema)
        && local != peer
    {
        return Err(IdentityError::Schema);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(managed: bool, build: &str) -> PeerIdentity {
        PeerIdentity {
            managed,
            build_id: Some(build.into()),
            ipc_schema: Some(1),
        }
    }
    #[test]
    fn managed_requires_exact_known_mode_build_and_schema() {
        let local = identity(true, "0.2.0-synthetic-a");
        assert_eq!(check_action_identity(&local, Some(&local)), Ok(()));
        assert_eq!(
            check_action_identity(&local, None),
            Err(IdentityError::UnknownManaged)
        );
        assert_eq!(
            check_action_identity(&local, Some(&identity(false, "0.2.0-synthetic-a"))),
            Err(IdentityError::InstallMode)
        );
        assert_eq!(
            check_action_identity(&local, Some(&identity(true, "0.2.0-synthetic-b"))),
            Err(IdentityError::Build)
        );
        let mut unknown = local.clone();
        unknown.build_id = None;
        assert_eq!(
            check_action_identity(&local, Some(&unknown)),
            Err(IdentityError::UnknownManaged)
        );
        assert_eq!(
            check_action_identity(&unknown, Some(&local)),
            Err(IdentityError::UnknownManaged)
        );
        let mut wrong_schema = local.clone();
        wrong_schema.ipc_schema = Some(2);
        assert_eq!(
            check_action_identity(&local, Some(&wrong_schema)),
            Err(IdentityError::Schema)
        );
    }
    #[test]
    fn development_compatibility_never_crosses_managed_boundary() {
        let local = identity(false, "development-a");
        assert_eq!(check_action_identity(&local, None), Ok(()));
        assert_eq!(
            check_action_identity(&local, Some(&identity(false, "development-b"))),
            Ok(())
        );
        assert_eq!(
            check_action_identity(&local, Some(&identity(true, "development-a"))),
            Err(IdentityError::InstallMode)
        );
    }
    #[test]
    fn malformed_managed_ids_are_unknown_and_never_echoed() {
        let local = identity(true, "0.2.0-synthetic-a");
        for build in [
            "unknown",
            "../synthetic-secret",
            "https://u:synthetic-secret@private.invalid",
            "",
        ] {
            let error = check_action_identity(&local, Some(&identity(true, build))).unwrap_err();
            let response = error.response();
            assert!(response.running && response.no_fallback && !response.accepted);
            assert!(!format!("{response:?}").contains("synthetic-secret"));
        }
    }
}
