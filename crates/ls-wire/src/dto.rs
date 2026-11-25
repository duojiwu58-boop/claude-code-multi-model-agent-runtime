//! Explicit protocol DTOs for verified Antigravity 2.35.0 RPCs and safe
//! redaction helpers for diagnostics.

use crate::{InstanceSummary, PROD_UI_VERBOSITY};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Redacts sensitive keys (`token`, `password`, `credential`, `email`,
/// `authorization`, `signature`) and OAuth/Bearer strings before logging or
/// returning diagnostic payloads. Boolean fields remain intact.
pub fn redact_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let lower = k.to_ascii_lowercase();
                    let is_secret = lower.contains("token")
                        || lower.contains("password")
                        || lower.contains("credential")
                        || lower.contains("email")
                        || lower.contains("authorization")
                        || lower.contains("signature")
                        || ["secret", "api_key", "apikey", "cookie", "private_key", "privatekey", "jwt"]
                            .iter().any(|key| lower.contains(key));
                    let clean = if is_secret && !v.is_boolean() {
                        json!("[REDACTED]")
                    } else {
                        redact_value(v)
                    };
                    (k, clean)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            if ["accounts.google.com", "oauth", "bearer ", "ya29.", "aiza"]
                .iter().any(|marker| lower.contains(marker)) {
                json!("[REDACTED auth text]")
            } else { Value::String(s) }
        }
        other => other,
    }
}

#[cfg(test)]
mod redaction_regressions {
    use super::*;
    #[test]
    fn covers_secret_aliases_and_auth_text_case() {
        let value = redact_value(json!({"api_key":"private", "privateKey":"private", "cookie":"private", "JWT":"private", "secret":"private", "hasToken":true}));
        for key in ["api_key", "privateKey", "cookie", "JWT", "secret"] { assert_eq!(value[key], "[REDACTED]"); }
        assert_eq!(value["hasToken"], true);
        for text in ["bearer private", "OAUTH private", "ya29.private", "AIzaPrivate"] {
            assert_eq!(redact_value(json!(text)), "[REDACTED auth text]");
        }
        assert_eq!(redact_value(json!("ordinary error")), "ordinary error");
    }
}

/// Page bounds for `StreamAgentStateUpdates` and `RequestAgentStatePageUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_index_exclusive: Option<i64>,
}

impl PageBounds {
    pub const fn range(start: i64, end_exclusive: i64) -> Self {
        Self {
            start_index: Some(start),
            end_index_exclusive: Some(end_exclusive),
        }
    }

    pub const fn default_initial() -> Self {
        Self {
            start_index: Some(-50),
            end_index_exclusive: None,
        }
    }
}

/// Single model entry from `GetAvailableModels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub model: String,
    pub label: Option<String>,
    /// Preserves unknown fields from the server response for diagnostics.
    pub extra: Value,
}

/// Live model catalog returned by `GetAvailableModels` (never hardcoded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub default_agent_model_id: String,
    pub models: Vec<ModelEntry>,
}

impl ModelCatalog {
    pub fn from_rpc_response(value: &Value) -> Option<Self> {
        let resp = value.get("response")?;
        let default_agent_model_id = resp
            .get("defaultAgentModelId")
            .and_then(Value::as_str)?
            .to_owned();
        let models_obj = resp.get("models").and_then(Value::as_object)?;
        let mut models = Vec::with_capacity(models_obj.len());
        for (id, entry) in models_obj {
            let Some(model) = entry.get("model").and_then(Value::as_str) else {
                continue;
            };
            let label = entry
                .get("label")
                .or_else(|| entry.get("displayName"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            models.push(ModelEntry {
                id: id.clone(),
                model: model.to_owned(),
                label,
                extra: redact_value(entry.clone()),
            });
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Some(Self {
            default_agent_model_id,
            models,
        })
    }

    /// Resolves either an explicit model ID (or model enum string) or the server's
    /// `defaultAgentModelId` into `(model_id, plan_model_enum)`.
    pub fn resolve(&self, requested: Option<&str>) -> Option<(String, String)> {
        let key = requested
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.default_agent_model_id);
        self.models
            .iter()
            .find(|m| m.id == key || m.model == key)
            .map(|m| (m.id.clone(), m.model.clone()))
    }
}

/// Sanitized authentication summary (strictly booleans and key names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSummary {
    pub auth_status_bools: BTreeMap<String, bool>,
    pub auth_status_keys: Vec<String>,
    pub has_auth_token_bools: BTreeMap<String, bool>,
    pub has_auth_token_keys: Vec<String>,
}

impl AuthSummary {
    pub fn from_responses(auth_status: &Value, has_auth_token: &Value) -> Self {
        let (auth_status_bools, auth_status_keys) = extract_bools_and_keys(auth_status);
        let (has_auth_token_bools, has_auth_token_keys) = extract_bools_and_keys(has_auth_token);
        Self {
            auth_status_bools,
            auth_status_keys,
            has_auth_token_bools,
            has_auth_token_keys,
        }
    }
}

fn extract_bools_and_keys(value: &Value) -> (BTreeMap<String, bool>, Vec<String>) {
    let mut bools = BTreeMap::new();
    let mut keys = Vec::new();
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            keys.push(k.clone());
            if let Some(b) = v.as_bool() {
                bools.insert(k.clone(), b);
            }
        }
        keys.sort();
    }
    (bools, keys)
}

