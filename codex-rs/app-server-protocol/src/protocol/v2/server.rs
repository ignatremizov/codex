use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerReadParams {}

/// Startup credential selection, without credentials or absolute auth paths.
///
/// The opaque ID describes host-local configuration, not an authenticated
/// account, and cannot be used to compare credential paths across hosts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerAuthProfile {
    pub profile_opaque_id: String,
    pub display_label: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ServerReadResponse {
    pub auth_profile: ServerAuthProfile,
}
