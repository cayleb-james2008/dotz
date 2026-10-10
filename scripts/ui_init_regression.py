#!/usr/bin/env python3
"""Browser regression for Dotz ESM boot, first-run onboarding, settings, and panel wiring.

Prerequisites: run `npm run install:deps`, have a built `dotz-core` serve binary (built automatically when absent),
and have a Chromium-compatible browser. Run the test with `npm run test:ui`.
"""
from __future__ import annotations

import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

from ui_regression_platform import (
    BrowserStartupGate,
    BrowserStartupUnavailable,
    agent_browser_argv,
    agent_browser_target,
    browser_socket_directory,
    serve_binary_path,
)

ROOT = pathlib.Path(__file__).resolve().parents[1]
if os.environ.get("DOTZ_UI_TEST_OUT"):
    OUT = pathlib.Path(os.environ["DOTZ_UI_TEST_OUT"])
elif pathlib.Path("/work/evidence").is_dir():
    OUT = pathlib.Path("/work/evidence/evidence/ui-init-candidate-backend")
else:
    OUT = ROOT / "target/ui-init-regression"
OUT.mkdir(parents=True, exist_ok=True)
AGENT_OVERRIDE = os.environ.get("DOTZ_AGENT_BROWSER_BIN")
AGENT_SCRIPT = ROOT / "node_modules/agent-browser/bin/agent-browser.js"
AGENT_CANDIDATE = AGENT_OVERRIDE or str(AGENT_SCRIPT)
AGENT_COMMAND = agent_browser_argv(
    ROOT,
    node_executable=shutil.which("node"),
    executable=AGENT_CANDIDATE,
    platform_name=sys.platform,
)
AGENT = agent_browser_target(AGENT_COMMAND, pathlib.Path(AGENT_CANDIDATE))
TARGET_DIR = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
SERVE = pathlib.Path(os.environ["DOTZ_SERVE_BIN"]) if os.environ.get("DOTZ_SERVE_BIN") else serve_binary_path(TARGET_DIR, sys.platform)
CHROMIUM = os.environ.get("CHROMIUM_PATH") or shutil.which("chromium") or shutil.which("chromium-browser")
SESSION = "d" + uuid.uuid4().hex[:8]
PANEL_DEFINITIONS_EXPECTED = [
    {"name": "chat", "icon": "▓", "label": "CHAT"},
    {"name": "graph", "icon": "◐", "label": "WORKFLOW GRAPH", "color": "var(--cyan)", "toolMap": [{"exact": "subagent"}]},
    {"name": "brain", "icon": "◆", "label": "AGENT BRAIN", "color": "var(--mauve)", "toolMap": [{"exact": ["rsi_baseline", "rsi_compare"]}]},
    {"name": "browser", "icon": "▣", "label": "BROWSER", "color": "var(--cyan)", "toolMap": [{"prefix": "browser_"}]},
    {"name": "memory", "icon": "▤", "label": "MEMORY", "color": "var(--mauve)", "toolMap": [{"prefix": "memory_"}]},
    {"name": "files", "icon": "▥", "label": "FILES", "color": "var(--muted)", "toolMap": [{"exact": ["edit", "write"]}]},
    {"name": "sandbox", "icon": "▩", "label": "SANDBOX", "color": "var(--peach)", "toolMap": [{"prefix": "sandbox_"}]},
    {"name": "skills", "icon": "✦", "label": "SKILLS", "color": "var(--yellow)", "toolMap": [{"exact": ["skill", "create_skill", "list_skills", "create_agent", "list_agents"]}]},
    {"name": "templates", "icon": "⬡", "label": "TEMPLATES"},
    {"name": "design", "icon": "❖", "label": "DESIGN", "color": "var(--pink)", "toolMap": [{"prefix": "design_"}]},
    {"name": "spec", "icon": "◇", "label": "SPEC", "color": "var(--peach)", "toolMap": [{"prefix": "openspec_"}]},
    {"name": "living-docs", "icon": "◧", "label": "LIVING DOCS", "color": "var(--pink)", "toolMap": [{"prefix": "living_docs_"}]},
    {"name": "vcs", "icon": "⌁", "label": "VCS", "color": "var(--green)", "toolMap": [{"prefix": "vcs_"}]},
    {"name": "connections", "icon": "⊕", "label": "CONNECTIONS"},
    {"name": "doctrine", "icon": "◈", "label": "DOCTRINE", "color": "var(--lav)", "toolMap": [{"exact": "agents_md"}]},
    {"name": "marketplace", "icon": "⚑", "label": "MARKETPLACE"},
    {"name": "perf", "icon": "⚡", "label": "PERFORMANCE"},
]
PANEL_EXPECTATIONS = [(entry["name"], entry["label"]) for entry in PANEL_DEFINITIONS_EXPECTED]
PROVIDER_ENV_NAMES = {
    "ANTHROPIC_API_KEY",
    "COHERE_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "NVIDIA_API_KEY",
    "OLLAMA_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
    "DOTZ_TOKEN",
}

