#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ] || {
    [ "${2:-}" = "codex" ] && [ "${3:-}" = "--version" ];
}; then
    printf '%s\n' 'codex-fake 0.1.0'
    exit 0
fi

if [ "${1:-}" = "app-server" ] || {
    [ "${2:-}" = "codex" ] && [ "${3:-}" = "app-server" ];
}; then
    if [ "${1:-}" = "app-server" ]; then
        shift
    else
        shift 3
    fi
    while IFS= read -r message; do
        case "$message" in
            *'"method":"initialize"'*)
                printf '%s\n' '{"id":1,"result":{}}'
                ;;
            *'"method":"model/list"'*)
                printf '%s\n' '{"id":3,"result":{"data":[],"nextCursor":null}}'
                ;;
        esac
    done
    exit 0
fi

wrapper_id=${1:-missing-wrapper-id}
shift || true
if [ -n "${FAKE_CODEX_WRAPPER_FILE:-}" ]; then
    printf '%s\n' "$wrapper_id" > "$FAKE_CODEX_WRAPPER_FILE"
fi
if [ -n "${FAKE_CODEX_ARGS_FILE:-}" ]; then
    printf '%s\n' "$@" > "$FAKE_CODEX_ARGS_FILE"
fi
if [ -n "${FAKE_CODEX_HARNESS_PID_FILE:-}" ]; then
    trap '' TERM
    harness_pid_file_tmp="${FAKE_CODEX_HARNESS_PID_FILE}.tmp"
    printf '%s\n' "$$" > "$harness_pid_file_tmp"
    mv "$harness_pid_file_tmp" "$FAKE_CODEX_HARNESS_PID_FILE"
fi

if [ -n "${FAKE_CODEX_WORKSPACE_PROBE_FILE:-}" ]; then
    printf 'workspace=%s\n' "$PWD" > "$FAKE_CODEX_WORKSPACE_PROBE_FILE"
    printf 'prompt=' >> "$FAKE_CODEX_WORKSPACE_PROBE_FILE"
    cat prompt.md >> "$FAKE_CODEX_WORKSPACE_PROBE_FILE"
    printf 'repo=' >> "$FAKE_CODEX_WORKSPACE_PROBE_FILE"
    cat materialized/acceptance.txt >> "$FAKE_CODEX_WORKSPACE_PROBE_FILE"
fi

if [ -n "${FAKE_CODEX_ENV_PROBE_FILE:-}" ]; then
    printf '%s\n' "${DEPLOY_TOKEN:-}" > "$FAKE_CODEX_ENV_PROBE_FILE"
fi

if [ -n "${FAKE_CODEX_RUN_KEY_PROBE_FILE:-}" ]; then
    curl --fail --silent --show-error \
        --header "Authorization: Bearer $TINES_API_KEY" \
        "$TINES_API_URL/api/v1/issues/iss_arun_happy" \
        > "$FAKE_CODEX_RUN_KEY_PROBE_FILE"
fi

if [ -n "${FAKE_CODEX_BARRIER_DIR:-}" ]; then
    : > "$FAKE_CODEX_BARRIER_DIR/$FAKE_CODEX_BARRIER_NAME"
    while :; do
        count=$(find "$FAKE_CODEX_BARRIER_DIR" -type f | wc -l)
        if [ "$count" -ge "$FAKE_CODEX_BARRIER_COUNT" ]; then
            break
        fi
        sleep 0.01
    done
fi

if [ -n "${FAKE_CODEX_CHILD_PID_FILE:-}" ]; then
    (trap '' TERM; exec sleep 30) &
    child_pid_file_tmp="${FAKE_CODEX_CHILD_PID_FILE}.tmp"
    printf '%s\n' "$!" > "$child_pid_file_tmp"
    mv "$child_pid_file_tmp" "$FAKE_CODEX_CHILD_PID_FILE"
fi

if [ -n "${FAKE_CODEX_SLEEP_SECONDS:-}" ]; then
    sleep "$FAKE_CODEX_SLEEP_SECONDS"
fi

if [ -n "${FAKE_CODEX_JSONL_FILE:-}" ]; then
    cat "$FAKE_CODEX_JSONL_FILE"
else
    printf '%s\n' \
        '{"type":"thread.started","thread_id":"stub-thread"}' \
        '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"cache_write_input_tokens":5,"output_tokens":7}}'
fi

if [ -n "${FAKE_CODEX_CHILD_PID_FILE:-}" ]; then
    while :; do sleep 1; done
fi

exit "${FAKE_CODEX_EXIT_CODE:-0}"
