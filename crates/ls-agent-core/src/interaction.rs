//! Live pending-interaction inspection and pre-submit validation registry.
//!
//! Enforces `docs/engine-contract.md` section 4.4:
//! - Only responds when the target step is still `CORTEX_STEP_STATUS_WAITING`
//!   with a non-empty `requestedInteraction`.
//! - Confirms `conversationId`, `trajectoryId`, `stepIndex`, and interaction
//!   kind match the live state.
//! - Keeps unsupported interaction types waiting and rejects submission with
//!   `NotSent` (never auto-approves).

use crate::state_store::AgentStateStore;
use ls_wire::{InteractionReply, PermissionScope, TransportErrorCategory};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "rawKey", rename_all = "camelCase")]
pub enum InteractionKind {
    FilePermission,
    Elicitation,
    Permission,
    RunCommand,
    AskQuestion,
    Unsupported(String),
}

impl InteractionKind {
    pub fn wire_key(&self) -> &str {
        match self {
            Self::FilePermission => "filePermission",
            Self::Elicitation => "elicitation",
            Self::Permission => "permission",
            Self::RunCommand => "runCommand",
            Self::AskQuestion => "askQuestion",
            Self::Unsupported(key) => key.as_str(),
        }
    }

    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }
}

/// Normalized live pending interaction extracted from `AgentStateStore`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInteraction {
    pub conversation_id: String,
    pub trajectory_id: String,
    pub step_index: u64,
    pub kind: String,
    pub supported: bool,
    pub spec: Value,
}

/// Answer entry for one question inside a generalized `askQuestion` interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestionAnswerInput {
    #[serde(default)]
    pub question_id: Option<String>,
    #[serde(default)]
    pub selected_option_ids: Vec<String>,
    #[serde(default)]
    pub write_in_response: Option<String>,
}

/// Caller decision for responding to a live pending interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum InteractionDecision {
    FilePermission {
        allow: bool,
    },
    Elicitation {
        action: String,
        content: Value,
    },
    Permission {
        allow: bool,
        #[serde(default = "default_permission_scope")]
        scope: PermissionScope,
    },
    RunCommand {
        confirm: bool,
    },
    AskQuestion {
        #[serde(default)]
        answers: Vec<AskQuestionAnswerInput>,
        #[serde(default)]
        cancelled: bool,
    },
}

fn default_permission_scope() -> PermissionScope {
    PermissionScope::Once
}

/// Explicit submission request matching UI/CLI intent to a specific live step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionSubmitRequest {
    pub conversation_id: String,
    pub trajectory_id: String,
    pub step_index: u64,
    pub kind: String,
    pub decision: InteractionDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionValidationError {
    pub category: TransportErrorCategory,
    pub reason_code: String,
    pub message_zh: String,
}

impl InteractionValidationError {
    fn not_sent(reason_code: impl Into<String>, message_zh: impl Into<String>) -> Self {
        Self {
            category: TransportErrorCategory::NotSent,
            reason_code: reason_code.into(),
            message_zh: message_zh.into(),
        }
    }
}

impl fmt::Display for InteractionValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.reason_code, self.message_zh)
    }
}

impl std::error::Error for InteractionValidationError {}

/// Inspects live state and validates user interaction replies before any RPC is sent.
#[derive(Default, Debug, Clone)]
pub struct InteractionRegistry {
    last_observed: Option<PendingInteraction>,
}

impl InteractionRegistry {
    /// Refreshes the tracked pending interaction from the current live state store.
    pub fn sync_from_state(&mut self, store: &AgentStateStore) -> Option<PendingInteraction> {
        let live = Self::extract_live_pending(store);
        self.last_observed = live.clone();
        live
    }

    pub fn current(&self) -> Option<&PendingInteraction> {
        self.last_observed.as_ref()
    }

