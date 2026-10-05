from __future__ import annotations

import shutil
import sys
from pathlib import Path


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
    return [str(selected)]


def agent_browser_target(argv_prefix: list[str], fallback: Path) -> Path:
    """Return the actual script/binary checked by the regression's prerequisite."""
    return Path(argv_prefix[-1]) if argv_prefix else Path(fallback)
