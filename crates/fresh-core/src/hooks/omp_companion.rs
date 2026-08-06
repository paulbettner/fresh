//! Strict OMP companion snapshot schema for the plugin hook boundary.
//!
//! Only allowlisted ephemeral metadata is represented here; authentication
//! material and raw wire bytes must never cross the boundary.

/// Strict version-1 snapshot delivered by the native OMP terminal companion.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpCompanionSnapshotV1 {
    pub version: u8,
    pub incarnation: String,
    pub sequence: u64,
    pub session_generation: u64,
    pub timestamp_ms: u64,
    pub omp_version: String,
    pub process_id: u64,
    pub session_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_name: Option<String>,
    pub cwd: String,
    pub state: OmpCompanionState,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub status_text: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<OmpCompanionModel>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level: Option<OmpCompanionThinkingLevel>,
    pub running_tools: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub current_tool: Option<OmpCompanionCurrentTool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: Option<OmpCompanionGoal>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub todos: Option<OmpCompanionTodos>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub context: Option<OmpCompanionContext>,
    pub pending_approvals: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub async_jobs: Option<OmpCompanionAsyncJobs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmpCompanionState {
    Idle,
    Working,
    AwaitingApproval,
    Retrying,
    Compacting,
    Stopped,
    Error,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmpCompanionModel {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OmpCompanionThinkingLevel {
    Auto,
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmpCompanionCurrentTool {
    pub name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmpCompanionGoal {
    pub objective: String,
    pub status: OmpCompanionGoalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OmpCompanionGoalStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "budget-limited")]
    BudgetLimited,
    #[serde(rename = "complete")]
    Complete,
    #[serde(rename = "dropped")]
    Dropped,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpCompanionTodos {
    pub pending: u64,
    pub in_progress: u64,
    pub blocked: u64,
    pub completed: u64,
    pub abandoned: u64,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub current: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpCompanionContext {
    pub tokens: u64,
    pub context_window: u64,
    pub percent_bps: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpCompanionAsyncJobs {
    pub running: u64,
    pub recent_failures: u64,
    pub pending_delivery: u64,
}

fn deserialize_optional_non_null<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{hook_args_to_json, HookArgs};

    fn sample_omp_companion_snapshot() -> OmpCompanionSnapshotV1 {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "incarnation": "550e8400-e29b-41d4-a716-446655440000",
            "sequence": 7,
            "sessionGeneration": 3,
            "timestampMs": 1234,
            "ompVersion": "0.52.1",
            "processId": 42,
            "sessionId": "123e4567-e89b-12d3-a456-426614174000",
            "cwd": "/tmp/project",
            "state": "working",
            "statusText": "Finding top-level files",
            "runningTools": 1,
            "pendingApprovals": 0
        }))
        .unwrap()
    }

    #[test]
    fn hook_args_to_json_omp_companion_payload_is_exact_and_allowlisted() {
        let json = hook_args_to_json(&HookArgs::OmpCompanionSnapshot {
            window_id: 9,
            terminal_id: 4,
            received_at_ms: 5678,
            launch_executable: "omp".to_string(),
            snapshot: sample_omp_companion_snapshot(),
        })
        .unwrap();

        let mut outer_keys: Vec<_> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        outer_keys.sort_unstable();
        assert_eq!(
            outer_keys,
            [
                "launch_executable",
                "received_at_ms",
                "snapshot",
                "terminal_id",
                "window_id",
            ]
        );
        assert_eq!(json["window_id"], 9);
        assert_eq!(json["terminal_id"], 4);
        assert_eq!(json["received_at_ms"], 5678);
        assert_eq!(json["launch_executable"], "omp");

        let snapshot = json["snapshot"].as_object().unwrap();
        let mut snapshot_keys: Vec<_> = snapshot.keys().map(String::as_str).collect();
        snapshot_keys.sort_unstable();
        assert_eq!(
            snapshot_keys,
            [
                "cwd",
                "incarnation",
                "ompVersion",
                "pendingApprovals",
                "processId",
                "runningTools",
                "sequence",
                "sessionGeneration",
                "sessionId",
                "state",
                "statusText",
                "timestampMs",
                "version",
            ]
        );
        assert_eq!(snapshot["sessionGeneration"], 3);
        assert_eq!(snapshot["ompVersion"], "0.52.1");
        assert!(snapshot.get("session_generation").is_none());
        assert!(snapshot.get("sessionName").is_none());
    }

    #[test]
    fn omp_companion_optional_fields_reject_explicit_null() {
        for field in ["sessionName", "statusText"] {
            let mut snapshot = serde_json::to_value(sample_omp_companion_snapshot()).unwrap();
            snapshot[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<OmpCompanionSnapshotV1>(snapshot).is_err());
        }
    }
}
