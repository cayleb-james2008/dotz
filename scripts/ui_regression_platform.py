from __future__ import annotations

import shutil
import os
import re
import sys
from pathlib import Path

UNIX_SOCKET_PATH_LIMIT_BYTES = 103
_SOCKET_FILENAME_RESERVE_BYTES = 64


class BrowserStartupUnavailable(RuntimeError):
    """Raised when browser checks depend on a failed agent-browser startup."""


class BrowserStartupGate:
    """Prevent browser-dependent checks from cascading after `open` fails."""

    def __init__(self) -> None:
        self.failure: str | None = None

    @property
    def ready(self) -> bool:
        return self.failure is None

    def record_open_result(self, returncode: int) -> None:
        if returncode != 0:
            self.failure = f"agent-browser open exited with status {returncode}"

    def require_ready(self) -> None:
        if self.failure is not None:
            raise BrowserStartupUnavailable(self.failure)


def browser_socket_directory(run_token: str, roots: list[Path]) -> Path:
    """Select an isolated short POSIX socket directory, preferring the caller's temp root.

    Reserve room for a socket filename instead of only checking the directory itself; the
    agent-browser/Chromium Unix-domain socket path must fit within the conservative 103-byte
    limit even when the caller's temporary root is unusually long.
    """
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,12}", run_token):
        raise ValueError("run_token must be 1-12 safe filename characters")
    candidates = dict.fromkeys(Path(root) for root in roots)
    for root in candidates:
        directory = root / f"db-{run_token}"
        worst_case_socket = directory / ("s" * _SOCKET_FILENAME_RESERVE_BYTES)
        if len(os.fsencode(worst_case_socket)) <= UNIX_SOCKET_PATH_LIMIT_BYTES:
            return directory
    raise OSError("no candidate temporary root fits the Unix-domain socket path limit")


def serve_binary_path(target_dir: Path, platform_name: str | None = None) -> Path:
    """Return Cargo's debug server executable name for the requested OS."""
    platform_name = platform_name or sys.platform
    filename = "serve.exe" if platform_name == "win32" else "serve"
    return Path(target_dir) / "debug" / filename


def agent_browser_argv(
    project_root: Path,
    *,
    node_executable: str | None = None,
    executable: str | None = None,
    platform_name: str | None = None,
) -> list[str]:
    """Build an argv prefix without trying to execute npm's Windows .cmd shim.

    The package's JavaScript entrypoint performs platform-specific native-binary
    selection. Launch it with Node on every platform; retain direct native-binary
    overrides, and replace Windows .cmd/.bat shims with the local JS entrypoint.
    """
    platform_name = platform_name or sys.platform
    node_executable = node_executable or shutil.which("node")
    package_entry = Path(project_root) / "node_modules" / "agent-browser" / "bin" / "agent-browser.js"

    selected = Path(executable) if executable else package_entry
    if not executable and not selected.is_file():
        located = shutil.which("agent-browser")
        if not located:
            return []
        selected = Path(located)

    suffix = selected.suffix.casefold()
    if suffix == ".js":
        return [node_executable, str(selected)] if node_executable else []
    if platform_name == "win32" and suffix in {".cmd", ".bat"}:
        if node_executable and package_entry.is_file():
            return [node_executable, str(package_entry)]
        return []
    # Return a caller-supplied native override verbatim. Windows Path normalizes
    # separators, but direct executable overrides are argv tokens, not paths to
    # rewrite.
    return [executable] if executable else [str(selected)]


def agent_browser_target(argv_prefix: list[str], fallback: Path) -> Path:
    """Return the actual script/binary checked by the regression's prerequisite."""
    return Path(argv_prefix[-1]) if argv_prefix else Path(fallback)
