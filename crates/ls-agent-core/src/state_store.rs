//! `PROD_UI` agent state store, sparse indexed list merging, view separation,
//! and connection-epoch isolation.

use ls_wire::redact_value;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fmt};

/// Distinguishes wire views so full `GetCascadeTrajectory` payloads are never
/// compared as whole JSON objects against `PROD_UI` stream payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrajectoryView {
    ProdUi,
    FullTrajectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPageBounds {
    pub start_index: u64,
    pub end_index_exclusive: u64,
    pub raw_start: Option<i64>,
    pub raw_end: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateMergeError {
    IndexValueCountMismatch {
        indices: usize,
        values: usize,
    },
    TotalLengthOutOfRange(u64),
    InvalidIndexType,
    IndexOutOfBounds {
        index: u64,
        total_length: u64,
    },
    CrossViewTotalMismatch {
        stream_total: u64,
        full_total: usize,
    },
    CrossViewStepMismatch {
        index: u64,
        detail: String,
    },
    PageVerificationFailed(String),
}

impl fmt::Display for StateMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexValueCountMismatch { indices, values } => {
                write!(
                    f,
                    "indexed list count mismatch: {indices} indices vs {values} values"
                )
            }
            Self::TotalLengthOutOfRange(total) => {
                write!(f, "totalLength {total} exceeds protobuf uint32 range")
            }
            Self::InvalidIndexType => write!(f, "indexed list contained a non-u64 index"),
            Self::IndexOutOfBounds {
                index,
                total_length,
            } => write!(f, "index {index} is outside totalLength {total_length}"),
            Self::CrossViewTotalMismatch {
                stream_total,
                full_total,
            } => write!(
                f,
                "PROD_UI totalLength ({stream_total}) differs from GetCascadeTrajectory length ({full_total})"
            ),
            Self::CrossViewStepMismatch { index, detail } => {
                write!(f, "cross-view step mismatch at index {index}: {detail}")
            }
            Self::PageVerificationFailed(msg) => write!(f, "page verification failed: {msg}"),
        }
    }
}

impl std::error::Error for StateMergeError {}

/// Sparse absolute-index projection preserving holes and page boundaries.
///
/// Updates for an existing index always replace the entire protobuf JSON step
/// object rather than deep-merging fields, preventing stale fields (e.g.
/// `GENERIC`/`generic.args` after cancellation converts a step to
/// `RUN_COMMAND`/`CANCELED`).
#[derive(Default, Debug, Clone, PartialEq)]
pub struct IndexedList {
    pub values: BTreeMap<u64, Value>,
    pub total_length: u64,
    pub page_bounds: Option<ResolvedPageBounds>,
}

impl IndexedList {
    pub fn apply_update(
        &mut self,
        update: &Value,
        item_field: &str,
    ) -> Result<(), StateMergeError> {
        let empty = Vec::new();
        let indices = update
            .get("indices")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let items = update
            .get(item_field)
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        if indices.len() != items.len() {
            return Err(StateMergeError::IndexValueCountMismatch {
                indices: indices.len(),
                values: items.len(),
            });
        }

        // `totalLength` is a non-optional protobuf uint32: absent decodes to 0.
        let total = update
            .get("totalLength")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if total > u32::MAX as u64 {
            return Err(StateMergeError::TotalLengthOutOfRange(total));
        }

        let bounds = update.get("pageBounds");
        let raw_start = bounds
            .and_then(|b| b.get("startIndex"))
            .and_then(Value::as_i64);
        let raw_end = bounds
            .and_then(|b| b.get("endIndexExclusive"))
            .and_then(Value::as_i64);

        let resolve_rel = |v: i64| -> u64 {
            if v < 0 {
                (total as i64 + v).max(0) as u64
            } else {
                v as u64
            }
        };

        let start = raw_start.map(resolve_rel).unwrap_or(0).min(total);
        let end = raw_end
            .map(resolve_rel)
            .unwrap_or(total)
            .min(total)
            .max(start);

        // Apply on a clone so any validation failure rolls back atomically.
        let mut next = self.clone();
        next.total_length = total;
        next.page_bounds = bounds.map(|_| ResolvedPageBounds {
            start_index: start,
            end_index_exclusive: end,
            raw_start,
            raw_end,
        });
        next.values.retain(|&idx, _| idx >= start && idx < end);

        for (raw_idx, item) in indices.iter().zip(items) {
            let idx = raw_idx.as_u64().ok_or(StateMergeError::InvalidIndexType)?;
            if idx >= total {
                return Err(StateMergeError::IndexOutOfBounds {
                    index: idx,
                    total_length: total,
                });
            }
            if idx >= start && idx < end {
                // Full object replacement; never deep-merge old fields.
                next.values.insert(idx, item.clone());
            }
        }

        *self = next;
        Ok(())
    }
}

