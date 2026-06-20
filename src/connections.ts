/**
 * Local "Connections" — surfaces each provider's existing browser-CLI auth and offers a
 * one-click browser login on THIS machine. No OAuth app registration, no client secrets:
 * the provider CLIs (gh / vercel / neonctl) own the device/browser flow and the token storage,
 * so dotz only reads their auth status and shells their login. Tokens are never read or logged —
 * the streamed output is the CLI's own device-code / URL prompt, which the user completes in a
 * normal browser tab.
 */
import { spawn } from "node:child_process";

export type ConnectionProviderId = "github" | "vercel" | "neon";

export interface ConnectionStatus {
  id: ConnectionProviderId;
  label: string;
  cli: string;
  installed: boolean;
  loggedIn: boolean;
  account?: string;
  hint?: string;
}

export interface LoginState {
  provider: ConnectionProviderId;
  running: boolean;
  exitCode: number | null;
  output: string;
}

interface CmdResult { code: number | null; stdout: string; stderr: string; }

const OUTPUT_CAP = 16_384;
const LOGIN_TIMEOUT_MS = 180_000;
const STATUS_TIMEOUT_MS = 12_000;
const LOGOUT_TIMEOUT_MS = 30_000;

/** Run a fixed first-party command (no user input) through the shell, capturing combined output. */
function runCommand(command: string, timeoutMs = STATUS_TIMEOUT_MS): Promise<CmdResult> {
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    let settled = false;
    const finish = (code: number | null) => { if (!settled) { settled = true; resolve({ code, stdout, stderr }); } };
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn(command, { shell: true, windowsHide: true });
    } catch (err) {
      resolve({ code: 127, stdout: "", stderr: String((err as Error).message) });
      return;
    }
    const timer = setTimeout(() => { try { child.kill(); } catch { /* gone */ } finish(null); }, timeoutMs);
    child.stdout?.on("data", (d: Buffer) => { stdout = (stdout + d.toString()).slice(-OUTPUT_CAP); });
    child.stderr?.on("data", (d: Buffer) => { stderr = (stderr + d.toString()).slice(-OUTPUT_CAP); });
    child.on("error", (err) => { stderr += String(err.message); clearTimeout(timer); finish(127); });
    child.on("close", (code) => { clearTimeout(timer); finish(code); });
  });
}

function notInstalled(r: CmdResult): boolean {
  const s = `${r.stderr}\n${r.stdout}`.toLowerCase();
  return r.code === 127 || s.includes("not recognized") || s.includes("command not found") || s.includes("no such file");
}

interface ProviderSpec {
  id: ConnectionProviderId;
  label: string;
  cli: string;
  statusCommand: string;
  loginCommand: string;
  logoutCommand: string;
  parse(r: CmdResult): { installed: boolean; loggedIn: boolean; account?: string; hint?: string };
}

const PROVIDERS: ProviderSpec[] = [
  {
    id: "github", label: "GitHub", cli: "gh",
    statusCommand: "gh auth status",
    loginCommand: "gh auth login --web --hostname github.com --git-protocol https --skip-ssh-key",
    logoutCommand: "gh auth logout --hostname github.com",
    parse(r) {
      if (notInstalled(r)) return { installed: false, loggedIn: false, hint: "install the GitHub CLI (gh)" };
      const out = `${r.stdout}\n${r.stderr}`;
      // `gh auth status` prints "Logged in to <host> account <name>" only when authenticated; the
      // exit code is unreliable (it does a network token check that can fail/lag while still logged in),
      // so trust the text. "not logged into any GitHub hosts" lacks the "in to" form and won't match.
      const loggedIn = /logged in to/i.test(out);
      const account = out.match(/account\s+([A-Za-z0-9-]+)/i)?.[1];
      return { installed: true, loggedIn, account, hint: loggedIn ? undefined : "not logged in" };
    },
  },
  {
    id: "vercel", label: "Vercel", cli: "vercel",
    statusCommand: "vercel whoami",
    loginCommand: "vercel login",
    logoutCommand: "vercel logout",
    parse(r) {
      if (notInstalled(r)) return { installed: false, loggedIn: false, hint: "install the Vercel CLI" };
      const lines = `${r.stdout}\n${r.stderr}`.split(/\r?\n/).map((l) => l.trim()).filter((l) => l && !l.startsWith("<"));
      const loggedIn = r.code === 0 && lines.length > 0;
      return { installed: true, loggedIn, account: loggedIn ? lines[lines.length - 1] : undefined, hint: loggedIn ? undefined : "not logged in" };
    },
  },
  {
    id: "neon", label: "Neon", cli: "neonctl",
    statusCommand: "neonctl me",
    loginCommand: "npx -y neonctl@latest auth",
    logoutCommand: "npx -y neonctl@latest auth --logout",
    parse(r) {
      if (notInstalled(r)) return { installed: false, loggedIn: false, hint: "click Log in — installs neonctl via npx, then opens the browser" };
      const loggedIn = r.code === 0;
      const account = `${r.stdout}\n${r.stderr}`.split(/\r?\n/).map((l) => l.trim()).find((l) => /@|\bid\b/i.test(l));
      return { installed: true, loggedIn, account, hint: loggedIn ? undefined : "not logged in" };
    },
  },
];

