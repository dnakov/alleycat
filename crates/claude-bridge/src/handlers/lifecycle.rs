//! `initialize` / `initialized` plus the `account/*` and `feedback/upload`
//! shapes. The bridge does not own claude's authentication (the user runs
//! `claude /login` themselves), so the account methods are stubs that report
//! "no account, no auth required".

use std::path::PathBuf;
use std::sync::Arc;

use alleycat_codex_proto as p;
use anyhow::Result;

use crate::state::ConnectionState;

/// Bridge user agent string included in `initialize` responses.
pub const USER_AGENT: &str = concat!("alleycat-claude-bridge/", env!("CARGO_PKG_VERSION"));

/// Default codex_home for the bridge: `$XDG_CONFIG_HOME/codex/claude-bridge`
/// on Linux, equivalent on macOS/Windows. Falls back to `.codex/claude-bridge`
/// when no config dir is resolvable.
pub fn default_codex_home() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "codex") {
        dirs.config_dir().join("claude-bridge")
    } else {
        PathBuf::from(".codex/claude-bridge")
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

/// `initialized` is a one-shot notification with no params.
pub fn handle_initialized(_state: &Arc<ConnectionState>) {
    tracing::debug!("client sent initialized; connection ready");
}

// === account/* ============================================================
//
// claude's auth lives outside the bridge: the user runs `claude /login`
// (or sets `ANTHROPIC_API_KEY`) and the spawned claude process picks it up.
// The bridge has nothing to authenticate, so every account method synthesizes
// the "no account, no auth required" answer codex clients tolerate.

pub fn handle_account_read(
    _state: &Arc<ConnectionState>,
    _params: p::GetAccountParams,
) -> p::GetAccountResponse {
    // claude-code authenticates via Anthropic API key or OAuth token; either
    // way the codex-side account shape is `ApiKey` (codex reserves
    // `Chatgpt` for OpenAI sign-in only).
    p::GetAccountResponse {
        account: Some(p::Account::ApiKey {}),
        requires_openai_auth: false,
    }
}

/// Surface whatever `rate_limit_event` the claude event pump last saw. Until
/// any process emits one, the response is the empty default — codex clients
/// treat that as "no rate-limit metadata yet".
pub fn handle_account_rate_limits_read(
    state: &Arc<ConnectionState>,
) -> p::GetAccountRateLimitsResponse {
    let info = state.caches().rate_limit_info;
    let Some(info) = info else {
        return p::GetAccountRateLimitsResponse::default();
    };

    let window = p::RateLimitWindow {
        used_percent: 0,
        window_duration_mins: None,
        resets_at: info.resets_at,
    };
    let snapshot = p::RateLimitSnapshot {
        limit_id: info.rate_limit_type.clone(),
        limit_name: info.rate_limit_type.clone(),
        primary: Some(window),
        secondary: None,
        credits: None,
        plan_type: None,
        rate_limit_reached_type: (info.status != "allowed")
            .then(|| serde_json::Value::String(info.status.clone())),
    };
    p::GetAccountRateLimitsResponse {
        rate_limits: snapshot,
        rate_limits_by_limit_id: None,
    }
}

/// We never actually start a login flow. The simplest valid reply is the
/// `apiKey` shape; the codex client treats this as "auth completed
/// synchronously, nothing to poll".
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
        "feedback/upload received (discarded by claude-bridge)"
    );
    p::FeedbackUploadResponse::default()
}

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
