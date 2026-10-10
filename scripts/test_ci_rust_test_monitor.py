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

    @unittest.skipUnless(sys.platform == "linux", "requires Linux waitid/procfs")
    def test_post_exit_group_signal_retains_unreaped_owner(self) -> None:
        with tempfile.TemporaryDirectory(dir=os.environ.get("CI_MONITOR_TEMP_ROOT")) as temp:
            root = Path(temp)
            wrapper = root / "guarded_monitor.py"
            wrapper.write_text(
                "import os,pathlib,runpy,sys\n"
                "real = os.killpg\n"
                "def guarded(pid, sig):\n"
                " if not pathlib.Path(f'/proc/{pid}/stat').exists():\n"
                "  raise RuntimeError('leader reaped before group signal')\n"
                " return real(pid, sig)\n"
                "os.killpg = guarded\n"
                f"sys.argv[0] = {str(MONITOR)!r}\n"
                f"runpy.run_path({str(MONITOR)!r}, run_name='__main__')\n"
            )
            child = (
                "import subprocess,sys; "
                "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(2)']); "
                "print('fixture finished', flush=True)"
            )
            result = subprocess.run([
                sys.executable, str(wrapper),
                "--log", str(root / "child.log"),
                "--telemetry", str(root / "telemetry.jsonl"),
                "--progress", str(root / "progress.jsonl"),
                "--interval", "0.05", "--post-exit-drain-timeout", "0.2",
                "--termination-grace", "0.1", "--", sys.executable, "-u", "-c", child,
            ], capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 124, result.stdout + result.stderr)
            self.assertNotIn("leader reaped before group signal", result.stderr)

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
            sentinel = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
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
            command_pid: int | None = None
            try:
                try:
                    stdout, stderr = monitor.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    stdout, stderr = "", "monitor did not finish after its hard timeout"
                events = [json.loads(line) for line in progress.read_text().splitlines()]
                command = next(event for event in events if event["event"] == "command_start")
                command_pid = int(command["pid"])
                exits = [event for event in events if event["event"] == "command_exit"]
                self.assertIsNotNone(monitor.returncode, f"monitor exceeded hard timeout\n{stdout}\n{stderr}")
                self.assertEqual(monitor.returncode, 124, stderr)
                self.assertTrue(any(event["event"] == "command_process_exit_timeout" for event in events))
                self.assertEqual(len(exits), 1, events)
                self.assertEqual(exits[0].get("command_return_code"), -signal.SIGKILL)
                self.assertIn("running 1 test", log.read_text())
                self.assertIsNone(sentinel.poll(), "timeout cleanup must not kill an unrelated process")
            finally:
                if command_pid is None and progress.exists():
                    try:
                        command_pid = int(next(
                            event["pid"] for event in (json.loads(line) for line in progress.read_text().splitlines())
                            if event.get("event") == "command_start"
                        ))
                    except (StopIteration, OSError, ValueError, json.JSONDecodeError):
                        command_pid = None
                try:
                    events = [json.loads(line) for line in progress.read_text().splitlines()] if progress.exists() else []
                except (OSError, json.JSONDecodeError):
                    events = []
                leader_reaped = any(
                    event.get("event") == "command_exit" and event.get("command_return_code") is not None
                    for event in events
                )
                if monitor.poll() is None:
                    try:
                        monitor.communicate(timeout=3)
                    except subprocess.TimeoutExpired:
                        if command_pid is not None and not leader_reaped:
                            try:
                                os.kill(command_pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        try:
                            monitor.communicate(timeout=3)
                        except subprocess.TimeoutExpired:
                            monitor.kill()
                            monitor.communicate()
                elif command_pid is not None and not leader_reaped:
                    try:
                        os.kill(command_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                sentinel.terminate()
                sentinel.wait(timeout=3)

    @unittest.skipUnless(os.name == "posix" and hasattr(os, "killpg"), "requires POSIX process groups")
    def test_forced_abandon_waits_for_owned_leader_after_kill_failures(self) -> None:
        temp_root = os.environ.get("CI_MONITOR_TEMP_ROOT")
        with tempfile.TemporaryDirectory(prefix="rust-monitor-owned-leader-", dir=temp_root) as temp:
            root = Path(temp)
            log = root / "child.log"
            telemetry = root / "telemetry.jsonl"
            progress = root / "progress.jsonl"
            release = root / "release-child"
            injections = root / "injections.log"
            child = (
                "import pathlib,time; print('running 1 test', flush=True); "
                f"release=pathlib.Path({str(release)!r}); "
                "exec('while not release.exists():\\n time.sleep(.01)')"
            )
            monitor_args = [
                str(MONITOR), "--log", str(log), "--telemetry", str(telemetry),
                "--progress", str(progress), "--interval", "0.05",
                "--command-timeout", "0.15", "--post-exit-drain-timeout", "0.15",
                "--termination-grace", "0.1", "--", sys.executable, "-u", "-c", child,
            ]
            wrapper = (
                "import os,runpy,subprocess,sys\n"
                f"injections = {str(injections)!r}\n"
                "def fail_group(*_args):\n"
                " with open(injections, 'a', encoding='utf-8') as out: out.write('group\\n')\n"
                " raise ProcessLookupError('injected process-group kill failure')\n"
                "def fail_direct(self):\n"
                " with open(injections, 'a', encoding='utf-8') as out: out.write('direct\\n')\n"
                " return None\n"
                "os.killpg = fail_group\n"
                "subprocess.Popen.kill = fail_direct\n"
                f"sys.argv = {monitor_args!r}\n"
                "runpy.run_path(sys.argv[0], run_name='__main__')\n"
            )
            sentinel = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
            monitor = subprocess.Popen(
                [sys.executable, "-c", wrapper], stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
            )
            command_pid: int | None = None

            def read_events() -> list[dict[str, Any]]:
                if not progress.exists():
                    return []
                try:
                    return [json.loads(line) for line in progress.read_text().splitlines()]
                except (OSError, json.JSONDecodeError):
                    return []

            try:
                deadline = time.monotonic() + 5
                events: list[dict[str, Any]] = []
                while time.monotonic() < deadline:
                    events = read_events()
                    start_event = next((event for event in events if event.get("event") == "command_start"), None)
                    if start_event:
                        command_pid = int(start_event["pid"])
                    if (any(event.get("event") == "command_process_exit_timeout" for event in events)
                            and any(event.get("event") == "command_leader_reap_pending" for event in events)):
                        break
                    if monitor.poll() is not None:
                        break
                    time.sleep(0.02)
                self.assertIsNotNone(command_pid, f"missing command_start event: {events}")
                assert command_pid is not None
                self.assertTrue(
                    any(event.get("event") == "command_process_exit_timeout" for event in events),
                    f"monitor did not reach forced abandonment: {events}",
                )
                self.assertTrue(injections.exists(), "kill failures were not injected")
                injected = injections.read_text().splitlines()
                self.assertIn("group", injected)
                self.assertIn("direct", injected)

                # The old one-shot wait path exited after its wait timeout even though
                # this deliberately gated leader remained live. Hold the gate longer
                # than that wait to exercise the race deterministically.
                time.sleep(1.15)
                events = read_events()
                self.assertFalse(any(event.get("event") == "command_exit" for event in events), events)
                self.assertIsNone(monitor.poll(), "monitor relinquished its live owned leader")
                try:
                    os.kill(command_pid, 0)
                except ProcessLookupError:
                    self.fail("owned leader exited before the test released its gate")
                self.assertTrue(any(event.get("event") == "command_leader_reap_pending" for event in events))
                self.assertIsNone(sentinel.poll(), "cleanup must not widen to an unrelated process")

                release.touch()
                stdout, stderr = monitor.communicate(timeout=5)
                events = read_events()
                exits = [event for event in events if event.get("event") == "command_exit"]
                self.assertEqual(monitor.returncode, 124, f"{stdout}\\n{stderr}")
                self.assertIn("command_process_exit_timeout", stdout)
                self.assertIn("command_leader_reap_pending", stdout)
                self.assertEqual(len(exits), 1, events)
                self.assertEqual(exits[0].get("command_return_code"), 0, events)
                self.assertEqual(events[-1].get("event"), "command_exit")
                self.assertIn("running 1 test", log.read_text())
                self.assertIsNone(sentinel.poll(), "monitor cleanup killed an unrelated sentinel")
            finally:
                release.touch(exist_ok=True)
                if command_pid is None:
                    command_pid = next((
                        int(event["pid"]) for event in read_events()
                        if event.get("event") == "command_start"
                    ), None)
                if monitor.poll() is None:
                    try:
                        monitor.communicate(timeout=3)
                    except subprocess.TimeoutExpired:
                        if command_pid is not None:
                            try:
                                os.kill(command_pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        try:
                            monitor.communicate(timeout=3)
                        except subprocess.TimeoutExpired:
                            monitor.kill()
                            monitor.communicate()
                elif command_pid is not None and not any(
                    event.get("event") == "command_exit" and event.get("command_return_code") is not None
                    for event in read_events()
                ):
                    try:
                        os.kill(command_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                sentinel.terminate()
                sentinel.wait(timeout=3)

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
