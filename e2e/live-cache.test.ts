// Live prompt-cache check against the real Anthropic API, required before a
// release (AGENTS.md, item 6). Runs the real `claude` CLI in forward mode
// (HTTPS_PROXY + NODE_EXTRA_CA_CERTS, exactly what `launch claude` sets)
// through a proxy on the user's real config, for two turns of one session,
// and asserts the second turn reads the prefix the first one cached. Runs once
// per model: a Claude model and a ChatGPT model behind a Code Mode id (what
// forward `launch --model chatgpt/...` and `/model` send).
//
// Opt-in (spends subscription quota): LOCAL_PROXY_LIVE=1 bun test live-cache.test.ts
// Read-only on the credential store: the proxy only reads accounts.db.
import { spawn } from "bun";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, openSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { BINARY, findFreePort, waitHealth, type ProxyHandle } from "./helpers";

// Comma-separated; override with LOCAL_PROXY_LIVE_MODELS.
const MODELS = (process.env.LOCAL_PROXY_LIVE_MODELS
  ?? "claude-sonnet-5-5,claude-opus-5-5+chatgpt/gpt-5.6-luna").split(",").map((m) => m.trim()).filter(Boolean);
const configDir = process.platform === "win32"
  ? join(process.env.APPDATA ?? "", "local-proxy", "config")
  : join(process.env.HOME ?? "", ".config", "local-proxy");
const configPath = join(configDir, "config.yaml");

const skipReason = process.env.LOCAL_PROXY_LIVE !== "1" ? "set LOCAL_PROXY_LIVE=1"
  : !existsSync(configPath) ? `no config at ${configPath}`
  : !Bun.which("claude") ? "claude CLI not on PATH"
  : "";

let proxy: ProxyHandle;
const workDir = mkdtempSync(join(tmpdir(), "live-cache-"));
/** Proxy log, kept after the run for diagnosis. */
const proxyLog = join(tmpdir(), "live-cache-proxy.log");
const children = new Set<ReturnType<typeof spawn>>();

beforeAll(async () => {
  if (skipReason) return;
  const port = await findFreePort();
  const log = openSync(proxyLog, "w");
  // A Claude id the router can't resolve (model discovery down) falls back to
  // the active model; pin it so the user's own selection can't steal the run.
  const fallback = process.env.LOCAL_PROXY_LIVE_FALLBACK ?? "claude/claude-sonnet-5-5";
  const proc = spawn([BINARY, "serve", "--config", configPath, "--port", String(port), "--model", fallback], {
    stdout: log, stderr: log, env: { ...process.env, RUST_LOG: "local_proxy=debug" },
  });
  proxy = { proc, base: `http://127.0.0.1:${port}`, port };
  await waitHealth(proxy.base, 25000);
}, 30000);

afterAll(async () => {
  for (const c of children) c.kill();
  proxy?.proc.kill();
  await Promise.all([...children].map((c) => c.exited));
  rmSync(workDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
  console.log(`[live-cache] proxy log: ${proxyLog}`);
});

async function claude(model: string, args: string[]) {
  const env: Record<string, string | undefined> = {
    ...process.env,
    HTTPS_PROXY: proxy.base,
    NODE_EXTRA_CA_CERTS: join(configDir, "forward-ca.pem"),
  };
  for (const k of ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "HTTP_PROXY", "NO_PROXY", "no_proxy"]) delete env[k];
  const child = spawn(["claude", "-p", ...args, "--model", model, "--output-format", "json", "--max-turns", "1"], { cwd: workDir, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  children.add(child);
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; child.kill(); }, 75000);
  const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
  clearTimeout(timer);
  children.delete(child);
  expect(timedOut, `claude hung for 75s (see ${proxyLog}): ${stdout}${stderr}`).toBe(false);
  expect(code, stdout + stderr).toBe(0);
  const out = JSON.parse(stdout);
  expect(out.is_error, stdout).toBe(false);
  return out as { session_id: string; usage: { cache_read_input_tokens?: number; cache_creation_input_tokens?: number } };
}

describe.skipIf(skipReason)("live: prompt cache via real API (forward mode)", () => {
  test.each(MODELS)("%s: the last of three turns reads the cached prefix", async (model) => {
    // Three turns, asserting the last: OpenAI sometimes misses right after
    // the first write, which would make a two-turn check flaky.
    const first = await claude(model, ["Reply with exactly: one"]);
    await claude(model, ["--resume", first.session_id, "Reply with exactly: two"]);
    const last = await claude(model, ["--resume", first.session_id, "Reply with exactly: three"]);
    console.log("[live-cache]", model, { first: first.usage, last: last.usage });
    expect(last.usage.cache_read_input_tokens ?? 0).toBeGreaterThan(0);
  }, 300000);
});