const KNOWN_SCALAR_KEYS: &[&str] = &[
    "conversationId",
    "trajectoryId",
    "status",
    "executableStatus",
    "executorLoopStatus",
    "executionStatus",
    "executorMetadata",
    "contextWindowUsage",
    "activeHook",
];

const KNOWN_STRUCTURAL_KEYS: &[&str] = &[
    "fullyIdle",
    "mainTrajectoryUpdate",
    "backgroundTasksUpdate",
    "trajectoryFileDiffsUpdate",
    "artifactSnapshotsUpdate",
    "pendingAgentMessagesUpdate",
    "subtrajectoryUpdates",
    "turnsUpdate",
];

/// Merged `PROD_UI` state for a single conversation subscription.
#[derive(Debug, Clone)]
pub struct AgentStateStore {
    pub task_progress: crate::presentation::TaskProgress,
    pub view: TrajectoryView,
    pub connection_epoch: u64,
    pub subscriber_id: Option<String>,
    pub steps: IndexedList,
    pub generator_metadatas: IndexedList,
    pub executor_metadatas: IndexedList,
    pub turns: IndexedList,
    pub background_tasks: IndexedList,
    pub file_diffs: IndexedList,
    pub artifact_snapshots: IndexedList,
    pub pending_agent_messages: IndexedList,
    pub subtrajectories: BTreeMap<String, AgentStateStore>,
    pub background_seen: BTreeMap<String, Value>,
    pub scalars: BTreeMap<String, Value>,
    /// Raw JSON presence of `fullyIdle` in the most recent frame.
    /// Absent in JSON means protobuf default `false`, NEVER inheriting a prior `true`.
    pub fully_idle_raw: Option<bool>,
    pub frames: u64,
    /// Diagnostic preservation of unrecognized top-level update fields (redacted).
    pub unknown_fields: BTreeMap<String, Value>,
}

impl Default for AgentStateStore {
    fn default() -> Self {
        Self {
            task_progress: crate::presentation::TaskProgress::default(),
            view: TrajectoryView::ProdUi,
            connection_epoch: 0,
            subscriber_id: None,
            steps: IndexedList::default(),
            generator_metadatas: IndexedList::default(),
            executor_metadatas: IndexedList::default(),
            turns: IndexedList::default(),
            background_tasks: IndexedList::default(),
            file_diffs: IndexedList::default(),
            artifact_snapshots: IndexedList::default(),
            pending_agent_messages: IndexedList::default(),
            subtrajectories: BTreeMap::new(),
            background_seen: BTreeMap::new(),
            scalars: BTreeMap::new(),
            fully_idle_raw: None,
            frames: 0,
            unknown_fields: BTreeMap::new(),
        }
    }
}

impl AgentStateStore {
    /// Clears incremental subscription baselines when establishing a new
    /// `StreamAgentStateUpdates` subscription with a newer `connection_epoch`
    /// and fresh `subscriber_id`.
    pub fn reset_for_subscription(&mut self, connection_epoch: u64, subscriber_id: String) {
        *self = Self {
            view: TrajectoryView::ProdUi,
            connection_epoch,
            subscriber_id: Some(subscriber_id),
            ..Self::default()
        };
    }

