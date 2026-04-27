//! `initialize` / `initialized` handlers + the `account/*` and
//! `feedback/upload` no-ops. Pi handles model auth itself, so the bridge
//! always reports "no account, no auth required".

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::codex_proto as p;
use crate::state::ConnectionState;

/// Bridge user agent string included in `initialize` responses.
pub const USER_AGENT: &str = concat!("alleycat-pi-bridge/", env!("CARGO_PKG_VERSION"));

/// Default codex_home for the bridge: `$XDG_CONFIG_HOME/codex/pi-bridge` on
/// Linux, equivalent on macOS/Windows. Can be overridden by the caller of
/// `handle_initialize`.
pub fn default_codex_home() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "codex") {
        dirs.config_dir().join("pi-bridge")
    } else {
        PathBuf::from(".codex/pi-bridge")
    }
}

pub fn handle_initialize(
    state: &Arc<ConnectionState>,
    params: p::InitializeParams,
    codex_home: &std::path::Path,
) -> p::InitializeResponse {
    state.set_capabilities(
        Some(params.client_info.name.clone()),
        params.client_info.title.clone(),
        Some(params.client_info.version.clone()),
        params.capabilities.as_ref(),
    );

    p::InitializeResponse {
        user_agent: USER_AGENT.to_string(),
        codex_home: codex_home.to_string_lossy().into_owned(),
        platform_family: platform_family().to_string(),
        platform_os: platform_os().to_string(),
    }
}

/// `initialized` is a one-shot notification with no params. The bridge has
/// nothing meaningful to do — capabilities are already set from
/// `initialize` — but we accept it so the codex test client does not see an
/// "unknown method" error.
pub fn handle_initialized(_state: &Arc<ConnectionState>) {
    tracing::debug!("client sent initialized; connection ready");
}

// === account/* ============================================================

pub fn handle_account_read(
    _state: &Arc<ConnectionState>,
    _params: p::GetAccountParams,
) -> p::GetAccountResponse {
    p::GetAccountResponse {
        account: None,
        requires_openai_auth: false,
    }
}

pub fn handle_account_rate_limits_read(
    _state: &Arc<ConnectionState>,
) -> p::GetAccountRateLimitsResponse {
    p::GetAccountRateLimitsResponse::default()
}

/// We never actually start a login. The simplest valid reply is the
/// `apiKey` shape (no metadata required).
pub fn handle_account_login_start(
    _state: &Arc<ConnectionState>,
    _params: p::LoginAccountParams,
) -> Result<p::LoginAccountResponse> {
    Ok(p::LoginAccountResponse::ApiKey {})
}

pub fn handle_account_login_cancel(
    _state: &Arc<ConnectionState>,
    _params: p::CancelLoginAccountParams,
) -> p::CancelLoginAccountResponse {
    p::CancelLoginAccountResponse {
        status: p::CancelLoginAccountStatus::NotFound,
    }
}

pub fn handle_account_logout(_state: &Arc<ConnectionState>) -> p::LogoutAccountResponse {
    p::LogoutAccountResponse::default()
}

pub fn handle_feedback_upload(
    _state: &Arc<ConnectionState>,
    params: p::FeedbackUploadParams,
) -> p::FeedbackUploadResponse {
    tracing::info!(
        classification = %params.classification,
        reason = ?params.reason,
        "feedback/upload received (discarded by pi-bridge)"
    );
    p::FeedbackUploadResponse::default()
}

// === platform helpers =====================================================

fn platform_family() -> &'static str {
    if cfg!(target_family = "windows") {
        "windows"
    } else {
        "unix"
    }
}

fn platform_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        std::env::consts::OS
    }
}
