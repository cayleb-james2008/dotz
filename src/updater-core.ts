/**
 * Pure, electron-free core of the dotz source-rebuild updater.
 *
 * Kept separate from updater.ts (the electron shell) so the git-check logic and the
 * detached-helper script generation can be unit-tested without booting electron or
 * spawning real processes. updater.ts injects a real git runner + spawn; tests inject
 * fakes.
 */

/** Result of running a git subcommand. */
export interface GitResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** A function that runs `git <args>` in the repo and resolves the result (never throws). */
export type GitRunner = (args: string[]) => Promise<GitResult>;

export interface CheckResult {
  ok: boolean;
  behind: number;
  localSha: string;
  remoteSha: string;
  dirty: boolean;
  error?: string;
}

/** Tracking branch the local HEAD pulls from (e.g. origin/master); defaults to origin/main. */
export async function trackingRef(git: GitRunner): Promise<string> {
  const r = await git(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
  const ref = r.code === 0 ? r.stdout.trim() : "";
  return ref || "origin/main";
}

/**
 * Check whether the local checkout is behind its tracking branch:
 *   git fetch → rev-list --left-right --count HEAD...<upstream> → short shas → dirty?
 */
export async function checkForUpdate(git: GitRunner): Promise<CheckResult> {
  const fetch = await git(["fetch", "--quiet"]);
  if (fetch.code !== 0) {
    return { ok: false, behind: 0, localSha: "", remoteSha: "", dirty: false, error: fetch.stderr.trim() || "git fetch failed" };
  }
  const ref = await trackingRef(git);
  const counts = await git(["rev-list", "--left-right", "--count", `HEAD...${ref}`]);
  if (counts.code !== 0) {
    return { ok: false, behind: 0, localSha: "", remoteSha: "", dirty: false, error: counts.stderr.trim() || "git rev-list failed" };
  }
  // "<ahead>\t<behind>" — second column is commits on the remote not in HEAD.
  const behind = Number(counts.stdout.trim().split(/\s+/)[1] || 0);
  const local = await git(["rev-parse", "--short", "HEAD"]);
  const remote = await git(["rev-parse", "--short", ref]);
  const statusOut = await git(["status", "--porcelain"]);
  const dirty = statusOut.stdout.trim().length > 0;
  return { ok: true, behind, localSha: local.stdout.trim(), remoteSha: remote.stdout.trim(), dirty };
}

export interface HelperOpts {
  repo: string;
  pid: number;
  exePath: string;
  rebuildScript: string;
  isPackaged: boolean;
}

/**
 * Contents of the detached helper .bat:
 *   wait-for-exit (parent pid) → git pull --ff-only → portable rebuild → relaunch.
 * In a packaged portable build, relaunch the freshly-built exe at the same path; in dev
 * there is no portable exe, so relaunch `npm run electron`.
 */
export function buildHelperScript(opts: HelperOpts): string {
  const { repo, pid, exePath, rebuildScript, isPackaged } = opts;
  // Packaged: relaunch the FRESHLY-REBUILT portable exe — the newest release\dotz*.exe — NOT
  // process.execPath. A portable exe runs from a temp dir electron-builder RMDir's on exit (so
  // process.execPath is already gone by relaunch time), and a version bump renames the artifact, so
  // a hard-coded launched path also goes stale. Glob the newest build; fall back to the on-disk
  // launched path (exePath) only if release\ has no exe (e.g. the repo dir vanished). Dev: npm run electron.
  const relaunch = isPackaged
    ? [
        `set "DOTZ_EXE="`,
        `for /f "delims=" %%F in ('dir /b /o-d "release\\dotz*.exe" 2^>nul') do if not defined DOTZ_EXE set "DOTZ_EXE=%CD%\\release\\%%F"`,
        `if defined DOTZ_EXE ( start "" "%DOTZ_EXE%" ) else ( start "" "${exePath}" )`,
      ]
    : [`start "" cmd /c "npm run electron"`];
  return [
    `@echo off`,
    // Guard the cd: a missing/unmounted repo dir must abort to :fail (relaunch current build), not
    // run git pull + the rebuild in whatever directory the detached console happened to inherit.
    `cd /d "${repo}" || goto fail`,
    `echo [dotz-update] waiting for dotz (pid ${pid}) to exit...`,
    `:waitloop`,
    `tasklist /FI "PID eq ${pid}" 2>nul | find "${pid}" >nul`,
    `if not errorlevel 1 ( timeout /t 1 /nobreak >nul & goto waitloop )`,
    `echo [dotz-update] pulling...`,
    `call git pull --ff-only || goto fail`,
    `echo [dotz-update] rebuilding portable exe...`,
    `call npm run ${rebuildScript} || goto fail`,
    `echo [dotz-update] relaunching...`,
    ...relaunch,
    `goto done`,
    `:fail`,
    `echo [dotz-update] update FAILED -- see above. Relaunching current build.`,
    ...relaunch,
    `:done`,
  ].join("\r\n");
}
