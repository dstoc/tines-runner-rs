#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
    printf '%s\n' 'transport-codex 0.1.0'
    exit 0
fi

if [ "${1:-}" = "app-server" ]; then
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

{
    printf 'workspace=%s\n' "$PWD"
    printf 'executor_marker=%s\n' "${TRANSPORT_EXECUTOR_MARKER:-}"
    printf 'daemon_marker=%s\n' "${TRANSPORT_DAEMON_MARKER:-}"
    printf 'path=%s\n' "$PATH"
    printf 'prompt='
    cat prompt.md
    printf 'skill='
    cat .agents/skills/transport-skill/SKILL.md
    printf 'repo='
    cat materialized/acceptance.txt
} > "$TRANSPORT_HARNESS_CAPTURE"
printf '%s\n' 'stderr-only-transport-harness-diagnostic' >&2

if [ -n "${TRANSPORT_HARNESS_PID_FILE:-}" ]; then
    temporary="${TRANSPORT_HARNESS_PID_FILE}.tmp"
    printf '%s\n' "$$" > "$temporary"
    mv "$temporary" "$TRANSPORT_HARNESS_PID_FILE"
fi

case "${TRANSPORT_HARNESS_MODE:-success}" in
    cancel|timeout)
        trap '' TERM
        (trap '' TERM; exec sleep 30) &
        child=$!
        temporary="${TRANSPORT_HARNESS_CHILD_PID_FILE}.tmp"
        printf '%s\n' "$child" > "$temporary"
        mv "$temporary" "$TRANSPORT_HARNESS_CHILD_PID_FILE"
        printf '%s\n' '{"type":"thread.started","thread_id":"transport-stub-thread"}'
        while :; do sleep 1; done
        ;;
    failure)
        printf '%s\n' '{"type":"thread.started","thread_id":"transport-stub-thread"}'
        printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":20,"cached_input_tokens":5,"cache_write_input_tokens":2,"output_tokens":7}}'
        exit 9
        ;;
    *)
        printf '%s\n' '{"type":"thread.started","thread_id":"transport-stub-thread"}'
        printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":20,"cached_input_tokens":5,"cache_write_input_tokens":2,"output_tokens":7}}'
        ;;
esac