steps: list[dict] = []
checks: list[dict] = []
failures: list[str] = []
server: subprocess.Popen | None = None
base_url = ""
browser_startup = BrowserStartupGate()


def check(name: str, ok: bool, detail=None) -> None:
    item = {"name": name, "passed": bool(ok)}
    if detail is not None:
        item["detail"] = detail
    checks.append(item)
    if not ok:
        failures.append(name)


def run(name: str, *args: str, timeout: int = 30) -> dict:
    browser_startup.require_ready()
    argv = [*AGENT_COMMAND, "--session", SESSION, *args]
    try:
        proc = subprocess.run(
            argv,
            cwd=ROOT,
            env=browser_env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        item = {"name": name, "argv": argv, "returncode": proc.returncode,
                "stdout": proc.stdout, "stderr": proc.stderr}
    except Exception as exc:
        item = {"name": name, "argv": argv, "returncode": 127,
                "stdout": "", "stderr": repr(exc)}
    steps.append(item)
    (OUT / "steps.json").write_text(json.dumps(steps, indent=2) + "\n")
    if item["returncode"] != 0:
        failures.append(f"browser command failed: {name}")
    return item


def parse_eval(item: dict):
    raw = item.get("stdout", "").strip()
    try:
        parsed = json.loads(raw)
    except (json.JSONDecodeError, TypeError):
        return raw
    if isinstance(parsed, str):
        try:
            return json.loads(parsed)
        except json.JSONDecodeError:
            return parsed
    return parsed


def evaluate(name: str, expression: str):
    return parse_eval(run(name, "eval", expression))


def wait_ready(url: str, timeout_s: float = 30) -> tuple[bool, object]:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if server is not None and server.poll() is not None:
            return False, {"server_exit_code": server.returncode}
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                body = json.loads(response.read())
                if response.status == 200 and body.get("ok") is True:
                    return True, body
        except Exception:
            time.sleep(0.2)
    return False, "health endpoint did not become ready"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def ensure_serve_binary() -> dict:
    if SERVE.is_file():
        return {"path": str(SERVE), "built_during_test": False}
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError(f"serve binary missing at {SERVE} and cargo is unavailable")
    proc = subprocess.run(
        [cargo, "build", "--locked", "-p", "dotz-core", "--bin", "serve"],
        cwd=ROOT,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=1200,
        check=False,
    )
    (OUT / "cargo-build-serve.stdout.log").write_text(proc.stdout)
    (OUT / "cargo-build-serve.stderr.log").write_text(proc.stderr)
    if proc.returncode != 0 or not SERVE.is_file():
        raise RuntimeError(f"cargo build for serve failed ({proc.returncode}); see build logs")
    return {"path": str(SERVE), "built_during_test": True,
            "build_exit_code": proc.returncode}


# Isolate backend-owned files even when the caller has provider credentials or ~/.pi state.
app_home = pathlib.Path(tempfile.mkdtemp(prefix="dotz-ui-home-"))
config_dir = app_home / "dotz-config"
config_dir.mkdir()
workspace = app_home / "workspace"
workspace.mkdir()

safe_environment_names = {"PATH", "SSL_CERT_FILE", "LANG", "LC_ALL", "TZ"}
base_environment = {key: value for key, value in os.environ.items() if key in safe_environment_names}
removed_provider_vars = sorted(
    key for key in os.environ
    if key in PROVIDER_ENV_NAMES or key.endswith("_API_KEY") or key.startswith("DOTZ_")
)
port = free_port()
base_url = f"http://127.0.0.1:{port}"
private_tmp = app_home / "tmp"
private_tmp.mkdir()
private_cache = app_home / "cache"
private_cache.mkdir()
socket_dir: pathlib.Path | None = None
if os.name == "posix":
    socket_roots = [pathlib.Path(tempfile.gettempdir()), pathlib.Path("/tmp"), pathlib.Path("/var/tmp")]
    socket_roots = [root for root in socket_roots if root.is_dir() and os.access(root, os.W_OK)]
    socket_dir = browser_socket_directory(SESSION[1:], socket_roots)
    socket_dir.mkdir(mode=0o700)
app_env = dict(base_environment)
app_env.update({
    "HOME": str(app_home),
    "TMPDIR": str(private_tmp),
    "XDG_CACHE_HOME": str(private_cache),
    "DOTZ_PORT": str(port),
    "DOTZ_CONFIG_DIR": str(config_dir),
    "DOTZ_TOKEN": "",
    "AGENT_BROWSER_SESSION": SESSION,
    "AGENT_BROWSER_EXECUTABLE_PATH": str(CHROMIUM or ""),
    "AGENT_BROWSER_ARGS": "--disable-gpu,--disable-dev-shm-usage",
    "AGENT_BROWSER_IDLE_TIMEOUT_MS": "15000",
    "AGENT_BROWSER_INIT_SCRIPTS": str(OUT / "capture-ui-errors.js"),
})
if socket_dir is not None:
    app_env["AGENT_BROWSER_SOCKET_DIR"] = str(socket_dir)
browser_env = dict(app_env)
if socket_dir is not None:
    # Chromium embeds TMPDIR in Unix-domain socket paths too; keep it on the selected short root.
    browser_env["TMPDIR"] = str(socket_dir.parent)

(OUT / "capture-ui-errors.js").write_text(
    "window.__dotzUiErrors = [];\n"
    "window.addEventListener('error', (event) => window.__dotzUiErrors.push({"
    "message:event.message||'', filename:event.filename||'', lineno:event.lineno||0, "
    "stack:event.error&&event.error.stack?String(event.error.stack):''}));\n"
    "window.addEventListener('unhandledrejection', (event) => {const reason=event.reason; "
    "window.__dotzUiErrors.push({message:reason&&reason.message?reason.message:String(reason), "
    "stack:reason&&reason.stack?String(reason.stack):''});});\n"
)

server_binary = {"path": str(SERVE), "built_during_test": False}
try:
    if not AGENT_COMMAND or not AGENT.is_file():
        check("agent-browser dependency available", False, str(AGENT))
    else:
        check("agent-browser dependency available", True, str(AGENT))
    if not CHROMIUM:
        check("Chromium executable available", False, "set CHROMIUM_PATH or install chromium")
    else:
        check("Chromium executable available", True, CHROMIUM)
    if AGENT_COMMAND and AGENT.is_file() and CHROMIUM:
        server_binary = ensure_serve_binary()
        (OUT / "serve.stdout.log").write_text("")
        (OUT / "serve.stderr.log").write_text("")
        with (OUT / "serve.stdout.log").open("w") as stdout, (OUT / "serve.stderr.log").open("w") as stderr:
            server = subprocess.Popen(
                [str(SERVE)],
                cwd=ROOT,
                env=app_env,
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
            )
            health_ok, health = wait_ready(base_url + "/api/health")
            check("actual backend health", health_ok, health)
            if health_ok:
                opened = run("open-app", "open", base_url + "/")
                browser_startup.record_open_result(opened["returncode"])
                check("browser opens app", browser_startup.ready, opened["stderr"])
                browser_startup.require_ready()
                run("set-viewport", "set", "viewport", "1920", "1080")
                run("wait-for-module-initialization", "wait", "1200")

                wizard_visible = evaluate(
                    "first-run-visible",
                    "(() => {const el=document.querySelector('#wizard-overlay');"
                    "return !!el && !el.classList.contains('hidden');})()",
                )
                check("first-run wizard visible on isolated fresh profile", wizard_visible is True, wizard_visible)
                wizard_steps = []
                if wizard_visible is True:
                    for label in ("SKIP", "NEXT", "SKIP", "FINISH ✓"):
                        label_js = json.dumps(label, ensure_ascii=False)
                        expression = (
                            "(() => {const button=Array.from(document.querySelectorAll('#wizard-overlay button'))"
                            f".find(el=>el.textContent.trim()==={label_js});"
                            "if(!button) throw new Error('missing onboarding button');"
                            "button.click(); return button.textContent.trim();})()"
                        )
                        action = run("onboarding-" + str(len(wizard_steps) + 1), "eval", expression)
                        wizard_steps.append({"button": label, "returncode": action["returncode"],
                                             "stdout": action["stdout"]})
                        run("wait-onboarding-step-" + str(len(wizard_steps)), "wait", "350")
                    wizard_closed = evaluate(
                        "first-run-hidden-after-finish",
                        "document.querySelector('#wizard-overlay') === null",
                    )
                    check("no-provider onboarding completes", wizard_closed is True, wizard_closed)

                settings_before = evaluate(
                    "settings-hidden-before-open",
                    "document.querySelector('#settings-card').classList.contains('hidden')",
                )
                check("settings modal starts closed", settings_before is True, settings_before)
                settings_click = run("open-settings", "click", "#settings-btn")
                run("wait-settings-open", "wait", "250")
                settings_state = evaluate(
                    "settings-state-after-open",
                    "JSON.stringify({hidden:document.querySelector('#settings-card').classList.contains('hidden'),"
                    "feed:document.querySelector('#settings-feed').textContent,"
                    "version:document.querySelector('#settings-version').textContent,"
                    "providerSelect:document.querySelector('#provider-select').options.length,"
                    "modelInput:document.querySelector('#model-input').value})",
                )
                settings_opened = (
                    settings_click["returncode"] == 0
                    and isinstance(settings_state, dict)
                    and settings_state.get("hidden") is False
                )
                check("settings opens and is populated", settings_opened, settings_state)
                check(
                    "browser/dev settings disclosure",
                    isinstance(settings_state, dict)
                    and settings_state.get("feed") == "not configured (browser/dev)"
                    and settings_state.get("version") == "browser",
                    settings_state,
                )
                run("settings-screenshot", "screenshot", str(OUT / "settings-open.png"))
                run("close-settings", "click", "#settings-close")
                settings_closed = evaluate(
                    "settings-hidden-after-close",
                    "document.querySelector('#settings-card').classList.contains('hidden')",
                )
                check("settings closes", settings_closed is True, settings_closed)

                # Open a temporary local project to expose the real bento panel surface. No prompt
                # is submitted: creating a session and loading local endpoints never calls a provider.
                project_setup = evaluate(
                    "fill-temporary-project-form",
                    "(() => {document.querySelector('#pf-name').value='UI init regression';"
                    f"document.querySelector('#pf-cwd').value={json.dumps(str(workspace))};"
                    "const model=document.querySelector('#pf-model'); if(model) model.value='glm-5.2';"
                    "return {profileOptions:document.querySelector('#pf-profile').options.length,"
                    "cwd:document.querySelector('#pf-cwd').value};})()",
                )
                run("open-project-dropdown", "click", "#project-select")
                run("start-create-project", "click", "#project-new-btn")
                # Refill after the controls have been exposed; the first eval above also proves the
                # form elements are available in the document even on the baseline that lacks handlers.
                project_setup = evaluate(
                    "set-project-form-values",
                    "(() => {document.querySelector('#pf-name').value='UI init regression';"
                    f"document.querySelector('#pf-cwd').value={json.dumps(str(workspace))};"
                    "document.querySelector('#pf-model').value='glm-5.2';"
                    "return {profileOptions:document.querySelector('#pf-profile').options.length};})()",
                )
                run("create-temporary-project", "click", "#pf-create")
                run("wait-project-session", "wait", "1400")
                bento_ready = evaluate(
                    "bento-session-visible",
                    "JSON.stringify({commandCenterHidden:document.querySelector('#command-center').classList.contains('hidden'),"
                    "bentoHidden:document.querySelector('#bento').classList.contains('hidden'),"
                    "chat:!!document.querySelector('.panel[data-panel=\"chat\"]'),"
                    "sessionCreated:!!document.querySelector('#bento .panel')})",
                )
                project_opened = (
                    isinstance(bento_ready, dict)
                    and bento_ready.get("commandCenterHidden") is True
                    and bento_ready.get("bentoHidden") is False
                    and bento_ready.get("chat") is True
                    and bento_ready.get("sessionCreated") is True
                )
                check("temporary no-provider project opens local session", project_opened, bento_ready)

                panel_definitions = evaluate(
                    "panel-registry-export",
                    "import('/panel-registry.js').then(({PANEL_DEFINITIONS}) => "
                    "JSON.stringify(PANEL_DEFINITIONS))",
                )
                check(
                    "panel icons labels colors and tool mappings unchanged",
                    panel_definitions == PANEL_DEFINITIONS_EXPECTED,
                    {"expected": PANEL_DEFINITIONS_EXPECTED, "actual": panel_definitions},
                )

                palette_items = evaluate(
                    "panel-palette-items",
                    "(() => {document.querySelector('#panels-btn').click();"
                    "return JSON.stringify(Array.from(document.querySelectorAll('#palette-grid .palette-item'))"
                    ".map(el=>el.querySelector('.pi-label')?.textContent.trim()||''));})()",
                )
                check(
                    "panel palette metadata unchanged",
                    palette_items == [label for _, label in PANEL_EXPECTATIONS],
                    {"expected": [label for _, label in PANEL_EXPECTATIONS], "actual": palette_items},
                )
                panel_results = evaluate(
                    "open-and-close-every-panel",
                    "(() => {const expected=" + json.dumps(PANEL_EXPECTATIONS) + ";"
                    "const results=[]; for(const [name,label] of expected){"
                    "if(name==='chat'){const chat=document.querySelector('.panel[data-panel=\"chat\"]');"
                    "results.push({name,label,opened:!!chat,role:chat?.getAttribute('role'),"
                    "ariaLabel:chat?.getAttribute('aria-label'),closed:null});continue;}"
                    "document.querySelector('#panels-btn').click();"
                    "const item=Array.from(document.querySelectorAll('#palette-grid .palette-item'))"
                    ".find(el=>el.querySelector('.pi-label')?.textContent.trim()===label);"
                    "if(item)item.click();"
                    "const panel=document.querySelector('.panel[data-panel=\"'+name+'\"]');"
                    "const result={name,label,menuItemFound:!!item,opened:!!panel,"
                    "role:panel?.getAttribute('role'),ariaLabel:panel?.getAttribute('aria-label'),closed:false};"
                    "panel?.querySelector('.panel-close')?.click();"
                    "result.closed=!document.querySelector('.panel[data-panel=\"'+name+'\"]');"
                    "results.push(result);} return JSON.stringify(results);})()",
                )
                check(
                    "all critical panels mount from the browser palette",
                    isinstance(panel_results, list)
                    and len(panel_results) == len(PANEL_EXPECTATIONS)
                    and all(
                        item.get("opened") is True
                        and item.get("role") == "region"
                        and bool(item.get("ariaLabel"))
                        and (item.get("closed") is None or item.get("closed") is True)
                        for item in panel_results
                    ),
                    panel_results,
                )
                run("wait-panel-loaders", "wait", "1000")
                js_errors = evaluate(
                    "captured-javascript-errors",
                    "JSON.stringify(window.__dotzUiErrors || [])",
                )
                check("no browser JavaScript errors", js_errors == [], js_errors)

                # The normal server serves the actual `web/` tree (DOTZ_WEB_DIR intentionally unset).
                asset_status = None
                asset_content_type = None
                asset_excerpt = ""
                try:
                    with urllib.request.urlopen(base_url + "/panel-registry.js", timeout=5) as response:
                        asset_status = response.status
                        asset_content_type = response.headers.get("Content-Type")
                        asset_excerpt = response.read(4096).decode("utf-8", "replace")
                except urllib.error.HTTPError as exc:
                    asset_status = exc.code
                except Exception as exc:
                    asset_excerpt = repr(exc)
                check(
                    "actual devserver serves panel-registry.js",
                    asset_status == 200 and "PANEL_DEFINITIONS" in asset_excerpt,
                    {"status": asset_status, "content_type": asset_content_type,
                     "excerpt": asset_excerpt[:180]},
                )
                tauri_config = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())
                tauri_main = (ROOT / "src-tauri/src/main.rs").read_text()
                resource_map = tauri_config.get("bundle", {}).get("resources", {})
                resource_config = (
                    tauri_config.get("build", {}).get("frontendDist") == "../web"
                    and resource_map.get("../web") == "web"
                    and "res.join(\"web\")" in tauri_main
                    and not os.environ.get("DOTZ_WEB_DIR")
                )
                check(
                    "Tauri native resource mapping preserves the web directory",
                    resource_config,
                    {"frontendDist": tauri_config.get("build", {}).get("frontendDist"),
                     "resourceMapping": resource_map.get("../web"),
                     "runtimeResolvesResourceWebDirectory": "res.join(\"web\")" in tauri_main},
                )
                registry_file = ROOT / "web/panel-registry.js"
                check(
                    "native resource source tree contains panel-registry.js",
                    resource_config and registry_file.is_file(),
                    {"sourceFile": str(registry_file), "expectedBundledPath": "web/panel-registry.js",
                     "exists": registry_file.is_file()},
                )
                network = run("network-requests", "network", "requests")
                (OUT / "network-requests.log").write_text(network["stdout"])
                external_lines = [
                    line for line in network["stdout"].splitlines()
                    if "http://127.0.0.1:" not in line and "ws://127.0.0.1:" not in line
                ]
                check("browser network stays on local backend", not external_lines, external_lines[:20])

                marker = config_dir / "first-run-done"
                check("onboarding writes only the isolated config marker", marker.is_file(), str(marker))
                check("provider auth file remains absent", not (app_home / ".pi/agent/auth.json").exists(),
                      str(app_home / ".pi/agent/auth.json"))
