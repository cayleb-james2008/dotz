from __future__ import annotations

import tempfile
import unittest
from pathlib import Path, PureWindowsPath
from unittest import mock

from ui_regression_platform import (
    UNIX_SOCKET_PATH_LIMIT_BYTES,
    BrowserStartupGate,
    BrowserStartupUnavailable,
    agent_browser_argv,
    browser_socket_directory,
    serve_binary_path,
)


class UiRegressionPlatformTests(unittest.TestCase):
    def test_windows_backend_uses_cargo_exe_name(self) -> None:
        self.assertEqual(
            serve_binary_path(Path("/project/target"), "win32"),
            Path("/project/target/debug/serve.exe"),
        )

    def test_posix_backend_uses_unextended_cargo_name(self) -> None:
        self.assertEqual(
            serve_binary_path(Path("/project/target"), "linux"),
            Path("/project/target/debug/serve"),
        )

    def test_windows_cmd_shim_uses_local_node_entrypoint(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entry = root / "node_modules/agent-browser/bin/agent-browser.js"
            entry.parent.mkdir(parents=True)
            entry.write_text("// fixture\n", encoding="utf-8")
            command = agent_browser_argv(
                root,
                node_executable="node.exe",
                executable="/repo/node_modules/.bin/agent-browser.cmd",
                platform_name="win32",
            )
            self.assertEqual(command, ["node.exe", str(entry)])

    def test_windows_native_override_remains_direct(self) -> None:
        executable = "C:/tools/agent-browser.exe"
        with mock.patch(
            "ui_regression_platform.Path", side_effect=PureWindowsPath
        ):
            command = agent_browser_argv(
                Path("/repo"),
                node_executable="node.exe",
                executable=executable,
                platform_name="win32",
            )
        self.assertEqual(command, [executable])

    def test_js_entrypoint_requires_node(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entry = root / "node_modules/agent-browser/bin/agent-browser.js"
            entry.parent.mkdir(parents=True)
            entry.write_text("// fixture\n", encoding="utf-8")
            with mock.patch("ui_regression_platform.shutil.which", return_value=None):
                command = agent_browser_argv(
                    root,
                    node_executable=None,
                    platform_name="win32",
                )
            self.assertEqual(command, [])

    def test_socket_directory_stays_below_unix_limit_with_long_temp_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            long_root = root / ("l" * 100)
            long_root.mkdir()

            short_root = Path("/tmp")
            selected = browser_socket_directory("deadbeef", [long_root, short_root])
            worst_case_socket = selected / ("s" * 64)

            self.assertEqual(selected.parent, short_root)
            self.assertLessEqual(
                len(str(worst_case_socket).encode()),
                UNIX_SOCKET_PATH_LIMIT_BYTES,
            )

    def test_socket_directory_fails_closed_when_every_root_is_too_long(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            long_root = Path(tmp) / ("x" * 100)
            long_root.mkdir()
            with self.assertRaises(OSError):
                browser_socket_directory("deadbeef", [long_root])

    def test_browser_startup_failure_prevents_dependent_actions(self) -> None:
        gate = BrowserStartupGate()
        actions: list[str] = []
        gate.require_ready()  # The open command itself is allowed.
        gate.record_open_result(1)

        with self.assertRaises(BrowserStartupUnavailable):
            gate.require_ready()
            actions.append("snapshot")

        self.assertEqual(actions, [])
        self.assertFalse(gate.ready)


if __name__ == "__main__":
    unittest.main()
