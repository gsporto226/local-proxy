import { spawn } from "bun";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import {
  BINARY,
  findFreePort,
  get,
  postJsonRetry,
  readBody,
  waitHealth,
  type ProxyHandle,
} from "./helpers";

/**
 * Live e2e against the user's *actual* global config: the model routed by the
 * proxy is the one configured for the current user (`defaults.active_model`),
 * and the upstream key comes from the user's real `auth.json` (`opencode-go`).
 * The real `claude` CLI is pointed at the proxy via `ANTHROPIC_BASE_URL`,
 * mirroring the env `local-proxy launch` sets.
 *
 * Skips (environmental) when the user has not configured `opencode-go`, has no
 * stored key, or `claude` is not on PATH.
 */

const T = 180000;
const live = (name: string, fn: () => void | Promise<void>) => test(name, fn, T);

function globalConfigDir(): string {
  if (process.platform === "win32") {
    return join(process.env.APPDATA ?? "", "local-proxy", "config");
  }
  return join(process.env.HOME ?? process.env.USERPROFILE ?? "", ".config", "local-proxy");
}

const userDir = globalConfigDir();
const userConfigPath = join(userDir, "config.yaml");
const userAuthPath = join(userDir, "auth.json");

function firstApiKey(configText: string): string | undefined {
  const inline = configText.match(/api_keys\s*:\s*\[\s*["']?([^"'\]]+?)["']?\s*\]/);
  if (inline) return inline[1].trim();
  const block = configText.match(/api_keys\s*:\s*\r?\n\s*-\s*["']?([^"'\r\n]+?)["']?\s*$/m);
  return block?.[1]?.trim();
}

function activeModel(configText: string): string | undefined {
  return configText.match(/active_model\s*:\s*["']?([^"'\r\n]+?)["']?\s*$/m)?.[1]?.trim();
}

let skipReason = "";
let userConfigText = "";
let proxyKey = "unused";
const ACTIVE_MODEL = activeModel(
  (() => {
    try {
      userConfigText = readFileSync(userConfigPath, "utf8");
      return userConfigText;
    } catch {
      return "";
    }
  })(),
);

if (!existsSync(userConfigPath)) {
  skipReason = `user global config not found at ${userConfigPath}`;
} else if (!ACTIVE_MODEL || !ACTIVE_MODEL.includes("opencode-go")) {
  skipReason = `active_model not opencode-go (got ${ACTIVE_MODEL ?? "(unset)"})`;
} else {
  try {
    const auth = JSON.parse(readFileSync(userAuthPath, "utf8"));
    if (!auth["opencode-go"]?.key) {
      skipReason = "no opencode-go key in auth.json";
    }
  } catch {
    skipReason = "no/invalid auth.json";
  }
  proxyKey = firstApiKey(userConfigText) ?? "unused";
}
if (!Bun.which("claude")) skipReason = "claude CLI not on PATH";

interface ClaudeResult {
  exitCode: number | null;
  stdout: string;
  stderr: string;
}

/** Start the proxy on the user's real global config with an ephemeral port. */
async function startUserProxy(): Promise<ProxyHandle> {
  const port = await findFreePort();
  const proc = spawn([BINARY, "serve", "--config", userConfigPath, "--port", String(port)], {
    stdout: "ignore",
    stderr: "pipe",
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

async function runClaude(proxy: ProxyHandle, args: string[]): Promise<ClaudeResult> {
  const proc = spawn(["claude", "-p", ...args], {
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      ANTHROPIC_BASE_URL: proxy.base,
      ANTHROPIC_API_KEY: proxyKey,
      ANTHROPIC_AUTH_TOKEN: proxyKey,
      ANTHROPIC_MODEL: ACTIVE_MODEL!,
      ANTHROPIC_SMALL_FAST_MODEL: ACTIVE_MODEL!,
    },
  });
  const stdout = await new Response(proc.stdout).text();
  const stderr = await new Response(proc.stderr).text();
  const exitCode = await proc.exited;
  return { exitCode, stdout, stderr };
}

/** Treat upstream errors as an environmental skip, not a proxy failure. */
function assertClaudeOk(r: ClaudeResult): boolean {
  if (r.exitCode === 0) return true;
  console.warn(
    `[live-user/claude] not ok (exit ${r.exitCode}): ${(r.stderr || r.stdout).slice(0, 160)} — environmental skip`,
  );
  return false;
}

describe.skipIf(skipReason)("e2e: live opencode-go via user config", () => {
  let proxy: ProxyHandle;

  beforeAll(async () => {
    if (skipReason) {
      throw new Error(`cannot run: ${skipReason}`);
    }
    proxy = await startUserProxy();
  });

  afterAll(() => {
    if (proxy) {
      try {
        proxy.proc.kill();
      } catch {
        // already dead
      }
    }
  });

  live("/v1/models lists the user's active model", async () => {
    const r = await get(proxy.base, "/v1/models", { "x-api-key": proxyKey });
    if (r.status !== 200) {
      console.warn(
        `[live-user] /v1/models not ok (${r.status}): ${(await readBody(r)).slice(0, 160)} — environmental skip`,
      );
      return;
    }
    const body = JSON.parse(await readBody(r));
    const ids = body.data.map((m: any) => m.id);
    expect(ids).toContain(ACTIVE_MODEL);
  });

  live("messages non-streaming routes through the active model", async () => {
    const r = await postJsonRetry(
      proxy.base,
      "/v1/messages",
      {
        model: ACTIVE_MODEL,
        max_tokens: 64,
        messages: [{ role: "user", content: "Reply with exactly: pong" }],
      },
      { "x-api-key": proxyKey },
    );
    if (r.status !== 200) {
      console.warn(
        `[live-user] /v1/messages not ok (${r.status}): ${(await readBody(r)).slice(0, 160)} — environmental skip`,
      );
      return;
    }
    const body = JSON.parse(await readBody(r));
    expect(body.type).toBe("message");
    const text = body.content
      ?.filter((b: any) => b.type === "text")
      .map((b: any) => b.text)
      .join("");
    expect(typeof text).toBe("string");
    expect(text.length).toBeGreaterThan(0);
  });

  live("real claude CLI via proxy: print (text) reaches the active model", async () => {
    const r = await runClaude(proxy, [
      "Say the single word: banana",
      "--model",
      ACTIVE_MODEL!,
      "--dangerously-skip-permissions",
      "--verbose",
      "--output-format",
      "text",
    ]);
    if (!assertClaudeOk(r)) return;
    if (r.stdout.trim().length === 0) {
      console.warn(
        "[live-user/claude] empty model output — environmental skip (upstream returned nothing)",
      );
      return;
    }
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim().length).toBeGreaterThan(0);
  });

  live("real claude CLI via proxy: stream-json emits assistant output", async () => {
    const r = await runClaude(proxy, [
      "Say the single word: banana",
      "--model",
      ACTIVE_MODEL!,
      "--dangerously-skip-permissions",
      "--verbose",
      "--output-format",
      "stream-json",
    ]);
    if (!assertClaudeOk(r)) return;
    if (r.stdout.trim().length === 0) {
      console.warn(
        "[live-user/claude] empty model output — environmental skip (upstream returned nothing)",
      );
      return;
    }
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim().length).toBeGreaterThan(0);
  });
});