    /// Extracts the current live waiting interaction from `AgentStateStore`.
    pub fn extract_live_pending(store: &AgentStateStore) -> Option<PendingInteraction> {
        let conversation_id = store.conversation_id()?.to_owned();
        let trajectory_id = store.trajectory_id()?.to_owned();
        for (&step_index, step) in &store.steps.values {
            if step.get("status").and_then(Value::as_str) != Some("CORTEX_STEP_STATUS_WAITING") {
                continue;
            }
            let Some(spec) = step.get("requestedInteraction") else {
                continue;
            };
            let Some(obj) = spec.as_object().filter(|m| !m.is_empty()) else {
                continue;
            };
            let kind = classify_interaction_kind(obj);
            return Some(PendingInteraction {
                conversation_id,
                trajectory_id,
                step_index,
                kind: kind.wire_key().to_owned(),
                supported: kind.is_supported()
                    && (kind != InteractionKind::Elicitation || elicitation_schema(spec).is_ok()),
                spec: spec.clone(),
            });
        }
        None
    }

    /// Validates `req` against live `store` and builds the wire `InteractionReply`.
    /// Never transmits anything; callers must only call `HandleCascadeUserInteraction`
    /// if this returns `Ok(...)`.
    pub fn validate_and_build_reply(
        store: &AgentStateStore,
        req: &InteractionSubmitRequest,
    ) -> Result<InteractionReply, InteractionValidationError> {
        let Some(live) = Self::extract_live_pending(store) else {
            return Err(InteractionValidationError::not_sent(
                "INTERACTION_EXPIRED",
                "当前没有实时待处理的交互请求（该卡片可能已过期或已被处理），未发送任何回复。",
            ));
        };

        if live.conversation_id != req.conversation_id
            || live.trajectory_id != req.trajectory_id
            || live.step_index != req.step_index
        {
            return Err(InteractionValidationError::not_sent(
                "INTERACTION_MISMATCH",
                format!(
                    "待处理交互标识不一致（实时 step={}，请求 step={}），已刷新状态且未发送回复。",
                    live.step_index, req.step_index
                ),
            ));
        }

        if !live.supported {
            return Err(InteractionValidationError::not_sent(
                "INTERACTION_UNSUPPORTED",
                format!(
                    "交互类型 '{}' 尚未在当前版本支持；请使用停止功能处理，绝不自动允许。",
                    live.kind
                ),
            ));
        }

        if live.kind != req.kind {
            return Err(InteractionValidationError::not_sent(
                "INTERACTION_KIND_MISMATCH",
                format!(
                    "交互类型不匹配（实时={}，请求={}），未发送任何回复。",
                    live.kind, req.kind
                ),
            ));
        }

        match (&req.decision, live.kind.as_str()) {
            (InteractionDecision::FilePermission { allow }, "filePermission") => {
                let path = live
                    .spec
                    .pointer("/filePermission/absolutePathUri")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| {
                        InteractionValidationError::not_sent(
                            "FILE_PERMISSION_PATH_MISSING",
                            "文件授权缺少实时目标路径，未提交。",
                        )
                    })?;
                Ok(InteractionReply::FilePermission {
                    allow: *allow,
                    absolute_path_uri: path.into(),
                })
            }
            (InteractionDecision::Elicitation { action, content }, "elicitation") => {
                let schema = elicitation_schema(&live.spec)?;
                if !["accept", "decline", "cancel"].contains(&action.as_str()) {
                    return Err(InteractionValidationError::not_sent(
                        "ELICITATION_ACTION_INVALID",
                        "无效的表单处理动作。",
                    ));
                }
                if action == "accept" {
                    validate_elicitation_content(&schema, content)?;
                }
                Ok(InteractionReply::Elicitation {
                    action: action.clone(),
                    content: if action == "accept" {
                        content.clone()
                    } else {
                        json!({})
                    },
                })
            }
            (InteractionDecision::Permission { allow, scope }, "permission") => {
                Ok(InteractionReply::Permission {
                    allow: *allow,
                    scope: *scope,
                })
            }
            (InteractionDecision::RunCommand { confirm }, "runCommand") => {
                Ok(InteractionReply::RunCommand { confirm: *confirm })
            }
            (InteractionDecision::AskQuestion { answers, cancelled }, "askQuestion") => {
                build_ask_question_reply(&live.spec, answers, *cancelled)
            }
            _ => Err(InteractionValidationError::not_sent(
                "INTERACTION_DECISION_MISMATCH",
                format!(
                    "提交的决策结构与实时交互类型 '{}' 不匹配，未发送任何回复。",
                    live.kind
                ),
            )),
        }
    }
}

