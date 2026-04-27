//! `account/*` methods. The bridge always synthesizes "no account" responses
//! (pi handles model auth itself) but the wire shapes still need to round-trip
//! cleanly so any codex client that probes them gets a valid reply.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// `Account` is a tagged enum on `type`. The bridge never emits the populated
/// variants — but to round-trip both directions we keep them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Account {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {},
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt {
        email: String,
        /// Codex emits a string (`"plus"`, `"pro"`, ...). Keep opaque so the
        /// bridge does not need a frozen enum.
        plan_type: Value,
    },
    #[serde(rename = "amazonBedrock", rename_all = "camelCase")]
    AmazonBedrock {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountParams {
    #[serde(default)]
    pub refresh_token: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountResponse {
    pub account: Option<Account>,
    pub requires_openai_auth: bool,
}

/// `account/login/start` params.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum LoginAccountParams {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {
        #[serde(rename = "apiKey")]
        api_key: String,
    },
    #[serde(rename = "chatgpt")]
    Chatgpt,
    #[serde(rename = "chatgptDeviceCode")]
    ChatgptDeviceCode,
    #[serde(rename = "chatgptAuthTokens", rename_all = "camelCase")]
    ChatgptAuthTokens {
        access_token: String,
        chatgpt_account_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chatgpt_plan_type: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LoginAccountResponse {
    #[serde(rename = "apiKey", rename_all = "camelCase")]
    ApiKey {},
    #[serde(rename = "chatgpt", rename_all = "camelCase")]
    Chatgpt { login_id: String, auth_url: String },
    #[serde(rename = "chatgptDeviceCode", rename_all = "camelCase")]
    ChatgptDeviceCode {
        login_id: String,
        verification_url: String,
        user_code: String,
    },
    #[serde(rename = "chatgptAuthTokens", rename_all = "camelCase")]
    ChatgptAuthTokens {},
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CancelLoginAccountParams {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CancelLoginAccountStatus {
    Canceled,
    NotFound,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CancelLoginAccountResponse {
    pub status: CancelLoginAccountStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogoutAccountResponse {}

/// Rate-limit envelope. `account/rateLimits/read` returns it; we never
/// populate it from pi, so default = empty.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GetAccountRateLimitsResponse {
    pub rate_limits: RateLimitSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits_by_limit_id: Option<std::collections::HashMap<String, RateLimitSnapshot>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditsSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub used_percent: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// `feedback/upload` request and response — pass-through, bridge will accept
/// and discard.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackUploadParams {
    pub classification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Other fields the bridge does not care about.
    #[serde(flatten)]
    pub additional: std::collections::HashMap<String, Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackUploadResponse {}

/// Server→client request used when codex auth tokens go stale. The bridge
/// never originates this — included for completeness of the wire types.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatgptAuthTokensRefreshParams {
    pub reason: ChatgptAuthTokensRefreshReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_account_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChatgptAuthTokensRefreshReason {
    Unauthorized,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatgptAuthTokensRefreshResponse {
    pub access_token: String,
    pub chatgpt_account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_plan_type: Option<String>,
}
