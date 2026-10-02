//! Serde models for the subset of the Tines API used by the local runner.

pub mod client;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunnerHarness {
    ClaudeCode,
    Codex,
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
    pub draining: Option<bool>,
}

#[derive(Clone, Deserialize, PartialEq)]
pub struct RunnerPollResponse {
    #[serde(default)]
    pub assignments: Vec<RunnerAssignment>,
    #[serde(default)]
    pub cancels: Vec<String>,
}

#[derive(Clone, Deserialize, PartialEq)]
pub struct RunnerAssignment {
    pub run: RunReference,
    pub prompt: String,
    pub bundle: Value,
    pub run_key: String,
    pub timeout_minutes: u64,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct RunReference {
    pub id: String,
    pub issue_id: String,
    #[serde(default)]
    pub issue_ref: Option<IssueReference>,
    #[serde(default)]
    pub state_at_start_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FinishStatus {
    Completed,
    Failed,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinishRunRequest {
    pub status: FinishStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            response.assignments[0]
                .run
                .issue_ref
                .as_ref()
                .unwrap()
                .number,
            4
        );
        assert_eq!(response.assignments[0].run_key, "fixture-run-key");
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