fn classify_interaction_kind(obj: &serde_json::Map<String, Value>) -> InteractionKind {
    if obj.len() != 1 {
        InteractionKind::Unsupported("ambiguous".into())
    } else if obj.contains_key("filePermission") {
        InteractionKind::FilePermission
    } else if obj.contains_key("elicitation") {
        InteractionKind::Elicitation
    } else if obj.contains_key("permission") {
        InteractionKind::Permission
    } else if obj.contains_key("runCommand") {
        InteractionKind::RunCommand
    } else if obj.contains_key("askQuestion") {
        InteractionKind::AskQuestion
    } else {
        let first_key = obj
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());
        InteractionKind::Unsupported(first_key)
    }
}

pub fn elicitation_schema(spec: &Value) -> Result<Value, InteractionValidationError> {
    let invalid = || {
        InteractionValidationError::not_sent(
            "ELICITATION_SCHEMA_UNSUPPORTED",
            "此表单含未支持的类型或约束，保持等待，可停止任务。",
        )
    };
    let elic = spec.get("elicitation").ok_or_else(invalid)?;
    if !["", "form"].contains(&elic.get("mode").and_then(Value::as_str).unwrap_or_default()) {
        return Err(invalid());
    }
    let schema: Value = serde_json::from_str(
        elic.get("requestedSchemaJson")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    let obj = schema.as_object().ok_or_else(invalid)?;
    if schema["type"] != "object"
        || obj.keys().any(|k| {
            ![
                "type",
                "properties",
                "required",
                "title",
                "description",
                "additionalProperties",
            ]
            .contains(&k.as_str())
        })
        || schema
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err(invalid());
    }
    let props = schema["properties"].as_object().ok_or_else(invalid)?;
    if props.len() > 64 {
        return Err(invalid());
    }
    if let Some(required) = schema.get("required") {
        let list = required.as_array().ok_or_else(invalid)?;
        if list
            .iter()
            .any(|v| !v.as_str().is_some_and(|s| props.contains_key(s)))
        {
            return Err(invalid());
        }
    }
    for field in props.values() {
        let obj = field.as_object().ok_or_else(invalid)?;
        if !["string", "boolean", "integer", "number"]
            .contains(&field["type"].as_str().unwrap_or_default())
            || obj.keys().any(|k| {
                ![
                    "type",
                    "title",
                    "description",
                    "default",
                    "enum",
                    "minimum",
                    "maximum",
                    "minLength",
                    "maxLength",
                ]
                .contains(&k.as_str())
            })
        {
            return Err(invalid());
        }
        if let Some(values) = field.get("enum")
            && !values.as_array().is_some_and(|v| {
                !v.is_empty()
                    && v.iter()
                        .all(|x| matches!(x, Value::String(_) | Value::Bool(_) | Value::Number(_)))
            })
        {
            return Err(invalid());
        }
        for key in ["minimum", "maximum"] {
            if field.get(key).is_some_and(|v| !v.is_number()) {
                return Err(invalid());
            }
        }
        for key in ["minLength", "maxLength"] {
            if field.get(key).is_some_and(|v| v.as_u64().is_none()) {
                return Err(invalid());
            }
        }
    }
    Ok(schema)
}

