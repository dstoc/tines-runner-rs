//! Serde models for the subset of the Tines API used by the local runner.

pub mod client;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::effort::EffortCapabilities;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerHarness {
    ClaudeCode,
    Codex,
    Pi,
    Custom,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterRunnerRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<RunnerHarness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_run_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct RunnerTokenResponse {
    pub runner: RunnerIdentity,
    pub runner_token: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RunnerIdentity {
    pub id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerPollRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub owned_runs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency_control: Option<RunnerConcurrencyReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_acks: Option<Vec<RunnerCancellationAck>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declined_assignments: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draining: Option<bool>,
    /// Capability: accept resolved assignment environment metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_delivery: Option<u8>,
    /// Exact-model Codex effort catalog discovered by this daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_capabilities: Option<EffortCapabilities>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerConcurrencyReport {
    pub version: u8,
    pub allow_remote: bool,
    pub ceiling: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<RunnerConcurrencyApplied>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerConcurrencyApplied {
    pub revision: u64,
    pub cap: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCancellationAck {
    pub run_id: String,
    pub token: String,
}

#[derive(Clone, Deserialize, PartialEq)]
pub struct RunnerPollResponse {
    #[serde(default)]
    pub assignments: Vec<RunnerAssignment>,
    #[serde(default)]
    pub cancels: Vec<String>,
    #[serde(default)]
    pub cancel_requests: Vec<RunnerCancellationAck>,
    #[serde(default)]
    pub cancellation_acks: Vec<RunnerCancellationAck>,
    #[serde(default)]
    pub released_assignments: Vec<String>,
    #[serde(default)]
    pub concurrency_control: Option<RunnerConcurrencyInstruction>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RunnerConcurrencyInstruction {
    pub version: u8,
    pub available: bool,
    pub revision: u64,
    pub cap: u32,
    pub ceiling: Option<u32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct RunnerAssignment {
    pub run: RunReference,
    /// Effort accepted for this assignment after runner capability negotiation.
    #[serde(default)]
    pub effort: Option<RunnerAssignmentEffort>,
    pub prompt: String,
    pub bundle: Value,
    /// Optional only in executor requests when the daemon uses environment delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_key: Option<String>,
    /// Resolved environment values for the child process. These values are
    /// never written into the assignment workspace.
    #[serde(default)]
    pub env: Vec<RunnerAssignmentEnv>,
    pub timeout_minutes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerAssignmentEffort {
    pub version: u8,
    pub value: String,
    /// Digest of the capability catalog used when Tines delivered this effort.
    #[serde(default)]
    pub capability_digest: Option<String>,
    /// Present when the exact model was not listed and support was asserted.
    #[serde(default)]
    pub verification: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerAssignmentEnv {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunReference {
    pub id: String,
    pub issue_id: String,
    /// Resolved launch model; null when the harness uses its provider default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub issue_ref: Option<IssueReference>,
    #[serde(default)]
    pub state_at_start_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueReference {
    pub project_name: String,
    pub number: u64,
    pub title: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppendRunLogRequest {
    pub chunk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AppendRunLogResponse {
    pub status: String,
    pub log_bytes_dropped: u64,
    pub log_seq: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FinishStatus {
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishJudgment {
    Interrupted,
    RateLimited,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexRawUsageV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexMeasurementStatus {
    Complete,
    #[default]
    Missing,
    Invalid,
    Nonmonotonic,
    IncompleteAttempt,
    MultipleThreads,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodexPricingEvidenceV1 {
    pub version: u8,
    pub harness: String,
    /// Null means the launch did not specify a model.
    pub model: Option<String>,
    pub identity_source: String,
    pub usage_scope: String,
    pub session_mode: String,
    pub normalization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_usage: Option<CodexRawUsageV1>,
    pub model_rerouted: bool,
    pub measurement_status: CodexMeasurementStatus,
    pub terminal_snapshots: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    /// Optional rollout-derived request context. Keep the provider payload
    /// opaque here so the daemon can carry the existing schema without loss.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_context: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinishRunRequest {
    pub status: FinishStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<RunUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing_evidence: Option<CodexPricingEvidenceV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judgment: Option<FinishJudgment>,
    /// Provider reset instant as epoch milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FinishRunResponse {
    pub id: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct IssueDetailResponse {
    pub id: String,
    #[serde(default)]
    pub workflow: Option<IssueWorkflow>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct IssueWorkflow {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::{
        AppendRunLogRequest, AppendRunLogResponse, FinishRunRequest, FinishRunResponse,
        IssueDetailResponse, RegisterRunnerRequest, RunnerPollRequest, RunnerPollResponse,
        RunnerTokenResponse,
    };
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    fn fixture(name: &str) -> Value {
        let source = match name {
            "registration" => include_str!("../../tests/fixtures/registration.json"),
            "poll" => include_str!("../../tests/fixtures/poll.json"),
            "append-log" => include_str!("../../tests/fixtures/append-log.json"),
            "finish" => include_str!("../../tests/fixtures/finish.json"),
            "issue-detail" => include_str!("../../tests/fixtures/issue-detail.json"),
            _ => panic!("unknown fixture"),
        };
        serde_json::from_str(source).expect("valid protocol fixture")
    }

    fn assert_request_fixture<T>(fixture: &Value)
    where
        T: DeserializeOwned + Serialize,
    {
        let request: T = serde_json::from_value(fixture["request"].clone())
            .expect("deserialize request fixture");
        assert_eq!(
            serde_json::to_value(request).expect("serialize request"),
            fixture["request"]
        );
    }

    #[test]
    fn registration_fixture_round_trips_request_and_reads_response() {
        let fixture = fixture("registration");
        assert_request_fixture::<RegisterRunnerRequest>(&fixture);
        let response: RunnerTokenResponse =
            serde_json::from_value(fixture["response"].clone()).expect("registration response");
        assert_eq!(response.runner.id, "rnr_123");
        assert_eq!(response.runner_token, "fixture-runner-token");
    }

    #[test]
    fn poll_fixture_round_trips_request_and_accepts_additive_response_fields() {
        let fixture = fixture("poll");
        assert_request_fixture::<RunnerPollRequest>(&fixture);
        let response: RunnerPollResponse =
            serde_json::from_value(fixture["response"].clone()).expect("poll response");
        assert_eq!(response.assignments.len(), 1);
        assert_eq!(response.assignments[0].run.id, "arun_456");
        assert_eq!(response.assignments[0].run.issue_id, "iss_789");
        assert_eq!(
            response.assignments[0].run.model.as_deref(),
            Some("gpt-5.1-codex")
        );
        assert_eq!(response.assignments[0].effort.as_ref().unwrap().version, 1);
        assert_eq!(
            response.assignments[0].effort.as_ref().unwrap().value,
            "high"
        );
        assert_eq!(
            response.assignments[0]
                .run
                .issue_ref
                .as_ref()
                .unwrap()
                .number,
            4
        );
        assert_eq!(
            response.assignments[0].run_key.as_deref(),
            Some("fixture-run-key")
        );
        assert_eq!(response.assignments[0].env[0].name, "FIXTURE_TOKEN");
        assert_eq!(response.assignments[0].env[0].value, "fixture-secret");
        assert!(response.assignments[0].env[0].secret);
        assert_eq!(response.assignments[0].timeout_minutes, 120);
        assert!(response.cancels.is_empty());
    }

    #[test]
    fn append_log_fixture_round_trips_request_and_reads_response() {
        let fixture = fixture("append-log");
        assert_request_fixture::<AppendRunLogRequest>(&fixture);
        let response: AppendRunLogResponse =
            serde_json::from_value(fixture["response"].clone()).expect("log response");
        assert_eq!(response.status, "running");
        assert_eq!(response.log_seq, 1);
    }

    #[test]
    fn finish_fixture_round_trips_request_and_reads_response() {
        let fixture = fixture("finish");
        assert_request_fixture::<FinishRunRequest>(&fixture);
        let response: FinishRunResponse =
            serde_json::from_value(fixture["response"].clone()).expect("finish response");
        assert_eq!(response.id, "arun_456");
        assert_eq!(response.status, "failed");
    }

    #[test]
    fn issue_detail_fixture_reads_workflow_and_ignores_unknown_fields() {
        let fixture = fixture("issue-detail");
        let response: IssueDetailResponse =
            serde_json::from_value(fixture["response"].clone()).expect("issue detail response");
        assert_eq!(response.id, "iss_789");
        assert_eq!(response.workflow.unwrap().name, "Implementation");
    }
}
