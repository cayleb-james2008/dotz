#!/usr/bin/env python3
"""Stream test output/resources; preserve command status except explicit watchdog timeouts."""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
RUNNING = re.compile(r"^running\s+(\d+)\s+tests?\b")
RESULT = re.compile(r"^test\s+(.+?)\s+\.\.\.\s+(ok|FAILED|ignored|measured)(?:\s|$)")
SUMMARY = re.compile(r"^test result:\s*(.+)$")
SUMMARY_COUNTS = re.compile(r"^(?:ok|FAILED)\.\s*(\d+) passed;\s*(\d+) failed;\s*(\d+) ignored;\s*(\d+) measured")
LONG_RUNNING = re.compile(r"^test\s+(.+?)\s+\.\.\.\s+has been running for over (\d+) seconds$")


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def write_event(stream: Any, event: dict[str, Any]) -> None:
    stream.write(json.dumps(event, sort_keys=True, separators=(",", ":")) + "\n")
    stream.flush()


def proc_snapshot() -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    proc = Path("/proc")
    try:
        entries = list(proc.iterdir())
    except OSError:
        return rows
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            status: dict[str, str] = {}
            for line in (entry / "status").read_text(errors="replace").splitlines():
                key, sep, value = line.partition(":")
                if sep and key in {"Name", "State", "PPid", "VmRSS", "Threads"}:
                    status[key] = value.strip()
            rows.append({
                "pid": int(entry.name),
                "ppid": int(status.get("PPid", "0")),
                "name": status.get("Name", "?"),
                "state": status.get("State", "?"),
                "rss_kib": int(status.get("VmRSS", "0 kB").split()[0]),
                "threads": int(status.get("Threads", "0")),
            })
        except (OSError, ValueError, IndexError):
            continue
    rows.sort(key=lambda r: r["rss_kib"], reverse=True)
    return rows


def system_snapshot() -> dict[str, Any]:
    memory: dict[str, int] = {}
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            key, sep, value = line.partition(":")
            if sep and key in {"MemTotal", "MemAvailable", "SwapTotal", "SwapFree"}:
                memory[key] = int(value.strip().split()[0])
    except (OSError, ValueError, IndexError):
        pass
    try:
        workspace = Path.cwd()
        fs = os.statvfs(workspace)
        disk = {
            "path": str(workspace),
            "total_bytes": fs.f_blocks * fs.f_frsize,
            "available_bytes": fs.f_bavail * fs.f_frsize,
            "free_inodes": fs.f_favail if fs.f_favail else None,
        }
    except OSError:
        disk = {}
    cgroup: dict[str, str] = {}
    for key, path in (("memory_current", "/sys/fs/cgroup/memory.current"),
                      ("memory_max", "/sys/fs/cgroup/memory.max"),
                      ("oom", "/sys/fs/cgroup/memory.events")):
        try:
            cgroup[key] = Path(path).read_text().strip()
        except OSError:
            pass
    try:
        load = Path("/proc/loadavg").read_text().split()
    except OSError:
        load = []
    procs = proc_snapshot()
    return {
        "timestamp": utc_now(),
        "memory_kib": memory,
        "disk": disk,
        "cgroup": cgroup,
        "loadavg": load[:3],
        "process_count": len(procs),
        "process_rss_sum_kib": sum(p["rss_kib"] for p in procs),
        "processes_top_rss": procs[:20],
    }