    /// Protobuf scalar default semantics: absent `fullyIdle` means `false`.
    pub fn fully_idle(&self) -> bool {
        self.fully_idle_raw.unwrap_or(false)
    }

    pub fn conversation_id(&self) -> Option<&str> {
        self.scalars.get("conversationId").and_then(Value::as_str)
    }

    pub fn trajectory_id(&self) -> Option<&str> {
        self.scalars.get("trajectoryId").and_then(Value::as_str)
    }

    /// Applies a single `AgentStateUpdate` payload for `frame_epoch`.
    ///
    /// Returns `Ok(false)` when `frame_epoch < self.connection_epoch` (late event
    /// from an older subscription is discarded without touching state).
    pub fn apply_update(
        &mut self,
        frame_epoch: u64,
        update: &Value,
    ) -> Result<bool, StateMergeError> {
        if frame_epoch < self.connection_epoch {
            return Ok(false);
        }

        let mut next = self.clone();
        next.connection_epoch = frame_epoch;
        if let Some(id) = update.get("trajectoryId").and_then(Value::as_str)
            && self.trajectory_id().is_some_and(|old| old != id) {
            next.task_progress = crate::presentation::TaskProgress::default();
            next.steps = IndexedList::default();
        }
        next.frames = next.frames.saturating_add(1);
        next.fully_idle_raw = update.get("fullyIdle").and_then(Value::as_bool);

        for &key in KNOWN_SCALAR_KEYS {
            if let Some(v) = update.get(key) {
                next.scalars.insert(key.to_owned(), v.clone());
            }
        }

        if let Some(obj) = update.as_object() {
            for (k, v) in obj {
                if !KNOWN_SCALAR_KEYS.contains(&k.as_str())
                    && !KNOWN_STRUCTURAL_KEYS.contains(&k.as_str())
                {
                    next.unknown_fields
                        .insert(k.clone(), redact_value(v.clone()));
                }
            }
        }

        if let Some(main_traj) = update.get("mainTrajectoryUpdate")
            && let Some(steps_update) = main_traj.get("stepsUpdate")
        {
            next.steps.apply_update(steps_update, "steps")?;
            next.task_progress.observe(&next.steps.values);
        }

        if let Some(bg) = update.get("backgroundTasksUpdate") {
            next.background_tasks.apply_update(bg, "backgroundTasks")?;
        }
        if let Some(main) = update.get("mainTrajectoryUpdate") {
            if let Some(v) = main.get("generatorMetadatasUpdate") {
                next.generator_metadatas
                    .apply_update(v, "generatorMetadatas")?;
            }
            if let Some(v) = main.get("executorMetadatasUpdate") {
                next.executor_metadatas
                    .apply_update(v, "executorMetadatas")?;
            }
        }
        if let Some(v) = update.get("turnsUpdate") {
            let rows = v
                .get("turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let indices: Vec<Value> = rows
                .iter()
                .map(|r| r.get("index").cloned().unwrap_or(json!(0)))
                .collect();
            next.turns.apply_update(
                &json!({"indices":indices,"turns":rows,
                "totalLength":v.get("totalLength").cloned().unwrap_or(json!(0)),
                "pageBounds":v.get("pageBounds").cloned().unwrap_or(Value::Null)}),
                "turns",
            )?;
        }
        if let Some(diffs) = update.get("trajectoryFileDiffsUpdate") {
            next.file_diffs.apply_update(diffs, "trajectoryFileDiffs")?;
        }
        if let Some(artifacts) = update.get("artifactSnapshotsUpdate") {
            next.artifact_snapshots
                .apply_update(artifacts, "artifactSnapshots")?;
        }
        if let Some(pending_msgs) = update.get("pendingAgentMessagesUpdate") {
            next.pending_agent_messages
                .apply_update(pending_msgs, "pendingAgentMessages")?;
        }

        for task in next.background_tasks.values.values() {
            if let Some(id) = task
                .get("taskSnapshot")
                .and_then(|s| s.get("taskId"))
                .and_then(Value::as_str)
            {
                next.background_seen
                    .insert(id.to_owned(), task["taskSnapshot"].clone());
            }
        }

        if let Some(children) = update
            .get("subtrajectoryUpdates")
            .and_then(Value::as_object)
        {
            for (child_id, traj_update) in children {
                let child = next.subtrajectories.entry(child_id.clone()).or_default();
                child.apply_update(frame_epoch, &json!({ "mainTrajectoryUpdate": traj_update }))?;
            }
        }

        *self = next;
        Ok(true)
    }

    /// Extracts real assistant responses from `plannerResponse.response` and
    /// `plannerResponse.modifiedResponse` only (never `userInput` or `thinking`).
    pub fn assistant_texts(&self) -> Vec<String> {
        let mut texts = Vec::new();
        for step in self.steps.values.values() {
            let Some(planner) = step.get("plannerResponse") else {
                continue;
            };
            for key in ["response", "modifiedResponse"] {
                if let Some(text) = planner
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    texts.push(text.to_owned());
                }
            }
        }
        texts
    }