/// Result of verifying TLS trust anchor rejection and CSRF 401 enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityBoundaryReport {
    pub untrusted_tls_rejected: bool,
    pub missing_csrf_status: u16,
    pub wrong_csrf_status: u16,
}

/// Structured result of read-only health, auth, capability, and model discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadonlyCheckReport {
    pub instance: InstanceSummary,
    pub capabilities_keys: Vec<String>,
    pub auth: AuthSummary,
    pub default_agent_model_id: String,
    pub model_count: usize,
    pub security_checks: Option<SecurityBoundaryReport>,
}

/// Parameters for `StartCascade`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCascadeRequest {
    pub cascade_id: String,
    pub workspace_uri: String,
}

impl StartCascadeRequest {
    pub fn to_wire_json(&self) -> Value {
        json!({
            "cascadeId": self.cascade_id,
            "workspaceUris": [self.workspace_uri],
            "source": "CORTEX_TRAJECTORY_SOURCE_CASCADE_CLIENT",
            "trajectoryType": "CORTEX_TRAJECTORY_TYPE_CASCADE",
            "interactionMode": "INTERACTION_MODE_INTERACTIVE"
        })
    }
}

/// Parameters for `SendUserCascadeMessage`.
#[derive(Debug, Clone, PartialEq)]
pub struct SendMessageOptions {
    pub cascade_id: String,
    pub text: String,
    /// Explicit user text blocks, preserving boundaries and exact contents.
    pub text_items: Option<Vec<String>>,
    pub images: Vec<Value>,
    pub plan_model: String,
    /// Stable client-side correlation ID (never assumed to give server exactly-once).
    pub client_request_id: String,
    /// Optional wire `clientMessageId` when explicitly enabled.
    pub client_message_id: Option<String>,
    /// Optional `cascadeConfig.plannerConfig.toolConfig` overrides (e.g. review preset).
    pub tool_config: Option<Value>,
}

impl SendMessageOptions {
    pub fn to_wire_json(&self) -> Value {
        let mut planner_config = Map::new();
        planner_config.insert(
            "planModel".to_owned(),
            Value::String(self.plan_model.clone()),
        );
        if let Some(tool_config) = &self.tool_config {
            planner_config.insert("toolConfig".to_owned(), tool_config.clone());
        }
        let mut body = Map::new();
        body.insert(
            "cascadeId".to_owned(),
            Value::String(self.cascade_id.clone()),
        );
        let items:Vec<Value>=self.text_items.as_ref().map(|items|items.iter().map(|text|json!({"text":text})).collect())
            .unwrap_or_else(||vec![json!({"text":self.text})]);
        body.insert("items".to_owned(), json!(items));
        if !self.images.is_empty() { body.insert("media".to_owned(), json!(self.images)); }
        // Official builtin/default agent, interactive HUB surface (2.35.0 mob/nob).
        body.insert(
            "customAgentSpec".to_owned(),
            json!({"builtinAgent": {
                "defaultAgent": {"isInteractive": true, "surface": "AGENT_SURFACE_HUB"}
            }}),
        );
        body.insert(
            "cascadeConfig".to_owned(),
            json!({ "plannerConfig": Value::Object(planner_config) }),
        );
        if let Some(msg_id) = &self.client_message_id {
            body.insert("clientMessageId".to_owned(), Value::String(msg_id.clone()));
        }
        Value::Object(body)
    }
}

/// Parameters for `StreamAgentStateUpdates` using official `PROD_UI` verbosity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSubscribeRequest {
    pub conversation_id: String,
    pub subscriber_id: String,
    pub initial_steps_page_bounds: PageBounds,
}

impl StreamSubscribeRequest {
    pub fn to_wire_json(&self) -> Value {
        json!({
            "conversationId": self.conversation_id,
            "subscriberId": self.subscriber_id,
            "initialStepsPageBounds": self.initial_steps_page_bounds,
            "initialGeneratorMetadatasPageBounds": self.initial_steps_page_bounds,
            "initialExecutorMetadatasPageBounds": self.initial_steps_page_bounds,
            "trajectoryVerbosity": PROD_UI_VERBOSITY
        })
    }
}

/// Parameters for `RequestAgentStatePageUpdate` bound to an active `subscriberId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageUpdateRequest {
    pub conversation_id: String,
    pub subscriber_id: String,
    pub step_page_bounds: PageBounds,
}

