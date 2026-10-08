//! Mandatory admission for every managed execution entry point.
//!
//! Legacy host drivers cannot satisfy Firecracker custody. Until verified
//! runner dispatch is installed, reject before resolving source paths,
//! creating intents, copying authentication, or spawning a process.

use super::WebState;
use super::sessions::SessionRuntime;
use super::workcells_support::{TypedError, typed_error};
use axum::http::StatusCode;
use axum::response::Response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchDenial {
    HostExecution,
    InvalidRuntime,
    SandboxUnavailable,
}

impl LaunchDenial {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::HostExecution => "host_launch_denied",
            Self::InvalidRuntime => "managed_runtime_invalid",
            Self::SandboxUnavailable => "sandbox_unavailable",
        }
    }
    fn status(self) -> StatusCode {
        match self {
            Self::HostExecution => StatusCode::FORBIDDEN,
            Self::InvalidRuntime => StatusCode::UNPROCESSABLE_ENTITY,
            Self::SandboxUnavailable => StatusCode::FAILED_DEPENDENCY,
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::HostExecution => "managed agents and terminals require a Firecracker runner",
            Self::InvalidRuntime => "the configured managed runtime is unknown",
            Self::SandboxUnavailable => "a verified Firecracker runner is unavailable",
        }
    }
    fn response(self, purpose: &str) -> Response {
        typed_error(TypedError {
            status: self.status(),
            code: self.code(),
            purpose,
            reason: self.message(),
            common_fixes: &[
                "configure an enrolled Firecracker runner and a qualified profile",
                "retry after runner readiness; host execution is denied",
            ],
            docs_url: "docs/workcell.md#managed-firecracker-admission",
            repair_hint: "check runner readiness; never switch to a host execution backend",
            message: self.message(),
        })
    }
}

/// No selection authorizes a legacy host driver. Missing Firecracker is
/// an error; an unknown selector cannot become Auto.
pub(super) fn admission(runtime: SessionRuntime) -> Result<(), LaunchDenial> {
    Err(match runtime {
        SessionRuntime::Firecracker => LaunchDenial::SandboxUnavailable,
        SessionRuntime::Invalid => LaunchDenial::InvalidRuntime,
        SessionRuntime::Auto | SessionRuntime::Docker | SessionRuntime::Native => {
            LaunchDenial::HostExecution
        }
    })
}

pub(super) fn require(state: &WebState, purpose: &str) -> Result<(), Box<Response>> {
    admission(state.session_runtime.runtime).map_err(|denial| Box::new(denial.response(purpose)))
}

pub(super) fn require_mcp(state: &WebState, purpose: &str) -> Result<(), String> {
    admission(state.session_runtime.runtime).map_err(|denial| {
        serde_json::json!({
            "code": denial.code(), "purpose": purpose, "message": denial.message(),
            "docs_url": "docs/workcell.md#managed-firecracker-admission",
            "jeryu_repair_hint": "check runner readiness; never switch to a host execution backend"
        })
        .to_string()
    })
}

#[cfg(test)]
mod tests;