except BrowserStartupUnavailable as exc:
    # `open-app` already recorded the primary failure. Do not run dependent checks or report
    # each skipped browser operation as another failure.
    checks.append({
        "name": "browser-dependent checks skipped after startup failure",
        "passed": True,
        "detail": str(exc),
    })
except Exception as exc:
    failures.append(f"test harness exception: {exc!r}")
finally:
    if server is not None:
        try:
            subprocess.run(
                [*AGENT_COMMAND, "--session", SESSION, "close", "--all"],
                cwd=ROOT,
                env=browser_env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
        except Exception:
            pass
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)
    if socket_dir is not None:
        shutil.rmtree(socket_dir, ignore_errors=True)

result = {
    "revision": subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False
    ).stdout.strip(),
    "server_binary": server_binary,
    "server_cwd": str(ROOT),
    "DOTZ_WEB_DIR_override": os.environ.get("DOTZ_WEB_DIR"),
    "isolated_home": str(app_home),
    "agent_browser_socket_dir": str(socket_dir) if socket_dir is not None else None,
    "removed_provider_environment_names": removed_provider_vars,
    "provider_or_prompt_call": False,
    "first_run_wizard_buttons": ["SKIP", "NEXT", "SKIP", "FINISH ✓"],
    "checks": checks,
    "failures": failures,
    "passed_check_count": sum(1 for item in checks if item["passed"]),
    "failed_check_count": sum(1 for item in checks if not item["passed"]),
    "step_count": len(steps),
    "steps": steps,
    "evidence": {"directory": str(OUT), "settings_screenshot": str(OUT / "settings-open.png"),
                 "network_requests": str(OUT / "network-requests.log"),
                 "server_stdout": str(OUT / "serve.stdout.log"),
                 "server_stderr": str(OUT / "serve.stderr.log")},
}
(OUT / "result.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps({key: value for key, value in result.items() if key != "steps"}, indent=2))
raise SystemExit(1 if failures else 0)
