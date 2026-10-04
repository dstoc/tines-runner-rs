#!/bin/sh
set -eu

if [ "${5:-}" = "capabilities" ]; then
    printf '%s\n' '{"version":1,"harnesses":{"codex":{"version":"codex-fake 0.1.0","effort":{"version":1,"daemon_version":"0.1.0","harness":"codex","harness_version":"codex-fake 0.1.0","catalog_digest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","models":[],"accepts_asserted_effort":true}}}}'
    exit 0
fi

label=$1
events_dir=$2
capture_dir=$3
control_dir=$4
request_file="$control_dir/request.$$.json"
cat > "$request_file"
run_id=$(sed -n 's/.*"run":{"id":"\([^"]*\)".*/\1/p' "$request_file")
if [ -z "$run_id" ]; then
    echo 'stub executor received an invalid request' >&2
    exit 2
fi
mv "$request_file" "$capture_dir/$run_id.request.json"
printf '%s\n' "$label" > "$capture_dir/$run_id.executor"

if [ -f "$control_dir/$run_id.barrier" ]; then
    expected=$(cat "$control_dir/$run_id.barrier")
    barrier_dir="$control_dir/barrier"
    mkdir -p "$barrier_dir"
    : > "$barrier_dir/$run_id"
    while :; do
        count=$(find "$barrier_dir" -type f | wc -l)
        if [ "$count" -ge "$expected" ]; then
            break
        fi
        sleep 0.01
    done
fi

if [ -f "$control_dir/$run_id.child" ]; then
    (trap '' TERM; exec sleep 30) &
    temporary="$control_dir/$run_id.pid.tmp"
    printf '%s\n' "$!" > "$temporary"
    mv "$temporary" "$control_dir/$run_id.pid"
fi

if [ -f "$control_dir/$run_id.sleep" ]; then
    sleep "$(cat "$control_dir/$run_id.sleep")"
fi

if [ -f "$control_dir/$run_id.stderr" ]; then
    cat "$control_dir/$run_id.stderr" >&2
fi

if [ -f "$events_dir/$run_id.jsonl" ]; then
    cat "$events_dir/$run_id.jsonl"
else
    printf '%s\n' \
        '{"version":1,"type":"log","stream":"stdout","message":"executor stub completed"}' \
        '{"version":1,"type":"result","status":"completed","exit_code":0,"interrupted":false}'
fi

if [ -f "$control_dir/$run_id.child" ]; then
    while :; do sleep 1; done
fi
