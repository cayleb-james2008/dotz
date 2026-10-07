from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from typing import Any


MONITOR = Path(__file__).with_name("ci_rust_test_monitor.py")


class CiRustTestMonitorTests(unittest.TestCase):
    def invoke(self, child: list[str], *options: str) -> tuple[subprocess.CompletedProcess[str], list[dict[str, Any]]]:
        temp_root = os.environ.get("CI_MONITOR_TEMP_ROOT")
        with tempfile.TemporaryDirectory(prefix="rust-monitor-test-", dir=temp_root) as temp:
            root = Path(temp)
            log = root / "child.log"
            telemetry = root / "telemetry.jsonl"
            progress = root / "progress.jsonl"
            result = subprocess.run(
                [
                    sys.executable,
                    str(MONITOR),
                    "--log",
                    str(log),
                    "--telemetry",
                    str(telemetry),
                    "--progress",
                    str(progress),
                    "--interval",
                    "0.05",
                    *options,
                    "--",
                    *child,
                ],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
            events = [json.loads(line) for line in progress.read_text().splitlines()] if progress.exists() else []
            return result, events

    def test_compile_silence_is_not_a_test_stall(self) -> None:
        result, events = self.invoke(
            [sys.executable, "-u", "-c", "import time; print('Compiling fixture', flush=True); time.sleep(.25)"],
            "--test-idle-timeout",
            "0.1",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(e["event"] == "test_stall_timeout" for e in events))

    def test_stalled_test_fails_without_skipping_silently(self) -> None:
        result, events = self.invoke(
            [sys.executable, "-u", "-c", "import time; print('running 1 test', flush=True); time.sleep(20)"],
            "--test-idle-timeout",
            "0.2",
            "--termination-grace",
            "0.1",
        )
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertTrue(any(e["event"] == "test_idle_timeout" for e in events))
        self.assertTrue(any(e["event"] == "command_exit" and e.get("return_code") == 124 for e in events))

    def test_finished_cargo_with_inherited_child_pipe_is_bounded_and_failed(self) -> None:
        child = (
            "import subprocess,sys; "
            "print('running 1 test', flush=True); "
            "print('test fixture::ok ... ok', flush=True); "
            "print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s', flush=True); "
            "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(20)'])"
        )
        result, events = self.invoke(
            [sys.executable, "-u", "-c", child],
            "--post-exit-drain-timeout",
            "0.2",
            "--termination-grace",
            "0.1",
        )
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertTrue(any(e["event"] == "post_exit_drain_timeout" for e in events))
        self.assertTrue(any(e["event"] == "test_completed" for e in events))

    @unittest.skipUnless(os.name == "posix" and Path("/usr/bin/setsid").exists(), "requires setsid")
    def test_command_timeout_still_enforces_hard_drain_deadline_for_escaped_pipe(self) -> None:
        child = (
            "import subprocess,sys,time; "
            "subprocess.Popen(['/usr/bin/setsid', sys.executable, '-c', 'import time; time.sleep(3)']); "
            "print('running 1 test', flush=True); time.sleep(20)"
        )
        started = time.monotonic()
        result, events = self.invoke(
            [sys.executable, "-u", "-c", child],
            "--command-timeout",
            "0.3",
            "--post-exit-drain-timeout",
            "0.3",
            "--termination-grace",
            "0.1",
        )
        elapsed = time.monotonic() - started
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertLess(elapsed, 2.0, f"monitor exceeded hard drain deadline: {elapsed:.2f}s")
        self.assertTrue(any(e["event"] == "command_timeout" for e in events))
        self.assertTrue(any(e["event"] == "post_exit_drain_timeout" for e in events))
        self.assertEqual(events[-1]["event"], "command_exit")

    @unittest.skipUnless(hasattr(os, "killpg"), "requires POSIX process groups")
    def test_command_timeout_kills_leader_if_process_group_kill_fails(self) -> None:
        temp_root = os.environ.get("CI_MONITOR_TEMP_ROOT")
        with tempfile.TemporaryDirectory(prefix="rust-monitor-timeout-", dir=temp_root) as temp:
            root = Path(temp)
            log = root / "child.log"
            telemetry = root / "telemetry.jsonl"
            progress = root / "progress.jsonl"
            monitor_args = [
                str(MONITOR), "--log", str(log), "--telemetry", str(telemetry),
                "--progress", str(progress), "--interval", "0.05",
                "--command-timeout", "0.2", "--post-exit-drain-timeout", "0.2",
                "--termination-grace", "0.1", "--", sys.executable, "-u", "-c",
                "import time; print('running 1 test', flush=True); time.sleep(30)",
            ]
            wrapper = (
                "import os,runpy,sys; os.killpg=lambda *_args: None; "
                f"sys.argv={monitor_args!r}; runpy.run_path(sys.argv[0], run_name='__main__')"
            )
            monitor = subprocess.Popen(
                [sys.executable, "-c", wrapper], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            timed_out = False
            try:
                stdout, stderr = monitor.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                timed_out = True
                events = [json.loads(line) for line in progress.read_text().splitlines()]
                command = next(event for event in events if event["event"] == "command_start")
                try:
                    os.kill(int(command["pid"]), 9)
                except ProcessLookupError:
                    pass
                try:
                    monitor.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    monitor.kill()
                    monitor.communicate()
                stdout, stderr = "", "monitor did not finish after its hard timeout"
            events = [json.loads(line) for line in progress.read_text().splitlines()]
            command = next(event for event in events if event["event"] == "command_start")
            command_exit = next(event for event in events if event["event"] == "command_exit")
            try:
                self.assertFalse(timed_out, f"monitor exceeded hard timeout\n{stdout}\n{stderr}")
                self.assertEqual(monitor.returncode, 124, stderr)
                self.assertTrue(any(event["event"] == "command_process_exit_timeout" for event in events))
                self.assertEqual(command_exit.get("command_return_code"), -signal.SIGKILL)
            finally:
                try:
                    os.kill(int(command["pid"]), signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_long_running_test_warning_bounds_the_named_test(self) -> None:
        child = (
            "import time; "
            "print('running 1 test', flush=True); "
            "time.sleep(0.05); "
            "print('test fixture::hung ... has been running for over 60 seconds', flush=True); "
            "time.sleep(20)"
        )
        result, events = self.invoke(
            [sys.executable, "-u", "-c", child],
            "--individual-test-timeout", "0.2",
            "--termination-grace", "0.1",
        )
        self.assertEqual(result.returncode, 124, result.stderr)
        timeouts = [event for event in events if event.get("event") == "individual_test_timeout"]
        self.assertEqual(len(timeouts), 1)
        self.assertEqual(timeouts[0].get("test"), "fixture::hung")

    def test_real_test_failure_status_is_preserved(self) -> None:
        child = (
            "import sys; "
            "print('running 1 test', flush=True); "
            "print('test fixture::bad ... FAILED', flush=True); "
            "print('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s', flush=True); "
            "sys.exit(1)"
        )
        result, events = self.invoke([sys.executable, "-u", "-c", child])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(sum(int(e.get("failed", 0)) for e in events if e["event"] == "command_exit"), 1)


if __name__ == "__main__":
    unittest.main()
