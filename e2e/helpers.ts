import { spawn, type Subprocess } from "bun";
import { mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const BINARY = join(
  import.meta.dir,
  "..",
  "target",
  "debug",
  "local-proxy" + (process.platform === "win32" ? ".exe" : ""),
);

export interface ProxyHandle {
  proc: Subprocess;
  base: string;
  port: number;
}

/** Reserve an ephemeral port by binding a throwaway TCP listener to `0`. */
export function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      const port = typeof addr === "object" && addr ? addr.port : 0;
      srv.close(() => resolve(port));
    });
  });
}

/** Write a config file into a throwaway temp dir; returns its path. */
export function writeConfig(yml: string): string {
  const dir = mkdtempSync(join(tmpdir(), "local-proxy-e2e-"));
  const path = join(dir, "config.yaml");
  writeFileSync(path, yml);
  return path;
}

/** Poll `GET /health` until the proxy responds 200 or the timeout elapses. */
export async function waitHealth(base: string, timeoutMs = 20000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(`${base}/health`);
      if (r.ok) return;
    } catch {
      // not up yet
    }
    await Bun.sleep(200);
  }
  throw new Error(`proxy did not become healthy at ${base}`);
}

/** Spawn the compiled proxy with a config file; waits for health. */
export async function startProxy(
  configYaml: string,
  requestedPort?: number,
  env: Record<string, string> = {},
): Promise<ProxyHandle> {
  const port = requestedPort ?? (await findFreePort());
  const cfg = writeConfig(configYaml);
  const proc = spawn([BINARY, "serve", "--config", cfg, "--port", String(port)], {
    stdout: "ignore",
    stderr: "pipe",
    env: { ...process.env, ...env },
  });
  const base = `http://127.0.0.1:${port}`;
  try {
    await waitHealth(base);
  } catch (e) {
    proc.kill();
    throw e;
  }
  return { proc, base, port };
}

/** Run the compiled CLI once, returning its combined output and exit code. */
export async function runCli(
  args: string[],
  env: Record<string, string> = {},
): Promise<{ exit: number; output: string }> {
  const p = spawn([BINARY, ...args], {
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, ...env },
  });
  const out = (await new Response(p.stdout).text()) + (await new Response(p.stderr).text());
  const exit = await p.exited;
  return { exit, output: out };
}

export function stopProxy(_h: ProxyHandle): void {
  try {
    _h.proc.kill();
  } catch {
    // already dead
  }
  // best-effort: clear temp config dirs so the OS temp dir stays clean
  try {
    for (const name of readdirSync(tmpdir())) {
      if (name.startsWith("local-proxy-e2e-")) {
        try {
          rmSync(join(tmpdir(), name), { recursive: true, force: true });
        } catch {
          // ignore
        }
      }
    }
  } catch {
    // ignore
  }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

export async function get(
  base: string,
  path: string,
  headers: Record<string, string> = {},
  timeoutMs = 30000,
): Promise<Response> {
  return fetch(`${base}${path}`, {
    headers,
    signal: AbortSignal.timeout(timeoutMs),
  });
}

export async function postJson(
  base: string,
  path: string,
  body: unknown,
  headers: Record<string, string> = {},
  timeoutMs = 120000,
): Promise<Response> {
  return fetch(`${base}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
}

export async function readBody(res: Response): Promise<string> {
  return res.text();
}

/** POST with retry on 429/5xx (free upstream models rate-limit easily). */
export async function postJsonRetry(
  base: string,
  path: string,
  body: unknown,
  headers: Record<string, string> = {},
  opts: { attempts?: number; timeoutMs?: number } = {},
): Promise<Response> {
  const attempts = opts.attempts ?? 4;
  const timeoutMs = opts.timeoutMs ?? 120000;
  let last: Response | undefined;
  for (let i = 0; i < attempts; i++) {
    const r = await postJson(base, path, body, headers, timeoutMs);
    if (r.status === 429 || r.status >= 500) {
      last = r;
      await Bun.sleep(1000 * (i + 1) + Math.random() * 500);
      continue;
    }
    return r;
  }
  return last!;
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

export interface SseFrame {
  event?: string;
  data: string;
}

export function parseSse(text: string): SseFrame[] {
  const frames: SseFrame[] = [];
  for (const chunk of text.split("\n\n")) {
    let event: string | undefined;
    let data = "";
    for (const line of chunk.split("\n")) {
      if (line.startsWith("event:")) {
        event = line.slice("event:".length).trim();
      } else if (line.startsWith("data:")) {
        data = (data ? data + "\n" : "") + line.slice("data:".length).trimStart();
      }
    }
    if (data) frames.push({ event, data });
  }
  return frames;
}

export const eventNames = (frames: SseFrame[]): string[] =>
  frames.filter((f) => f.event).map((f) => f.event!);

export function framesJson(frames: SseFrame[], event?: string): unknown[] {
  return frames
    .filter((f) => f.event === event)
    .map((f) => JSON.parse(f.data));
}

export function frameData(frames: SseFrame[], event: string): string[] {
  return frames.filter((f) => f.event === event).map((f) => f.data);
}
