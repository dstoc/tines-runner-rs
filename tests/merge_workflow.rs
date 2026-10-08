const MERGE_WORKFLOW: &str = include_str!("../.github/workflows/merge.yml");

#[test]
fn merge_workflow_uses_v2_and_the_expected_triggers() {
    for expected in [
        "pull_request_review:",
        "types: [submitted]",
        "types: [auto_merge_enabled, synchronize]",
        "branches: [main]",
        "workflow_dispatch:",
        "github.event.review.state == 'approved'",
        "uses: dstoc/merge-action/.github/workflows/merge.yml@v2",
        "base-branch: main",
    ] {
        assert!(MERGE_WORKFLOW.contains(expected), "missing {expected:?}");
    }
}

#[test]
fn merge_workflow_serializes_runs_and_limits_the_token_boundary() {
    for expected in [
        "group: merge-worker",
        "cancel-in-progress: false",
        "queue: max",
        "permissions:\n  contents: read",
        "PR_GH_TOKEN: ${{ secrets.PR_GH_TOKEN }}",
    ] {
        assert!(MERGE_WORKFLOW.contains(expected), "missing {expected:?}");
    }

    assert_eq!(MERGE_WORKFLOW.matches("secrets.PR_GH_TOKEN").count(), 1);
    assert!(!MERGE_WORKFLOW.contains("contents: write"));
    assert!(!MERGE_WORKFLOW.contains("actions/checkout"));
    assert!(!MERGE_WORKFLOW.contains("steps:"));
    assert!(!MERGE_WORKFLOW.contains("run:"));
}