fn validate_elicitation_content(
    schema: &Value,
    content: &Value,
) -> Result<(), InteractionValidationError> {
    let invalid = || {
        InteractionValidationError::not_sent(
            "ELICITATION_CONTENT_INVALID",
            "表单必填项、类型或约束不符合实时请求，未发送。",
        )
    };
    let values = content.as_object().ok_or_else(invalid)?;
    let props = schema["properties"].as_object().ok_or_else(invalid)?;
    if values.keys().any(|k| !props.contains_key(k)) {
        return Err(invalid());
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|req| {
            req.iter()
                .any(|k| !values.contains_key(k.as_str().unwrap_or_default()))
        })
    {
        return Err(invalid());
    }
    for (key, value) in values {
        let field = &props[key];
        let valid = match field["type"].as_str() {
            Some("string") => value.as_str().is_some_and(|s| {
                !s.is_empty()
                    && field["minLength"]
                        .as_u64()
                        .is_none_or(|n| s.chars().count() as u64 >= n)
                    && field["maxLength"]
                        .as_u64()
                        .is_none_or(|n| s.chars().count() as u64 <= n)
            }),
            Some("boolean") => value.is_boolean(),
            Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
            Some("number") => value.is_number(),
            _ => false,
        };
        if !valid
            || field["enum"]
                .as_array()
                .is_some_and(|options| !options.contains(value))
        {
            return Err(invalid());
        }
        if let Some(n) = value.as_f64()
            && (field["minimum"].as_f64().is_some_and(|v| n < v)
                || field["maximum"].as_f64().is_some_and(|v| n > v))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn build_ask_question_reply(
    spec: &Value,
    answers: &[AskQuestionAnswerInput],
    cancelled: bool,
) -> Result<InteractionReply, InteractionValidationError> {
    let Some(questions) = spec
        .get("askQuestion")
        .and_then(|v| v.get("questions"))
        .and_then(Value::as_array)
    else {
        return Err(InteractionValidationError::not_sent(
            "ASK_QUESTION_SCHEMA_MISSING",
            "实时问答缺少 questions 列表，未发送任何回复。",
        ));
    };

    if cancelled {
        return Ok(InteractionReply::AskQuestion {
            responses: Vec::new(),
            cancelled: true,
        });
    }

    if answers.is_empty() {
        return Err(InteractionValidationError::not_sent(
            "ASK_QUESTION_EMPTY_ANSWER",
            "未提供任何问答内容，未发送任何回复。",
        ));
    }

    let mut responses = Vec::with_capacity(questions.len());
    for (idx, question) in questions.iter().enumerate() {
        let mut q_clone = question.clone();
        let q_id = question
            .get("id")
            .or_else(|| question.get("questionId"))
            .and_then(Value::as_str);

        let matched = q_id
            .and_then(|id| {
                answers
                    .iter()
                    .find(|a| a.question_id.as_deref() == Some(id))
            })
            .or_else(|| answers.get(idx).filter(|a| a.question_id.is_none()));

        let Some(answer) = matched else {
            return Err(InteractionValidationError::not_sent(
                "ASK_QUESTION_MISSING_ANSWER",
                format!("第 {} 个问题缺少对应回答，未发送任何回复。", idx + 1),
            ));
        };

        if answer.selected_option_ids.is_empty()
            && answer
                .write_in_response
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(InteractionValidationError::not_sent(
                "ASK_QUESTION_EMPTY_ANSWER",
                format!(
                    "第 {} 个问题既未选择选项也未填写文本，未发送任何回复。",
                    idx + 1
                ),
            ));
        }

        if let Some(obj) = q_clone.as_object_mut() {
            obj.insert(
                "selectedOptionIds".to_owned(),
                json!(answer.selected_option_ids),
            );
            if let Some(text) = answer
                .write_in_response
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                obj.insert("writeInResponse".to_owned(), json!(text));
            }
        }
        responses.push(q_clone);
    }

    Ok(InteractionReply::AskQuestion {
        responses,
        cancelled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_step(step: Value) -> AgentStateStore {
        let mut store = AgentStateStore::default();
        store.reset_for_subscription(1, "sub-1".into());
        store
            .apply_update(
                1,
                &json!({
                    "conversationId": "conv-1",
                    "trajectoryId": "traj-1",
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 3,
                            "indices": [2],
                            "steps": [step]
                        }
                    }
                }),
            )
            .unwrap();
        store
    }

    #[test]
    fn validates_live_permission_and_rejects_expired_or_mismatched_steps() {
        let store = state_with_step(json!({
            "type": "CORTEX_STEP_TYPE_RUN_COMMAND",
            "status": "CORTEX_STEP_STATUS_WAITING",
            "requestedInteraction": {
                "permission": { "toolName": "run_command" }
            }
        }));

        let live = InteractionRegistry::extract_live_pending(&store).unwrap();
        assert_eq!(live.kind, "permission");
        assert!(live.supported);
        assert_eq!(live.step_index, 2);

        let reply = InteractionRegistry::validate_and_build_reply(
            &store,
            &InteractionSubmitRequest {
                conversation_id: "conv-1".into(),
                trajectory_id: "traj-1".into(),
                step_index: 2,
                kind: "permission".into(),
                decision: InteractionDecision::Permission {
                    allow: true,
                    scope: PermissionScope::Once,
                },
            },
        )
        .unwrap();
        assert_eq!(
            reply,
            InteractionReply::Permission {
                allow: true,
                scope: PermissionScope::Once
            }
        );

        // Wrong step index rejected without sending.
        let mismatch = InteractionRegistry::validate_and_build_reply(
            &store,
            &InteractionSubmitRequest {
                conversation_id: "conv-1".into(),
                trajectory_id: "traj-1".into(),
                step_index: 99,
                kind: "permission".into(),
                decision: InteractionDecision::Permission {
                    allow: true,
                    scope: PermissionScope::Once,
                },
            },
        )
        .unwrap_err();
        assert_eq!(mismatch.reason_code, "INTERACTION_MISMATCH");

        // Expired step (status moved to DONE) rejected without sending.
        let done_store = state_with_step(json!({
            "type": "CORTEX_STEP_TYPE_RUN_COMMAND",
            "status": "CORTEX_STEP_STATUS_DONE",
            "requestedInteraction": { "permission": {} }
        }));
        let expired = InteractionRegistry::validate_and_build_reply(
            &done_store,
            &InteractionSubmitRequest {
                conversation_id: "conv-1".into(),
                trajectory_id: "traj-1".into(),
                step_index: 2,
                kind: "permission".into(),
                decision: InteractionDecision::Permission {
                    allow: true,
                    scope: PermissionScope::Once,
                },
            },
        )
        .unwrap_err();
        assert_eq!(expired.reason_code, "INTERACTION_EXPIRED");
    }

    #[test]
    fn keeps_unsupported_interactions_waiting_and_never_auto_approves() {
        let store = state_with_step(json!({
            "type": "CORTEX_STEP_TYPE_GENERIC",
            "status": "CORTEX_STEP_STATUS_WAITING",
            "requestedInteraction": {
                "elicitation": { "schema": {} }
            }
        }));
        let live = InteractionRegistry::extract_live_pending(&store).unwrap();
        assert_eq!(live.kind, "elicitation");
        assert!(!live.supported);

        let err = InteractionRegistry::validate_and_build_reply(
            &store,
            &InteractionSubmitRequest {
                conversation_id: "conv-1".into(),
                trajectory_id: "traj-1".into(),
                step_index: 2,
                kind: "elicitation".into(),
                decision: InteractionDecision::Permission {
                    allow: true,
                    scope: PermissionScope::Once,
                },
            },
        )
        .unwrap_err();
        assert_eq!(err.category, TransportErrorCategory::NotSent);
        assert_eq!(err.reason_code, "INTERACTION_UNSUPPORTED");
    }

    #[test]
    fn file_permission_uses_live_path_and_once_scope_and_rejects_stale_id() {
        let store = state_with_step(json!({"status":"CORTEX_STEP_STATUS_WAITING",
            "requestedInteraction":{"filePermission":{"absolutePathUri":"file:///C:/test/a.txt"}}}));
        let mut req = InteractionSubmitRequest {
            conversation_id: "conv-1".into(),
            trajectory_id: "traj-1".into(),
            step_index: 2,
            kind: "filePermission".into(),
            decision: InteractionDecision::FilePermission { allow: false },
        };
        let reply = InteractionRegistry::validate_and_build_reply(&store, &req)
            .unwrap()
            .to_wire_json("conv-1", "traj-1", 2);
        assert_eq!(
            reply["interaction"]["filePermission"],
            json!({"allow":false,"scope":"PERMISSION_SCOPE_ONCE","absolutePathUri":"file:///C:/test/a.txt"})
        );
        req.step_index = 3;
        assert!(InteractionRegistry::validate_and_build_reply(&store, &req).is_err());
    }

    #[test]
    fn elicitation_validates_required_types_ranges_and_unknown_constraints() {
        let schema = json!({"type":"object","properties":{"env":{"type":"string","enum":["dev","prod"]},
            "count":{"type":"integer","minimum":1,"maximum":3},"enabled":{"type":"boolean"}},"required":["env","count","enabled"]});
        let spec = json!({"elicitation":{"mode":"form","requestedSchemaJson":schema.to_string()}});
        let parsed = elicitation_schema(&spec).unwrap();
        assert!(
            validate_elicitation_content(&parsed, &json!({"env":"dev","count":2,"enabled":false}))
                .is_ok()
        );
        for bad in [
            json!({}),
            json!({"env":"invalid","count":2,"enabled":true}),
            json!({"env":"dev","count":4,"enabled":true}),
            json!({"env":"dev","count":2.5,"enabled":true}),
        ] {
            assert!(validate_elicitation_content(&parsed, &bad).is_err());
        }
        let mut constrained = schema.clone();
        constrained["properties"]["env"]["pattern"] = json!(".*");
        assert!(
            elicitation_schema(
                &json!({"elicitation":{"requestedSchemaJson":constrained.to_string()}})
            )
            .is_err()
        );
        let store = state_with_step(
            json!({"status":"CORTEX_STEP_STATUS_WAITING","requestedInteraction":spec}),
        );
        let req = InteractionSubmitRequest {
            conversation_id: "conv-1".into(),
            trajectory_id: "traj-1".into(),
            step_index: 2,
            kind: "elicitation".into(),
            decision: InteractionDecision::Elicitation {
                action: "decline".into(),
                content: json!({"ignored":true}),
            },
        };
        let wire = InteractionRegistry::validate_and_build_reply(&store, &req)
            .unwrap()
            .to_wire_json("conv-1", "traj-1", 2);
        assert_eq!(
            wire["interaction"]["elicitation"],
            json!({"action":"decline","contentJson":"{}"})
        );
    }

    #[test]
    fn maps_generalized_ask_question_options_and_write_in_responses() {
        let store = state_with_step(json!({
            "type": "CORTEX_STEP_TYPE_GENERIC",
            "status": "CORTEX_STEP_STATUS_WAITING",
            "requestedInteraction": {
                "askQuestion": {
                    "questions": [
                        { "id": "q1", "question": "Color?" },
                        { "id": "q2", "question": "Mode?" }
                    ]
                }
            }
        }));

        let reply = InteractionRegistry::validate_and_build_reply(
            &store,
            &InteractionSubmitRequest {
                conversation_id: "conv-1".into(),
                trajectory_id: "traj-1".into(),
                step_index: 2,
                kind: "askQuestion".into(),
                decision: InteractionDecision::AskQuestion {
                    answers: vec![
                        AskQuestionAnswerInput {
                            question_id: Some("q1".into()),
                            selected_option_ids: vec![],
                            write_in_response: Some("green".into()),
                        },
                        AskQuestionAnswerInput {
                            question_id: Some("q2".into()),
                            selected_option_ids: vec!["opt-fast".into()],
                            write_in_response: None,
                        },
                    ],
                    cancelled: false,
                },
            },
        )
        .unwrap();

        let wire = reply.to_wire_json("conv-1", "traj-1", 2);
        let responses = wire["interaction"]["askQuestion"]["responses"]
            .as_array()
            .unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["writeInResponse"], "green");
        assert_eq!(responses[1]["selectedOptionIds"], json!(["opt-fast"]));
    }

    #[test]
    fn missing_answers_are_not_reused_or_matched_to_wrong_ids() {
        let spec = json!({"askQuestion":{"questions":[{"id":"q1"},{"id":"q2"}]}});
        for question_id in [None, Some("q1".into()), Some("q2".into())] {
            let answers = vec![AskQuestionAnswerInput {
                question_id, selected_option_ids: vec![], write_in_response: Some("answer".into()),
            }];
            let err = build_ask_question_reply(&spec, &answers, false).unwrap_err();
            assert_eq!(err.reason_code, "ASK_QUESTION_MISSING_ANSWER");
        }
    }
}