def emit_test_events(line: str, counters: dict[str, Any], progress: Any) -> list[str]:
    clean = ANSI.sub("", line).strip()
    messages: list[str] = []
    match = RUNNING.match(clean)
    if match:
        count = int(match.group(1))
        counters["binary_index"] += 1
        counters["binary_total"] = count
        counters["binary_completed"] = 0
        counters["suite_total_known"] += count
        counters["binary_active"] = count > 0
        counters["last_test_activity"] = time.monotonic()
        write_event(progress, {
            "event": "binary_test_count", "timestamp": utc_now(),
            "binary_index": counters["binary_index"], "tests": count,
            "suite_total_known": counters["suite_total_known"],
        })
        messages.append(f"[ci-test-progress] target={counters['binary_index']} tests={count} suite_total_known={counters['suite_total_known']}")
    match = RESULT.match(clean)
    if match:
        name, outcome = match.groups()
        outcome = outcome.lower()
        counters["completed"] += 1
        counters["binary_completed"] += 1
        counters["last_test_activity"] = time.monotonic()
        counters["long_running_tests"].pop(name, None)
        counters[outcome] = counters.get(outcome, 0) + 1
        write_event(progress, {
            "event": "test_completed", "timestamp": utc_now(), "test": name,
            "outcome": outcome, "completed": counters["completed"],
            "suite_total_known": counters["suite_total_known"],
            "binary_index": counters["binary_index"],
            "binary_completed": counters["binary_completed"],
            "binary_total": counters["binary_total"],
        })
        messages.append(
            f"[ci-test-progress] completed={counters['completed']}/"
            f"{counters['suite_total_known']} target={counters['binary_index']}"
            f":{counters['binary_completed']}/{counters['binary_total']}"
            f" outcome={outcome} test={name}"
        )
    match = LONG_RUNNING.match(clean)
    if match:
        name, reported_seconds = match.groups()
        if name not in counters["long_running_tests"]:
            noticed_at = time.monotonic()
            counters["long_running_tests"][name] = noticed_at
            write_event(progress, {
                "event": "test_running_too_long", "timestamp": utc_now(),
                "test": name, "reported_seconds": int(reported_seconds),
                "noticed_monotonic": noticed_at,
            })
            messages.append(f"[ci-test-progress] long-running test={name} reported_over={reported_seconds}s")
    match = SUMMARY.match(clean)
    if match:
        summary = match.group(1)
        counts = SUMMARY_COUNTS.match(summary)
        summary_counts = None
        if counts:
            passed, failed, ignored, measured = (int(value) for value in counts.groups())
            summary_counts = {"passed": passed, "failed": failed, "ignored": ignored, "measured": measured}
            for key, value in summary_counts.items():
                counters["summary_" + key] += value
        counters["binary_active"] = False
        counters["long_running_tests"].clear()
        write_event(progress, {"event": "test_binary_summary", "timestamp": utc_now(), "summary": summary, "counts": summary_counts})
    return messages


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", required=True, type=Path)
    parser.add_argument("--telemetry", required=True, type=Path)
    parser.add_argument("--progress", required=True, type=Path)
    parser.add_argument("--interval", type=float, default=10.0)
    parser.add_argument("--test-idle-timeout", type=float, default=0.0,
                        help="fail if a running test binary emits no completed-test progress for this long")
    parser.add_argument("--individual-test-timeout", type=float, default=0.0,
                        help="fail this many seconds after libtest's long-running-test warning")
    parser.add_argument("--post-exit-drain-timeout", type=float, default=0.0,
                        help="fail if the command exits but a descendant keeps its output pipe open")
    parser.add_argument("--command-timeout", type=float, default=0.0,
                        help="fail and terminate the process group after this many seconds")
    parser.add_argument("--termination-grace", type=float, default=5.0,
                        help="seconds between SIGTERM and SIGKILL after a monitor timeout")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("provide a command after --")
    for path in (args.log, args.telemetry, args.progress):
        path.parent.mkdir(parents=True, exist_ok=True)
    counters: dict[str, Any] = {
        "binary_index": 0, "binary_total": 0, "binary_completed": 0,
        "suite_total_known": 0, "completed": 0, "ok": 0,
        "failed": 0, "ignored": 0, "measured": 0,
        "summary_passed": 0, "summary_failed": 0, "summary_ignored": 0, "summary_measured": 0,
        "binary_active": False, "last_test_activity": None, "long_running_tests": {},
    }
    start = time.monotonic()
    command_text = " ".join(command)
    print(f"[ci-telemetry] start={utc_now()} command={command_text}", flush=True)
    with args.log.open("wb", buffering=0) as raw, \
         args.telemetry.open("a", encoding="utf-8", buffering=1) as telemetry, \
         args.progress.open("a", encoding="utf-8", buffering=1) as progress:
        proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, bufsize=0, start_new_session=True)
        write_event(progress, {"event": "command_start", "timestamp": utc_now(), "pid": proc.pid, "command": command})
        selector = selectors.DefaultSelector()
        assert proc.stdout is not None
        selector.register(proc.stdout, selectors.EVENT_READ)
        output_buffer = b""
        next_sample = 0.0
        stream_eof = False
        timeout_reason: str | None = None
        termination_at: float | None = None
        kill_sent = False
        post_exit_since: float | None = None
        forced_abandon = False

        def trigger_timeout(reason: str, details: dict[str, Any], now: float) -> None:
            nonlocal timeout_reason, termination_at
            if timeout_reason is not None:
                return
            timeout_reason = reason
            termination_at = now
            event = {
                "event": reason, "timestamp": utc_now(),
                "elapsed_seconds": round(now - start, 1), **details,
            }
            write_event(progress, event)
            print("[ci-watchdog] " + json.dumps(event, sort_keys=True), flush=True)
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        while True:
            now = time.monotonic()
            wait = max(0.0, min(args.interval, next_sample - now)) if next_sample else 0.0
            events = selector.select(timeout=wait)
            for key, _mask in events:
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    stream_eof = True
                    selector.unregister(key.fileobj)
                    continue
                raw.write(chunk)
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
                output_buffer += chunk
                while b"\n" in output_buffer:
                    line, output_buffer = output_buffer.split(b"\n", 1)
                    text = line.decode(errors="replace")
                    for message in emit_test_events(text, counters, progress):
                        print(message, flush=True)
            now = time.monotonic()
            if now >= next_sample:
                sample = system_snapshot()
                sample.update({
                    "event": "resource_sample", "elapsed_seconds": round(now - start, 1),
                    "cargo_pid": proc.pid, "cargo_return_code": proc.poll(),
                    "tests_completed": counters["completed"],
                    "tests_total_known": counters["suite_total_known"],
                })
                write_event(telemetry, sample)
                disk = sample.get("disk", {}).get("available_bytes", 0) // (1024 * 1024 * 1024)
                avail = sample.get("memory_kib", {}).get("MemAvailable", 0) // 1024
                rss = sample.get("process_rss_sum_kib", 0) // 1024
                print(
                    f"[ci-resource] t={sample['elapsed_seconds']}s procs={sample['process_count']}"
                    f" proc_rss_sum={rss}MiB mem_available={avail}MiB"
                    f" workspace_free={disk}GiB tests={counters['completed']}/"
                    f"{counters['suite_total_known']}", flush=True,
                )
                next_sample = now + max(args.interval, 0.5)
            if timeout_reason is None:
                if args.individual_test_timeout > 0:
                    expired = [(name, noticed) for name, noticed in counters["long_running_tests"].items()
                               if now - noticed >= args.individual_test_timeout]
                    if expired:
                        name, noticed = min(expired, key=lambda item: item[1])
                        trigger_timeout("individual_test_timeout", {
                            "test": name,
                            "seconds_since_long_running_warning": round(now - noticed, 1),
                            "threshold_seconds": args.individual_test_timeout,
                        }, now)
                last_activity = counters["last_test_activity"]
                if (timeout_reason is None and args.test_idle_timeout > 0 and counters["binary_active"]
                        and last_activity is not None and now - last_activity >= args.test_idle_timeout):
                    trigger_timeout("test_idle_timeout", {
                        "tests_completed": counters["completed"],
                        "tests_total_known": counters["suite_total_known"],
                        "idle_seconds": round(now - last_activity, 1),
                        "threshold_seconds": args.test_idle_timeout,
                    }, now)
                if (timeout_reason is None and args.command_timeout > 0
                        and now - start >= args.command_timeout):
                    trigger_timeout("command_timeout", {
                        "threshold_seconds": args.command_timeout,
                        "tests_completed": counters["completed"],
                        "tests_total_known": counters["suite_total_known"],
                    }, now)
            command_return_code = proc.poll()
            if (command_return_code is not None and not stream_eof
                    and args.post_exit_drain_timeout > 0):
                if post_exit_since is None:
                    post_exit_since = now
                elif now - post_exit_since >= args.post_exit_drain_timeout:
                    lingering = system_snapshot().get("processes_top_rss", [])
                    details = {
                        "command_return_code": command_return_code,
                        "drain_seconds": round(now - post_exit_since, 1),
                        "threshold_seconds": args.post_exit_drain_timeout,
                        "tests_completed": counters["completed"],
                        "tests_total_known": counters["suite_total_known"],
                        "lingering_process_names": [row.get("name") for row in lingering[:10]],
                    }
                    if timeout_reason is None:
                        trigger_timeout("post_exit_drain_timeout", details, now)
                    else:
                        write_event(progress, {
                            "event": "post_exit_drain_timeout", "timestamp": utc_now(),
                            "timeout_reason": timeout_reason, **details,
                        })
                        print("[ci-watchdog] " + json.dumps({
                            "event": "post_exit_drain_timeout", "timeout_reason": timeout_reason,
                            **details,
                        }, sort_keys=True), flush=True)
                    if not kill_sent:
                        try:
                            os.killpg(proc.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        kill_sent = True
                        write_event(progress, {
                            "event": "process_group_sigkill", "timestamp": utc_now(),
                            "reason": timeout_reason, "elapsed_seconds": round(now - start, 1),
                        })
                    selector.unregister(proc.stdout)
                    proc.stdout.close()
                    stream_eof = True
            elif (termination_at is not None and not kill_sent
                    and now - termination_at >= max(args.termination_grace, 0.0)):
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                kill_sent = True
                write_event(progress, {"event": "process_group_sigkill", "timestamp": utc_now(),
                                       "reason": timeout_reason, "elapsed_seconds": round(now - start, 1)})
            if (termination_at is not None and command_return_code is None
                    and kill_sent
                    and now - termination_at >= max(args.termination_grace, 0.0)
                    + max(args.post_exit_drain_timeout, 0.0)):
                details = {
                    "event": "command_process_exit_timeout", "timestamp": utc_now(),
                    "timeout_reason": timeout_reason,
                    "pid": proc.pid,
                    "elapsed_seconds": round(now - start, 1),
                    "threshold_seconds": max(args.termination_grace, 0.0)
                    + max(args.post_exit_drain_timeout, 0.0),
                }
                write_event(progress, details)
                print("[ci-watchdog] " + json.dumps(details, sort_keys=True), flush=True)
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                # The process may have moved groups despite the new session; still kill the
                # owned command leader directly before abandoning pipe collection.
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                try:
                    selector.unregister(proc.stdout)
                except (KeyError, ValueError):
                    pass
                proc.stdout.close()
                stream_eof = True
                forced_abandon = True
            return_code = proc.poll()
            if (return_code is not None and stream_eof) or forced_abandon:
                break
        if timeout_reason is not None and not kill_sent:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            kill_sent = True
            write_event(progress, {"event": "process_group_sigkill", "timestamp": utc_now(),
                                   "reason": timeout_reason, "elapsed_seconds": round(time.monotonic() - start, 1)})
        if output_buffer:
            text = output_buffer.decode(errors="replace")
            for message in emit_test_events(text, counters, progress):
                print(message, flush=True)
        command_return_code = proc.poll()
        if forced_abandon:
            try:
                command_return_code = proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                command_return_code = proc.poll()
        else:
            command_return_code = proc.wait()
        if timeout_reason is None:
            assert command_return_code is not None
            return_code = command_return_code
        else:
            return_code = 124
        final_sample = system_snapshot()
        final_sample.update({
            "event": "resource_final", "elapsed_seconds": round(time.monotonic() - start, 1),
            "cargo_pid": proc.pid, "cargo_return_code": command_return_code,
            "tests_completed": counters["completed"],
            "tests_total_known": counters["suite_total_known"],
        })
        write_event(telemetry, final_sample)
        final = {
            "event": "command_exit", "timestamp": utc_now(), "return_code": return_code,
            "command_return_code": command_return_code, "timeout_reason": timeout_reason,
            "duration_seconds": round(time.monotonic() - start, 1),
            "tests_completed": counters["completed"], "tests_total_known": counters["suite_total_known"],
            "passed": counters["summary_passed"], "failed": counters["summary_failed"],
            "ignored": counters["summary_ignored"], "measured": counters["summary_measured"],
            "parsed_result_lines": {"passed": counters["ok"], "failed": counters["failed"],
                                    "ignored": counters["ignored"], "measured": counters["measured"]},
        }
        write_event(progress, final)
        print("[ci-telemetry] result=" + json.dumps(final, sort_keys=True), flush=True)
    return return_code if return_code >= 0 else 128 + (-return_code)


if __name__ == "__main__":
    raise SystemExit(main())