    pub fn has_assistant_output(&self) -> bool {
        self.has_assistant_output_since(0)
    }

    pub fn has_assistant_output_since(&self, min_step_index: u64) -> bool {
        self.steps.values.range(min_step_index..).any(|(_, step)| {
            let Some(planner) = step.get("plannerResponse") else {
                return false;
            };
            ["response", "modifiedResponse"].iter().any(|key| {
                planner
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
            })
        })
    }

    pub fn active_background_tasks(&self) -> Vec<Value> {
        self.background_tasks.values.values().cloned().collect()
    }

    pub fn has_active_background_tasks(&self) -> bool {
        !self.background_tasks.values.is_empty()
    }

    pub fn has_error_step(&self) -> bool {
        self.has_error_step_since(0)
    }

    pub fn has_error_step_since(&self, min_step_index: u64) -> bool {
        self.steps.values.range(min_step_index..).any(|(_, step)| {
            step.get("status").and_then(Value::as_str) == Some("CORTEX_STEP_STATUS_ERROR")
                || step
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .is_some_and(|m| !m.is_empty())
        })
    }

    pub fn has_canceled_step(&self) -> bool {
        self.has_canceled_step_since(0)
    }

    pub fn has_canceled_step_since(&self, min_step_index: u64) -> bool {
        self.steps.values.range(min_step_index..).any(|(_, step)| {
            step.get("status").and_then(Value::as_str) == Some("CORTEX_STEP_STATUS_CANCELED")
        })
    }

