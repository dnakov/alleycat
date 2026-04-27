//! `config/read`, `config/value/write`, `config/batchWrite`,
//! `configRequirements/read`. The bridge composes pi settings + bridge-only
//! keys; deep config introspection is left as opaque values.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadParams {
    #[serde(default)]
    pub include_layers: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Effective config response. The `config` value is the merged tree the
/// bridge surfaces (pi settings + bridge defaults). We keep it opaque so the
/// bridge does not need to know every codex setting key.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    pub config: Value,
    pub origins: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<Value>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeStrategy {
    Replace,
    Upsert,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WriteStatus {
    Ok,
    OkOverridden,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigValueWriteParams {
    pub key_path: String,
    pub value: Value,
    pub merge_strategy: MergeStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigEdit {
    pub key_path: String,
    pub value: Value,
    pub merge_strategy: MergeStrategy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigBatchWriteParams {
    pub edits: Vec<ConfigEdit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    #[serde(default)]
    pub reload_user_config: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWriteResponse {
    pub status: WriteStatus,
    pub version: String,
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden_metadata: Option<Value>,
}

/// `configRequirements/read` response. Bridge reports a minimal envelope.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRequirementsReadResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<Value>,
}
