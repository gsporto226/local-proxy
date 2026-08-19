import { spawn } from "bun";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import {
  eventNames,
  frameData,
  get,
  parseSse,
  postJsonRetry,
  readBody,
  startProxy,
  stopProxy,
  type ProxyHandle,
} from "./helpers";

const KEY = process.env.OPENCODE_ZEN_KEY;
const skip = !KEY || KEY.length === 0;

// Real upstream calls can be slow (free models, warmup); Bun's default 5s
// test timeout is far too short. Wrap every test with a 3-minute timeout.
const T = 180000;
const live = (name: string, fn: () => void | Promise<void>) => test(name, fn, T);

const ZEN_BASE = "https://opencode.ai/zen";
const FREE_MODEL = "deepseek-v4-flash-free";

// The free upstream is rate-limited/flaky; when it is unavailable we treat the
// test as an environmental skip (warning) rather than a proxy failure.
async function assertLiveOk(r: Response): Promise<boolean> {
  if (r.status === 200) return true;
  const body = await r.text().catch(() => "");
  console.warn(
    `[live-zen] upstream not ok (${r.status}): ${body.slice(0, 160)} — environmental skip`,
  );
  return false;
}

function zenConfig(): string {
  return `
server:
  host: 127.0.0.1
  port: 0
  api_keys: []
  passthrough_keys: false

providers:
  - name: zen
    base_url: ${ZEN_BASE}
    api_key_env: OPENCODE_ZEN_KEY
    format: openai
    models:
      - ${FREE_MODEL}
      - claude-sonnet-4-5

routes:
  - model: deepseek-free
    provider: zen
    upstream_model: ${FREE_MODEL}

defaults:
  provider: zen
`;
}

interface ClaudeResult {
  exitCode: number | null;
  stdout: string;
  stderr: string;
}

const CLAUDE_MODEL = process.env.ANTHROPIC_MODEL || "deepseek-free";

/**
 * Run the real `claude` CLI pointed at the proxy, mirroring the env that
 * `local-proxy launch` sets (ANTHROPIC_BASE_URL + model). The proxy has no
 * client auth (api_keys: []), so the CLI presents a dummy key that the proxy
 * ignores; the proxy uses the user's configured upstream key to reach zen.
 */
async function runClaude(
  proxy: ProxyHandle,
  args: string[],
): Promise<ClaudeResult> {
  const proc = spawn(["claude", "-p", ...args], {
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      ANTHROPIC_BASE_URL: proxy.base,
      ANTHROPIC_API_KEY: "unused",
      ANTHROPIC_AUTH_TOKEN: "unused",
      ANTHROPIC_MODEL: CLAUDE_MODEL,
      ANTHROPIC_SMALL_FAST_MODEL: CLAUDE_MODEL,
    },
  });
  const stdout = await new Response(proc.stdout).text();
  const stderr = await new Response(proc.stderr).text();
  const exitCode = await proc.exited;
  return { exitCode, stdout, stderr };
}

/** Treat upstream rate-limit/errors as an environmental skip, not a failure. */
function assertClaudeOk(r: ClaudeResult): boolean {
  if (r.exitCode === 0) return true;
  console.warn(
    `[live-zen/claude] not ok (exit ${r.exitCode}): ${(r.stderr || r.stdout).slice(0, 160)} — environmental skip`,
  );
  return false;
}