impl PageUpdateRequest {
    pub fn to_wire_json(&self) -> Value {
        json!({
            "conversationId": self.conversation_id,
            "subscriberId": self.subscriber_id,
            "stepPageBounds": self.step_page_bounds,
            "generatorMetadatasPageBounds": self.step_page_bounds,
            "executorMetadatasPageBounds": self.step_page_bounds
        })
    }
}

/// Explicit permission scope for `HandleCascadeUserInteraction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionScope {
    Once,
}

impl PermissionScope {
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Once => "PERMISSION_SCOPE_ONCE",
        }
    }
}

/// Verified answer payload for a single question inside `askQuestion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestionAnswer {
    #[serde(default)]
    pub selected_option_ids: Vec<String>,
    #[serde(default)]
    pub write_in_response: Option<String>,
}

/// Typed reply body for `HandleCascadeUserInteraction`.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionReply {
    FilePermission {
        allow: bool,
        absolute_path_uri: String,
    },
    Elicitation {
        action: String,
        content: Value,
    },
    Permission {
        allow: bool,
        scope: PermissionScope,
    },
    RunCommand {
        confirm: bool,
    },
    AskQuestion {
        /// Full cloned question objects populated with `selectedOptionIds` and `writeInResponse`.
        responses: Vec<Value>,
        cancelled: bool,
    },
}

impl InteractionReply {
    pub fn to_wire_json(&self, cascade_id: &str, trajectory_id: &str, step_index: u64) -> Value {
        let mut interaction = Map::new();
        interaction.insert(
            "trajectoryId".to_owned(),
            Value::String(trajectory_id.to_owned()),
        );
        interaction.insert("stepIndex".to_owned(), json!(step_index));
        match self {
            Self::FilePermission {
                allow,
                absolute_path_uri,
            } => {
                interaction.insert(
                    "filePermission".into(),
                    json!({"allow":allow,
                    "scope":"PERMISSION_SCOPE_ONCE","absolutePathUri":absolute_path_uri}),
                );
            }
            Self::Elicitation { action, content } => {
                interaction.insert(
                    "elicitation".into(),
                    json!({"action":action,"contentJson":content.to_string()}),
                );
            }
            Self::Permission { allow, scope } => {
                interaction.insert(
                    "permission".to_owned(),
                    json!({
                        "allow": allow,
                        "scope": scope.as_wire_str(),
                    }),
                );
            }
            Self::RunCommand { confirm } => {
                interaction.insert("runCommand".to_owned(), json!({ "confirm": confirm }));
            }
            Self::AskQuestion {
                responses,
                cancelled,
            } => {
                interaction.insert(
                    "askQuestion".to_owned(),
                    json!({
                        "responses": responses,
                        "cancelled": cancelled,
                    }),
                );
            }
        }
        json!({
            "cascadeId": cascade_id,
            "interaction": Value::Object(interaction),
        })
    }
}

/// Explicit stop scope distinguishing invocation, session background tasks, and owned tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopScope {
    Invocation,
    Background,
    Tree,
}

impl StopScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Background => "background",
            Self::Tree => "tree",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_and_auth_strings_while_preserving_booleans() {
        let input = json!({
            "csrfToken": "secret",
            "hasAuthToken": true,
            "userEmail": "user@example.com",
            "url": "https://accounts.google.com/o/oauth2/auth",
            "nested": { "authorization": "Bearer xyz", "ok": false }
        });
        let clean = redact_value(input);
        assert_eq!(clean["csrfToken"], "[REDACTED]");
        assert_eq!(clean["hasAuthToken"], true);
        assert_eq!(clean["userEmail"], "[REDACTED]");
        assert_eq!(clean["url"], "[REDACTED auth text]");
        assert_eq!(clean["nested"]["authorization"], "[REDACTED]");
        assert_eq!(clean["nested"]["ok"], false);
    }

    #[test]
    fn resolves_models_from_live_catalog() {
        let raw = json!({
            "response": {
                "defaultAgentModelId": "m-default",
                "models": {
                    "m-default": { "model": "MODEL_ALPHA", "label": "Alpha" },
                    "m-alt": { "model": "MODEL_BETA" }
                }
            }
        });
        let catalog = ModelCatalog::from_rpc_response(&raw).unwrap();
        assert_eq!(
            catalog.resolve(None),
            Some(("m-default".into(), "MODEL_ALPHA".into()))
        );
        assert_eq!(
            catalog.resolve(Some("m-alt")),
            Some(("m-alt".into(), "MODEL_BETA".into()))
        );
        assert_eq!(
            catalog.resolve(Some("MODEL_BETA")),
            Some(("m-alt".into(), "MODEL_BETA".into()))
        );
        assert_eq!(catalog.resolve(Some("nonexistent")), None);
    }
}
