/**
 * Unit tests for the source-rebuild updater core (src/updater-core.ts).
 *
 * Pure + offline: the git layer is injected as a fake GitRunner (no real git, no network),
 * and buildHelperScript is a pure string function (no spawn, no electron-builder run).
 *
 *   node --test --import tsx scripts/verify-updater.ts
 *   (wired as `npm run test:updater`)
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { checkForUpdate, buildHelperScript, type GitRunner, type GitResult } from "../src/updater-core";

const OK = (stdout = ""): GitResult => ({ code: 0, stdout, stderr: "" });
const ERR = (stderr: string): GitResult => ({ code: 1, stdout: "", stderr });

/** Build a fake git runner from a per-arg-key response map; records the calls it received. */
function fakeGit(responses: Record<string, GitResult>): GitRunner & { calls: string[][] } {
  const calls: string[][] = [];
  const fn = (async (args: string[]) => {
    calls.push(args);
    const key = args[0];
    return responses[key] ?? OK();
  }) as GitRunner & { calls: string[][] };
  fn.calls = calls;
  return fn;
}

test("checkForUpdate: behind by N → ok with behind count, shas, clean tree", async () => {
  const git = fakeGit({
    fetch: OK(),
    "rev-parse": OK(), // covers @{u}, HEAD, ref short shas (overridden below per-call)
    "rev-list": OK("0\t3\n"), // ahead=0  behind=3
    status: OK(""), // clean
  });
  // rev-parse is called 3x with different output; sequence it.
  const seq = ["origin/master\n", "abc1234\n", "def5678\n"];
  let i = 0;
  const wrapped: GitRunner = async (args) => {
    if (args[0] === "rev-parse") return OK(seq[i++]);
    return git(args);
  };
  const res = await checkForUpdate(wrapped);
  assert.equal(res.ok, true);
  assert.equal(res.behind, 3);
  assert.equal(res.localSha, "abc1234");
  assert.equal(res.remoteSha, "def5678");
  assert.equal(res.dirty, false);
});

test("checkForUpdate: up to date → behind 0", async () => {
  const git = fakeGit({
    fetch: OK(),
    "rev-list": OK("0\t0\n"),
    status: OK(""),
  });
  const res = await checkForUpdate(git);
  assert.equal(res.ok, true);
  assert.equal(res.behind, 0);
  assert.equal(res.dirty, false);
});

test("checkForUpdate: dirty tree flagged", async () => {
  const git = fakeGit({
    fetch: OK(),
    "rev-list": OK("0\t2\n"),
    status: OK(" M src/main.ts\n?? new.ts\n"),
  });
  const res = await checkForUpdate(git);
  assert.equal(res.behind, 2);
  assert.equal(res.dirty, true);
});

test("checkForUpdate: git fetch failure → ok=false with error, no rev-list", async () => {
  const git = fakeGit({ fetch: ERR("fatal: unable to access remote") });
  const res = await checkForUpdate(git);
  assert.equal(res.ok, false);
  assert.match(res.error ?? "", /unable to access/);
  assert.ok(!git.calls.some((c) => c[0] === "rev-list"), "must not run rev-list after a failed fetch");
});

test("checkForUpdate: defaults to origin/main when no upstream is configured", async () => {
  // @{u} resolution fails (code 1) → trackingRef falls back to origin/main.
  const calls: string[][] = [];
  const git: GitRunner = async (args) => {
    calls.push(args);
    if (args[0] === "fetch") return OK();
    if (args[0] === "rev-parse" && args.includes("@{u}")) return ERR("no upstream");
    if (args[0] === "rev-list") return OK("0\t1\n");
    if (args[0] === "rev-parse") return OK("0000000\n");
    if (args[0] === "status") return OK("");
    return OK();
  };
  const res = await checkForUpdate(git);
  assert.equal(res.ok, true);
  assert.equal(res.behind, 1);
  const revList = calls.find((c) => c[0] === "rev-list");
  assert.ok(revList && revList.some((a) => a.includes("origin/main")), "rev-list should target origin/main fallback");
});

test("buildHelperScript (packaged): waits for pid, ff-pulls, rebuilds, relaunches exe", () => {
  const bat = buildHelperScript({
    repo: "C:\\repo\\dotz",
    pid: 4242,
    exePath: "C:\\repo\\dotz\\release\\dotz 0.2.0.exe",
    rebuildScript: "dist:portable",
    isPackaged: true,
  });
  assert.match(bat, /PID eq 4242/, "waits on the parent pid");
  assert.match(bat, /git pull --ff-only/, "ff-only pull");
  assert.match(bat, /npm run dist:portable/, "uses the portable rebuild script");
  assert.match(bat, /cd \/d "C:\\repo\\dotz" \|\| goto fail/, "cd is guarded so a missing repo aborts to :fail");
  assert.match(bat, /dir \/b \/o-d "release\\dotz\*\.exe"/, "relaunches the NEWEST freshly-built release exe, not process.execPath");
  assert.match(bat, /else \( start "" "C:\\repo\\dotz\\release\\dotz 0\.2\.0\.exe" \)/, "falls back to the on-disk launched exe");
  assert.ok(bat.includes("\r\n"), "CRLF line endings for a .bat");
});

test("buildHelperScript (dev): relaunches via npm run electron, not an exe", () => {
  const bat = buildHelperScript({
    repo: "C:\\repo\\dotz",
    pid: 9,
    exePath: "C:\\anything.exe",
    rebuildScript: "dist:portable",
    isPackaged: false,
  });
  assert.match(bat, /npm run electron/, "dev relaunch uses npm run electron");
  assert.ok(!bat.includes("anything.exe"), "dev must not relaunch a portable exe path");
});
