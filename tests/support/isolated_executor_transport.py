#!/usr/bin/env python3
"""A CI-safe process boundary that models an attached executor transport."""

import json
import os
import selectors
import signal
import subprocess
import sys
import threading

captures, runner_binary, executor_bin, executor_home = sys.argv[1:5]
mode = sys.argv[-1]
child = None
received_signal = None


def save_json(path, value):
    temporary = f"{path}.{os.getpid()}.tmp"
    with open(temporary, "w", encoding="utf-8") as output:
        json.dump(value, output, ensure_ascii=False)
        output.write("\n")
    os.replace(temporary, path)


def forward_signal(signum, _frame):
    global received_signal
    received_signal = signum
    if child is not None and child.poll() is None:
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass


for handled_signal in (signal.SIGINT, signal.SIGTERM):
    signal.signal(handled_signal, forward_signal)

child_environment = {
    "HOME": executor_home,
    "XDG_CONFIG_HOME": os.path.join(executor_home, ".config"),
    "PATH": os.pathsep.join((executor_bin, "/usr/bin", "/bin")),
    "LANG": "C.UTF-8",
    "LC_ALL": "C.UTF-8",
    "RUST_LOG": "error",
    "TRANSPORT_EXECUTOR_MARKER": "executor-only",
}
os.makedirs(executor_home, exist_ok=True)
os.makedirs(child_environment["XDG_CONFIG_HOME"], exist_ok=True)

if mode == "capabilities":
    record = {
        "mode": mode,
        "pid": os.getpid(),
        "process_group": os.getpgrp(),
        "cwd": os.getcwd(),
        "outer_path": os.environ.get("PATH", ""),
        "outer_daemon_marker": os.environ.get("TRANSPORT_DAEMON_MARKER"),
        "outer_has_tines_api_key": "TINES_API_KEY" in os.environ,
        "executor_path": child_environment["PATH"],
        "executor_marker": child_environment["TRANSPORT_EXECUTOR_MARKER"],
    }
    capability_capture = os.path.join(captures, f"capabilities-{os.getpid()}.json")
    save_json(capability_capture, record)
    child = subprocess.Popen(
        [runner_binary, "capabilities"],
        cwd=os.getcwd(),
        env=child_environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
else:
    payload = sys.stdin.buffer.read()
    request = json.loads(payload)
    run_id = request["assignment"]["run"]["id"]
    with open(os.path.join(captures, f"{run_id}.request.json"), "wb") as output:
        output.write(payload)
    record = {
        "mode": mode,
        "run_id": run_id,
        "pid": os.getpid(),
        "process_group": os.getpgrp(),
        "cwd": os.getcwd(),
        "outer_path": os.environ.get("PATH", ""),
        "outer_daemon_marker": os.environ.get("TRANSPORT_DAEMON_MARKER"),
        "outer_has_tines_api_key": "TINES_API_KEY" in os.environ,
        "outer_has_assignment_secret": "DEPLOY_TOKEN" in os.environ,
        "outer_run_key_matches_assignment": os.environ.get("TINES_API_KEY")
        == f"issue-run-key-{run_id}",
        "request_has_run_key": "run_key" in request["assignment"],
        "inner_receives_tines_api_key": "TINES_API_KEY" in child_environment,
        "executor_path": child_environment["PATH"],
        "executor_marker": child_environment["TRANSPORT_EXECUTOR_MARKER"],
    }
    child = subprocess.Popen(
        [runner_binary, "execute"],
        cwd=os.getcwd(),
        env=child_environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    record["executor_pid"] = child.pid
    record["executor_process_group"] = os.getpgid(child.pid)
    save_json(os.path.join(captures, f"{run_id}.transport.json"), record)
    os.write(sys.stderr.fileno(), b"transport-shim-diagnostic-only\n")

    def forward_stdin():
        try:
            child.stdin.write(payload)
            child.stdin.close()
        except BrokenPipeError:
            pass

    threading.Thread(target=forward_stdin, daemon=True).start()

selector = selectors.DefaultSelector()
for stream, destination in ((child.stdout, sys.stdout.fileno()), (child.stderr, sys.stderr.fileno())):
    selector.register(stream, selectors.EVENT_READ, destination)

try:
    while selector.get_map():
        for key, _ in selector.select(timeout=0.1):
            data = os.read(key.fileobj.fileno(), 65536)
            if data:
                os.write(key.data, data)
            else:
                selector.unregister(key.fileobj)
    exit_status = child.wait()
finally:
    selector.close()
    if child.poll() is None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=4)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()

record["executor_exit_status"] = exit_status
record["forwarded_signal"] = received_signal
if mode == "capabilities":
    save_json(capability_capture, record)
else:
    save_json(os.path.join(captures, f"{run_id}.transport.json"), record)
sys.exit(exit_status)