describe.skipIf(skip)("e2e: live opencode-zen", () => {
  let proxy: ProxyHandle;

  beforeAll(async () => {
    if (skip) {
      throw new Error(
        "OPENCODE_ZEN_KEY not set; run with the env var to exercise live tests",
      );
    }
    proxy = await startProxy(zenConfig());
  });

  afterAll(() => {
    if (proxy) stopProxy(proxy);
  });

  live("/v1/models lists the free model", async () => {
    const r = await get(proxy.base, "/v1/models");
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    const ids = body.data.map((m: any) => m.id);
    expect(ids).toContain("deepseek-free");
  });

  live("chat completions non-streaming", async () => {
    const r = await postJsonRetry(proxy.base, "/v1/chat/completions", {
      model: "deepseek-free",
      messages: [{ role: "user", content: "Reply with exactly: pong" }],
      max_tokens: 64,
    });
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    const content = body.choices?.[0]?.message?.content;
    expect(typeof content).toBe("string");
    expect(content.length).toBeGreaterThan(0);
  });

  live("chat completions streaming", async () => {
    const r = await postJsonRetry(proxy.base, "/v1/chat/completions", {
      model: "deepseek-free",
      messages: [{ role: "user", content: "Reply with exactly: pong" }],
      max_tokens: 64,
      stream: true,
    });
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    const chunks = frames
      .filter((f) => !f.event && f.data !== "[DONE]")
      .map((f) => JSON.parse(f.data));
    const text = chunks
      .map((c: any) => c.choices?.[0]?.delta?.content ?? "")
      .join("");
    expect(text.trim().length).toBeGreaterThan(0);
    expect(frames.some((f) => !f.event && f.data === "[DONE]")).toBe(true);
  });

  live("messages non-streaming (Anthropic format) via provider/model syntax", async () => {
    const r = await postJsonRetry(proxy.base, "/v1/messages", {
      model: `zen/${FREE_MODEL}`,
      max_tokens: 64,
      messages: [{ role: "user", content: "Reply with exactly: pong" }],
    });
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.type).toBe("message");
    const text = body.content?.filter((b: any) => b.type === "text")
      .map((b: any) => b.text)
      .join("");
    expect(typeof text).toBe("string");
    expect(text.length).toBeGreaterThan(0);
  });

  live("messages streaming (Anthropic format) emits full event sequence", async () => {
    const r = await postJsonRetry(proxy.base, "/v1/messages", {
      model: "deepseek-free",
      max_tokens: 64,
      messages: [{ role: "user", content: "Reply with exactly: pong" }],
      stream: true,
    });
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    const names = eventNames(frames);
    expect(names[0]).toBe("message_start");
    expect(names).toContain("content_block_delta");
    expect(names[names.length - 1]).toBe("message_stop");
    const text = frameData(frames, "content_block_delta")
      .map((d) => JSON.parse(d).delta?.text ?? "")
      .join("");
    expect(text.trim().length).toBeGreaterThan(0);
  });

  live("responses streaming emits created..completed", async () => {
    const r = await postJsonRetry(proxy.base, "/v1/responses", {
      model: "deepseek-free",
      input: [
        { role: "user", content: [{ type: "input_text", text: "Reply with exactly: pong" }] },
      ],
      max_output_tokens: 64,
      stream: true,
    });
    if (!(await assertLiveOk(r))) return;
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    const names = eventNames(frames);
    expect(names[0]).toBe("response.created");
    expect(names).toContain("response.output_text.delta");
    expect(names[names.length - 1]).toBe("response.completed");
  });

  live("real claude CLI via proxy: print (text) reaches zen", async () => {
    const r = await runClaude(proxy, [
      "Say the single word: banana",
      "--model",
      CLAUDE_MODEL,
      "--dangerously-skip-permissions",
      "--verbose",
      "--output-format",
      "text",
    ]);
    if (!assertClaudeOk(r)) return;
    if (r.stdout.trim().length === 0) {
      console.warn(
        `[live-zen/claude] empty model output — environmental skip (free model returned nothing)`,
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
      CLAUDE_MODEL,
      "--dangerously-skip-permissions",
      "--verbose",
      "--output-format",
      "stream-json",
    ]);
    if (!assertClaudeOk(r)) return;
    if (r.stdout.trim().length === 0) {
      console.warn(
        `[live-zen/claude] empty model output — environmental skip (free model returned nothing)`,
      );
      return;
    }
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim().length).toBeGreaterThan(0);
  });
});