    /// Extracts structured subagent conversation IDs from `invokeSubagent.results[].conversationId`.
    /// Never guesses child IDs from natural-language text.
    pub fn structured_subagent_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for step in self.steps.values.values() {
            if let Some(results) = step
                .get("invokeSubagent")
                .and_then(|v| v.get("results"))
                .and_then(Value::as_array)
            {
                for item in results {
                    if let Some(id) = item.get("conversationId").and_then(Value::as_str)
                        && uuid::Uuid::parse_str(id).is_ok()
                        && !ids.iter().any(|existing| existing == id)
                    {
                        ids.push(id.to_owned());
                    }
                }
            }
        }
        ids
    }

    /// Cross-view consistency check against `GetCascadeTrajectory` steps.
    ///
    /// Never compares whole step JSON objects across `PROD_UI` and full
    /// trajectory views (since `PROD_UI` omits `internalMetadata`,
    /// `activeUserState`, `userConfig`, etc.). Only checks total count, `type`,
    /// and `status`.
    pub fn verify_cross_view_consistency(
        &self,
        full_trajectory_steps: &[Value],
    ) -> Result<(), StateMergeError> {
        if self.steps.total_length != full_trajectory_steps.len() as u64 {
            return Err(StateMergeError::CrossViewTotalMismatch {
                stream_total: self.steps.total_length,
                full_total: full_trajectory_steps.len(),
            });
        }
        for (&index, prod_step) in &self.steps.values {
            let Some(full_step) = full_trajectory_steps.get(index as usize) else {
                return Err(StateMergeError::CrossViewStepMismatch {
                    index,
                    detail: "index missing in GetCascadeTrajectory".to_owned(),
                });
            };
            if prod_step.get("type") != full_step.get("type")
                || prod_step.get("status") != full_step.get("status")
            {
                return Err(StateMergeError::CrossViewStepMismatch {
                    index,
                    detail: format!(
                        "type/status mismatch: PROD_UI=({:?},{:?}) vs full=({:?},{:?})",
                        prod_step.get("type"),
                        prod_step.get("status"),
                        full_step.get("type"),
                        full_step.get("status")
                    ),
                });
            }
        }
        Ok(())
    }

    /// Same-view (`PROD_UI`) page equality check against a stable full `PROD_UI`
    /// baseline. Rejects holes, stale indices, and page boundary mismatches.
    pub fn verify_same_view_page(
        &self,
        baseline_prod_ui: &[Value],
        start: u64,
        end: u64,
    ) -> Result<(), StateMergeError> {
        if self.steps.total_length != baseline_prod_ui.len() as u64 {
            return Err(StateMergeError::PageVerificationFailed(format!(
                "totalLength {} != baseline {}",
                self.steps.total_length,
                baseline_prod_ui.len()
            )));
        }
        let Some(bounds) = &self.steps.page_bounds else {
            return Err(StateMergeError::PageVerificationFailed(
                "missing pageBounds".to_owned(),
            ));
        };
        if bounds.start_index != start || bounds.end_index_exclusive != end {
            return Err(StateMergeError::PageVerificationFailed(format!(
                "pageBounds [{}, {}) != expected [{start}, {end})",
                bounds.start_index, bounds.end_index_exclusive
            )));
        }
        let expected_len = end.saturating_sub(start) as usize;
        if self.steps.values.len() != expected_len {
            return Err(StateMergeError::PageVerificationFailed(format!(
                "projected {} steps, expected {expected_len}",
                self.steps.values.len()
            )));
        }
        for index in start..end {
            if self.steps.values.get(&index) != baseline_prod_ui.get(index as usize) {
                return Err(StateMergeError::PageVerificationFailed(format!(
                    "step {index} differs from PROD_UI baseline"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn metadata_pages_turns_and_late_frames_are_atomic() {
        let mut s = super::AgentStateStore::default();
        s.reset_for_subscription(10, "new".into());
        let page = serde_json::json!({"fullyIdle":true,"mainTrajectoryUpdate": {
            "generatorMetadatasUpdate":{"totalLength":350,"pageBounds":{"startIndex":300},"indices":[301],"generatorMetadatas":[{"x":1}]},
            "executorMetadatasUpdate":{"totalLength":350,"pageBounds":{"startIndex":300},"indices":[302],"executorMetadatas":[{"x":2}]}},
            "turnsUpdate":{"totalLength":2,"turns":[{"startStepIndex":0},{"index":1,"startStepIndex":300}]}});
        assert!(s.apply_update(10, &page).unwrap());
        assert_eq!(s.generator_metadatas.values[&301]["x"], 1);
        assert_eq!(s.executor_metadatas.values[&302]["x"], 2);
        assert_eq!(s.turns.values.len(), 2);
        for _ in 0..100 {
            assert!(
                !s.apply_update(9, &serde_json::json!({"fullyIdle":false}))
                    .unwrap()
            );
        }
        assert!(s.fully_idle());
        let before = s.generator_metadatas.clone();
        assert!(
            s.apply_update(
                10,
                &serde_json::json!({"mainTrajectoryUpdate":{
            "generatorMetadatasUpdate":{"totalLength":1,"indices":[9],"generatorMetadatas":[{}]}}})
            )
            .is_err()
        );
        assert_eq!(s.generator_metadatas, before);
        s.apply_update(10, &serde_json::json!({"turnsUpdate":{}}))
            .unwrap();
        assert!(s.turns.values.is_empty());
    }
    use super::*;

    #[test]
    fn replays_verified_live_steps_pages_fixture_across_two_epochs() {
        let records: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/steps-pages.json"
        ))
        .expect("steps-pages fixture must be valid JSON");

        let mut store = AgentStateStore::default();
        let mut current_epoch = 0u64;
        let mut verified_pages = 0usize;

        for record in records {
            let incoming_epoch = record["epoch"].as_u64().unwrap();
            if incoming_epoch != current_epoch {
                assert_eq!(incoming_epoch, current_epoch + 1);
                store.reset_for_subscription(incoming_epoch, format!("sub-epoch-{incoming_epoch}"));
                current_epoch = incoming_epoch;
            }
            if record["kind"] == "update" {
                let applied = store
                    .apply_update(
                        incoming_epoch,
                        &json!({ "mainTrajectoryUpdate": { "stepsUpdate": record["stepsUpdate"] } }),
                    )
                    .unwrap();
                assert!(applied);
            } else {
                let start = record["start"].as_u64().unwrap();
                let end = record["end"].as_u64().unwrap();
                assert_eq!(store.steps.total_length, record["total"].as_u64().unwrap());
                assert_eq!(
                    store.steps.values.keys().copied().collect::<Vec<_>>(),
                    (start..end).collect::<Vec<_>>()
                );
                let bounds = store.steps.page_bounds.as_ref().unwrap();
                assert_eq!(
                    (bounds.start_index, bounds.end_index_exclusive),
                    (start, end)
                );
                verified_pages += 1;
            }
        }
        assert_eq!((current_epoch, verified_pages), (2, 6));
    }

    #[test]
    fn whole_step_replacement_clears_generic_fields_after_cancel_shape_change() {
        let mut store = AgentStateStore::default();
        store.reset_for_subscription(1, "sub-1".into());

        // Running subcommand initially appears as GENERIC with generic.args/result.
        store
            .apply_update(
                1,
                &json!({
                    "conversationId": "conv-1",
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 3,
                            "indices": [2],
                            "steps": [{
                                "type": "CORTEX_STEP_TYPE_GENERIC",
                                "status": "CORTEX_STEP_STATUS_RUNNING",
                                "generic": {
                                    "args": { "command": "Start-Sleep -Seconds 90" },
                                    "result": { "running": true }
                                }
                            }]
                        }
                    }
                }),
            )
            .unwrap();
        assert!(store.steps.values[&2].get("generic").is_some());

        // After cancel, step 2 becomes RUN_COMMAND/CANCELED without `generic`.
        store
            .apply_update(
                1,
                &json!({
                    "conversationId": "conv-1",
                    "fullyIdle": true,
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 3,
                            "indices": [2],
                            "steps": [{
                                "type": "CORTEX_STEP_TYPE_RUN_COMMAND",
                                "status": "CORTEX_STEP_STATUS_CANCELED",
                                "runCommand": {
                                    "commandLine": "Start-Sleep -Seconds 90"
                                }
                            }]
                        }
                    }
                }),
            )
            .unwrap();

        let replaced = &store.steps.values[&2];
        assert_eq!(replaced["type"], "CORTEX_STEP_TYPE_RUN_COMMAND");
        assert_eq!(replaced["status"], "CORTEX_STEP_STATUS_CANCELED");
        assert!(
            replaced.get("generic").is_none(),
            "whole-step replacement must never leave stale generic fields"
        );
        assert_eq!(
            replaced["runCommand"]["commandLine"],
            "Start-Sleep -Seconds 90"
        );
        assert!(store.has_canceled_step());
    }

    #[test]
    fn rejects_stale_connection_epoch_and_enforces_protobuf_default_false() {
        let mut store = AgentStateStore::default();
        store.reset_for_subscription(2, "sub-2".into());

        // Late frame from epoch 1 must be ignored completely.
        let applied = store
            .apply_update(1, &json!({ "fullyIdle": true }))
            .unwrap();
        assert!(!applied);
        assert_eq!(store.frames, 0);
        assert!(!store.fully_idle());

        // Epoch 2 sets fullyIdle = true, then next frame omits fullyIdle -> false!
        assert!(
            store
                .apply_update(2, &json!({ "fullyIdle": true }))
                .unwrap()
        );
        assert_eq!(store.fully_idle_raw, Some(true));
        assert!(store.fully_idle());

        assert!(store.apply_update(2, &json!({})).unwrap());
        assert_eq!(store.fully_idle_raw, None);
        assert!(!store.fully_idle());
    }

    #[test]
    fn cross_view_check_tolerates_full_trajectory_internal_fields_and_catches_mismatch() {
        let mut store = AgentStateStore::default();
        store.reset_for_subscription(1, "sub-1".into());
        store
            .apply_update(
                1,
                &json!({
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 2,
                            "indices": [0, 1],
                            "steps": [
                                { "type": "CORTEX_STEP_TYPE_USER_INPUT", "status": "CORTEX_STEP_STATUS_DONE" },
                                { "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE", "status": "CORTEX_STEP_STATUS_DONE", "plannerResponse": { "response": "hello" } }
                            ]
                        }
                    }
                }),
            )
            .unwrap();

        let full_steps = vec![
            json!({
                "type": "CORTEX_STEP_TYPE_USER_INPUT",
                "status": "CORTEX_STEP_STATUS_DONE",
                "internalMetadata": { "extra": 1 },
                "activeUserState": {},
                "userConfig": {}
            }),
            json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_DONE",
                "plannerResponse": { "response": "hello", "toolCalls": [] }
            }),
        ];
        store.verify_cross_view_consistency(&full_steps).unwrap();

        let mismatched = vec![
            full_steps[0].clone(),
            json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_RUNNING"
            }),
        ];
        assert!(store.verify_cross_view_consistency(&mismatched).is_err());
    }

    #[test]
    fn distinguishes_assistant_output_from_user_input_and_thinking() {
        let mut store = AgentStateStore::default();
        store
            .apply_update(
                0,
                &json!({
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 2,
                            "indices": [0, 1],
                            "steps": [
                                { "userInput": { "text": "hello" } },
                                { "plannerResponse": { "thinking": "internal thought" } }
                            ]
                        }
                    }
                }),
            )
            .unwrap();
        assert!(!store.has_assistant_output());

        store
            .apply_update(
                0,
                &json!({
                    "mainTrajectoryUpdate": {
                        "stepsUpdate": {
                            "totalLength": 2,
                            "indices": [1],
                            "steps": [
                                { "plannerResponse": { "thinking": "internal thought", "response": "real answer" } }
                            ]
                        }
                    }
                }),
            )
            .unwrap();
        assert!(store.has_assistant_output());
        assert_eq!(store.assistant_texts(), vec!["real answer"]);
    }

    #[test]
    fn atomic_rollback_on_invalid_indexed_update_and_negative_bounds_eviction() {
        let mut list = IndexedList::default();
        list.apply_update(
            &json!({
                "indices": [0, 3, 4],
                "steps": [{"a": 0}, {"a": 3}, {"a": 4}],
                "totalLength": 5
            }),
            "steps",
        )
        .unwrap();

        // Invalid index-value mismatch leaves state intact.
        assert!(
            list.apply_update(&json!({ "indices": [1], "totalLength": 1 }), "steps")
                .is_err()
        );
        assert_eq!(list.total_length, 5);
        assert_eq!(list.values.len(), 3);

        // Negative bounds [-2, -1) -> [3, 4) keeps only index 3.
        list.apply_update(
            &json!({
                "pageBounds": { "startIndex": -2, "endIndexExclusive": -1 },
                "totalLength": 5
            }),
            "steps",
        )
        .unwrap();
        assert_eq!(list.values.keys().copied().collect::<Vec<_>>(), vec![3]);
    }
}