interface LoginSession {
  child: ReturnType<typeof spawn>;
  output: string;
  running: boolean;
  exitCode: number | null;
  killTimer: ReturnType<typeof setTimeout>;
}

export class ConnectionsController {
  private readonly logins = new Map<ConnectionProviderId, LoginSession>();

  private spec(provider: string): ProviderSpec {
    const s = PROVIDERS.find((p) => p.id === provider);
    if (!s) throw new Error(`unknown connection provider: ${provider}`);
    return s;
  }

  /** All three providers' live auth status, checked in parallel. */
  async status(): Promise<ConnectionStatus[]> {
    return Promise.all(PROVIDERS.map(async (p) => {
      const parsed = p.parse(await runCommand(p.statusCommand));
      const login = this.logins.get(p.id);
      return {
        id: p.id, label: p.label, cli: p.cli,
        installed: parsed.installed, loggedIn: parsed.loggedIn, account: parsed.account,
        hint: login?.running ? "browser login in progress…" : parsed.hint,
      };
    }));
  }

  /** Spawn the provider CLI's own browser login on this machine and stream its output. */
  login(provider: string): LoginState {
    const spec = this.spec(provider);
    this.kill(spec.id);
    const child = spawn(spec.loginCommand, { shell: true, windowsHide: true });
    const session: LoginSession = {
      child, output: "", running: true, exitCode: null,
      killTimer: setTimeout(() => this.kill(spec.id), LOGIN_TIMEOUT_MS),
    };
    this.logins.set(spec.id, session);
    const append = (chunk: Buffer) => { session.output = (session.output + chunk.toString()).slice(-OUTPUT_CAP); };
    child.stdout?.on("data", append);
    child.stderr?.on("data", append);
    child.on("error", (err) => { session.output += `\n[error] ${err.message}`; session.running = false; });
    child.on("close", (code) => { session.running = false; session.exitCode = code; clearTimeout(session.killTimer); });
    // Advance the CLI's initial "press Enter to open the browser" prompt so the system browser opens.
    setTimeout(() => { try { child.stdin?.write("\n"); } catch { /* stdin may already be closed */ } }, 600);
    return this.loginState(spec.id);
  }

  loginState(provider: string): LoginState {
    const spec = this.spec(provider);
    const s = this.logins.get(spec.id);
    return { provider: spec.id, running: !!s?.running, exitCode: s?.exitCode ?? null, output: s?.output ?? "" };
  }

  async logout(provider: string): Promise<{ ok: boolean; output: string }> {
    const spec = this.spec(provider);
    this.kill(spec.id);
    const r = await runCommand(spec.logoutCommand, LOGOUT_TIMEOUT_MS);
    return { ok: r.code === 0, output: `${r.stdout}\n${r.stderr}`.trim() };
  }

  private kill(provider: ConnectionProviderId): void {
    const s = this.logins.get(provider);
    if (s) { clearTimeout(s.killTimer); try { s.child.kill(); } catch { /* already gone */ } s.running = false; }
  }

  disposeAll(): void {
    for (const id of [...this.logins.keys()]) this.kill(id);
  }
}

export const connectionsController = new ConnectionsController();
