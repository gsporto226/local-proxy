import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import {
  authStorePath,
  eventNames,
  frameData,
  get,
  isolatedConfigDir,
  parseSse,
  postJson,
  readBody,
  runCli,
  startProxy,
  stopProxy,
  writeConfig,
  type ProxyHandle,
} from "./helpers";
import { mockConfig, startMockUpstream, type CacheUsage, type MockUpstream } from "./mock-upstream";

// Providers get their API keys only from the credential store. These tests seed
// the legacy `auth.json` on purpose: the proxy migrates it to the encrypted
// store (`accounts.db`) on first read, so every scenario also exercises the
// migration path. This suite runs against a throwaway config dir
// (`LOCAL_PROXY_CONFIG_DIR`) so the user's real auth store is never read,
// written, or restored.
const { dir: configDir, env: isolatedEnv } = isolatedConfigDir();
const authPath = authStorePath(configDir);

function seedAuth(entries: Record<string, unknown>): void {
  if (!existsSync(authPath)) writeFileSync(authPath, "{}");
  const auth = JSON.parse(readFileSync(authPath, "utf8"));
  Object.assign(auth, entries);
  writeFileSync(authPath, JSON.stringify(auth, null, 2));
}

beforeAll(() => {
  const auth: Record<string, unknown> = {};
  for (const p of ["mock_openai", "mock_anthropic", "mock_responses"]) {
    auth[p] = { type: "api", key: "test-key" };
  }
  writeFileSync(authPath, JSON.stringify(auth, null, 2));
});

/**
 * Start a mock upstream plus a proxy whose active model routes to a specific
 * provider. A client-sent model wins and routes via the defined routes; the
 * configured `activeModel` is only the fallback used when a client sends none.
 */
async function startScenario(
  activeModel: string,
  opts: { apiKeys?: string[]; autoModel?: string; env?: Record<string, string> } = {},
) {
  const { env, ...configOpts } = opts;
  const mock = await startMockUpstream();
  const proxy = await startProxy(
    mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel, ...configOpts }),
    undefined,
    { ...isolatedEnv, ...env },
  );
  return { mock, proxy };
}

function stopScenario(handle: { mock: MockUpstream; proxy: ProxyHandle }): void {
  stopProxy(handle.proxy);
  handle.mock.stop();
}

function cacheRecord(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("expected a JSON object in cache test");
  }
  return Object.fromEntries(Object.entries(value));
}

function cacheRecords(value: unknown): Record<string, unknown>[] {
  if (!Array.isArray(value)) throw new Error("expected a JSON array in cache test");
  return value.map(cacheRecord);
}

async function cacheJson(response: Response): Promise<Record<string, unknown>> {
  if (response.status !== 200) throw new Error(`cache test HTTP ${response.status}: ${await response.text()}`);
  expect(response.status).toBe(200);
  const value: unknown = await response.json();
  return cacheRecord(value);
}

async function startCacheScenario(opts: { cacheUsage?: CacheUsage[]; simulateCache?: boolean; policy?: string } = {}) {
  const { dir, env } = isolatedConfigDir();
  writeFileSync(authStorePath(dir), JSON.stringify({
    mock_openai: { type: "api", key: "test-openai" },
    mock_anthropic: { type: "api", key: "test-anthropic" },
    mock_responses: { type: "api", key: "test-responses" },
  }));
  const mock = await startMockUpstream(opts);
  let config = mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: opts.policy ? "gpt-via-anthropic" : "claude-via-responses" });
  if (opts.policy) config = config.replace("    format: anthropic", `    format: anthropic\n    prompt_cache: ${opts.policy}`);
  const configPath = writeConfig(config);
  try {
    const proxy = await startProxy(config, undefined, env, configPath);
    return { mock, proxy, env, configPath };
  } catch (error) {
    mock.stop();
    throw error;
  }
}

async function cacheStats(proxy: ProxyHandle, count: number, session?: string): Promise<Record<string, unknown>> {
  const path = `/admin/stats?since=all${session ? `&session_id=${encodeURIComponent(session)}` : ""}`;
  for (let attempt = 0; attempt < 40; attempt++) {
    const stats = await cacheJson(await get(proxy.base, path));
    if (cacheRecord(stats.summary).requests === count) return stats;
    await Bun.sleep(50);
  }
  throw new Error(`cache stats did not record ${count} requests`);
}

describe("e2e: prompt cache request, response and persisted statistics", () => {
  test("cache-check generates separate repeatable arms and reports actual mock reuse", async () => {
    const handle = await startCacheScenario({ simulateCache: true });
    try {
      const args = ["--config", handle.configPath, "cache-check", "--model", "mock_responses/gpt-6.1-sol-medium", "--runs", "2", "--prefix-tokens", "1024", "--json"];
      const dryRun = await runCli([...args, "--dry-run"], handle.env);
      if (dryRun.exit !== 0) throw new Error(dryRun.output);
      expect(handle.mock.requests()).toHaveLength(0);
      const live = await runCli([...args, "--confirm-live"], handle.env);
      if (live.exit !== 0) throw new Error(live.output);
      const report = cacheRecord(JSON.parse(live.stdout));
      for (const arm of [cacheRecord(report.direct), cacheRecord(report.proxied)]) {
        expect(arm.requests).toBe(2);
        expect(arm.successful_requests).toBe(2);
        expect(arm.measured_requests).toBe(2);
        expect(arm.hits).toBe(1);
        expect(arm.cache_read_tokens).toBe(800);
        expect(arm.cache_read_percent).toBe(40);
        expect(arm.verdict).toBe("reuse");
      }
      const captured = handle.mock.requests();
      expect(captured).toHaveLength(4);
      expect(captured[0].body).toEqual(captured[1].body);
      expect(captured[2].body).toEqual(captured[3].body);
      expect(captured[0].body.prompt_cache_key).not.toBe(captured[2].body.prompt_cache_key);
      expect(captured.every((request) => request.body.stream === true && request.body.store === false)).toBe(true);
    } finally { stopScenario(handle); }
  }, 30000);

  test("Anthropic markers and TTL survive, with cold JSON then warm SSE usage", async () => {
    const handle = await startCacheScenario({ simulateCache: true });
    try {
      const body = {
        model: "gpt-via-anthropic", max_tokens: 20,
        system: [{ type: "text", text: "Stable instructions", cache_control: { type: "ephemeral", ttl: "1h" } }],
        tools: [{ name: "lookup", description: "Stable tool", input_schema: { type: "object", properties: {} }, cache_control: { type: "ephemeral" } }],
        messages: [{ role: "user", content: [{ type: "text", text: "Stable question", cache_control: { type: "ephemeral", ttl: "5m" } }] }],
      };
      const headers = { "x-claude-code-session-id": "anthropic-cache" };
      const cold = await cacheJson(await postJson(handle.proxy.base, "/v1/messages", body, headers));
      expect(cacheRecord(cold.usage).cache_read_input_tokens).toBe(0);
      expect(cacheRecord(cold.usage).cache_creation_input_tokens).toBe(800);
      const warm = await postJson(handle.proxy.base, "/v1/messages", { ...body, stream: true }, headers);
      expect(warm.status).toBe(200);
      const start = cacheRecord(JSON.parse(frameData(parseSse(await readBody(warm)), "message_start")[0]));
      expect(cacheRecord(cacheRecord(start.message).usage).cache_read_input_tokens).toBe(800);
      for (const request of handle.mock.requests()) {
        expect(request.body.system).toEqual(body.system);
        expect(request.body.tools).toEqual(body.tools);
        expect(request.body.messages).toEqual(body.messages);
      }
      const stats = await cacheStats(handle.proxy, 2);
      const cache = cacheRecord(cacheRecord(stats.summary).cache);
      expect(cache.read_tokens).toBe(800);
      expect(cache.write_tokens).toBe(800);
      expect(cache.reported_input_tokens).toBe(2000);
      expect(cache.read_percent).toBe(40);
    } finally { stopScenario(handle); }
  }, 30000);

  test("Responses uses the session cache key, preserves explicit keys and reports changed prefixes", async () => {
    const handle = await startCacheScenario({ simulateCache: true });
    try {
      const body = { model: "claude-via-responses", max_tokens: 20,
        system: "Stable instructions", messages: [{ role: "user", content: "Question" }] };
      const headers = { "x-claude-code-session-id": "responses-cache" };
      const reads: unknown[] = [];
      for (const system of [body.system, body.system, "Changed instructions"]) {
        const response = await cacheJson(await postJson(handle.proxy.base, "/v1/messages", { ...body, system }, headers));
        reads.push(cacheRecord(response.usage).cache_read_input_tokens);
        expect(handle.mock.lastResponsesBody().prompt_cache_key).toBe("responses-cache");
      }
      expect(reads).toEqual([0, 800, 0]);
      const stats = await cacheStats(handle.proxy, 3, "responses-cache");
      expect(cacheRecords(stats.recent)[0].cache_diagnostics).toContain("stable_prefix_changed");
      await cacheJson(await postJson(handle.proxy.base, "/v1/responses", {
        model: "resp-native", input: "Question", prompt_cache_key: "explicit-cache-key",
      }, headers));
      expect(handle.mock.lastResponsesBody().prompt_cache_key).toBe("explicit-cache-key");
      await cacheJson(await postJson(handle.proxy.base, "/v1/responses", { model: "resp-native", input: "Question" }));
      expect(handle.mock.lastResponsesBody().prompt_cache_key).toBeUndefined();
    } finally { stopScenario(handle); }
  }, 30000);

  test("missing, zero and positive cache telemetry remain distinct and token percentages are weighted", async () => {
    const handle = await startCacheScenario({ cacheUsage: [
      { input: 9000, read: null, write: 0 },
      { input: 100, read: 0, write: 0 },
      { input: 1000, read: 900, write: 0 },
      { input: 200, read: 100, write: 0 },
    ] });
    try {
      for (let index = 0; index < 4; index++) {
        const response = await postJson(handle.proxy.base, "/v1/chat/completions", {
          model: "claude-via-openai", messages: [{ role: "user", content: "Cache statistics" }], stream: index === 2,
        }, { "x-claude-code-session-id": index === 3 ? "other-session" : "weighted-session" });
        expect(response.status).toBe(200);
        await readBody(response);
      }
      const stats = await cacheStats(handle.proxy, 4);
      const cache = cacheRecord(cacheRecord(stats.summary).cache);
      expect(cache.reported_requests).toBe(3);
      expect(cache.hit_requests).toBe(2);
      expect(cache.read_tokens).toBe(1000);
      expect(cache.reported_input_tokens).toBe(1300);
      expect(cache.uncached_tokens).toBe(300);
      expect(cache.read_percent).toBeCloseTo(1000 / 1300 * 100);
      const recent = cacheRecords(stats.recent);
      expect(recent.map((row) => row.cache_read_tokens)).toEqual([100, 900, 0, null]);
      expect(recent[3].cache_diagnostics).toContain("telemetry_unavailable");
      const session = await cacheStats(handle.proxy, 3, "weighted-session");
      expect(cacheRecord(cacheRecord(session.summary).cache).read_percent).toBeCloseTo(900 / 1100 * 100);
      expect(cacheRecord(cacheRecords(stats.accounts)[0].cache).read_tokens).toBe(1000);
      // A new process reads the same persisted report, including missing telemetry.
      handle.proxy.proc.kill();
      await handle.proxy.proc.exited;
      const restarted = await startProxy("", undefined, handle.env, handle.configPath);
      handle.proxy = restarted;
      expect(cacheRecord(cacheRecord((await cacheStats(restarted, 4)).summary).cache)).toEqual(cache);
    } finally { stopScenario(handle); }
  }, 30000);

  test("cache reads are isolated by account and session", async () => {
    const handle = await startCacheScenario({ simulateCache: true });
    try {
      for (const alias of ["work", "personal"]) {
        const result = await runCli(["--config", handle.configPath, "connect", "mock_responses", "--account", alias, `key-${alias}`], handle.env);
        if (result.exit !== 0) throw new Error(result.output);
        expect(result.exit).toBe(0);
      }
      const body = { model: "resp-native", input: "Stable prefix", prompt_cache_key: "shared-key" };
      const cases = [["work", "work-session"], ["work", "work-session"], ["personal", "personal-session"]];
      const reads: unknown[] = [];
      for (const [account, session] of cases) {
        const response = await cacheJson(await postJson(handle.proxy.base, "/v1/responses", body, {
          "x-local-proxy-account": account, "x-claude-code-session-id": session,
        }));
        reads.push(cacheRecord(cacheRecord(response.usage).input_tokens_details).cached_tokens);
      }
      expect(reads).toEqual([0, 800, 0]);
      const stats = await cacheStats(handle.proxy, 3);
      for (const account of cacheRecords(stats.accounts)) {
        expect(cacheRecord(account.cache).read_tokens).toBe(account.alias === "work" ? 800 : 0);
      }
      const personal = await cacheStats(handle.proxy, 1, "personal-session");
      expect(cacheRecord(cacheRecord(personal.summary).cache).read_tokens).toBe(0);
    } finally { stopScenario(handle); }
  }, 30000);

  test("opt-in Anthropic policy marks translated stable content and preserve mode leaves it unmarked", async () => {
    for (const policy of ["preserve", "anthropic-5m", "anthropic-1h"]) {
      const handle = await startCacheScenario({ policy });
      try {
        await cacheJson(await postJson(handle.proxy.base, "/v1/chat/completions", {
          model: "gpt-via-anthropic",
          messages: [{ role: "system", content: "Keep this instruction" }, { role: "user", content: "Keep this question" }],
          tools: [{ type: "function", function: { name: "lookup", description: "Keep this tool", parameters: { type: "object", properties: {} } } }],
        }));
        const request = handle.mock.requests()[0].body;
        expect(JSON.stringify(request)).toContain("Keep this instruction");
        expect(JSON.stringify(request)).toContain("Keep this question");
        expect(JSON.stringify(request)).toContain("Keep this tool");
        if (policy === "preserve") expect(JSON.stringify(request)).not.toContain("cache_control");
        else {
          const system = cacheRecords(request.system);
          expect(system[system.length - 1].cache_control).toEqual({ type: "ephemeral", ttl: policy === "anthropic-1h" ? "1h" : "5m" });
          expect(cacheRecords(request.tools)[0].cache_control).toBeDefined();
        }
        const marked = {
          model: "gpt-via-anthropic", max_tokens: 20, system: "Preserve the native string",
          messages: [{ role: "user", content: [{ type: "text", text: "Client chose the breakpoint", cache_control: { type: "ephemeral", ttl: "1h" } }] }],
        };
        await cacheJson(await postJson(handle.proxy.base, "/v1/messages", marked));
        const native = handle.mock.requests()[1].body;
        expect(native.system).toBe(marked.system);
        expect(native.messages).toEqual(marked.messages);
      } finally { stopScenario(handle); }
    }
  }, 60000);
});

describe("e2e: proxy against a mock upstream (openai upstream)", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("claude-via-openai");
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("health", async () => {
    const r = await get(proxy.base, "/health");
    expect(r.status).toBe(200);
    expect(await readBody(r)).toBe("ok");
  });

  test("/v1/models returns OpenAI shape by default", async () => {
    const r = await get(proxy.base, "/v1/models");
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.object).toBe("list");
    const ids = body.data.map((m: any) => m.id);
    expect(ids).toContain("claude-via-openai");
    expect(ids).toContain("gpt-via-anthropic");
    expect(ids).toContain("err");
  });

  test("/v1/models returns Anthropic shape with anthropic-version", async () => {
    const r = await get(proxy.base, "/v1/models", { "anthropic-version": "2023-06-01" });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    const ids = body.data.map((m: any) => m.id);
    expect(body.data[0].type).toBe("model");
    expect(ids).toContain("claude-via-openai");
    expect(body.data.find((m: { id: string }) => m.id === "claude-via-openai").display_name)
      .toBe("mock_openai/claude-via-openai");
  });

  test("messages streaming via openai upstream -> Anthropic events", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-openai",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type") ?? "").toContain("text/event-stream");
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames)).toEqual([
      "message_start",
      "content_block_start",
      "content_block_delta",
      "content_block_delta",
      "content_block_stop",
      "message_delta",
      "message_stop",
    ]);
    const deltas = frameData(frames, "content_block_delta").map((d) =>
      JSON.parse(d).delta.text,
    );
    expect(deltas).toEqual(["Hel", "lo"]);
    const md = JSON.parse(frameData(frames, "message_delta")[0]);
    expect(md.delta.stop_reason).toBe("end_turn");
    expect(md.usage.input_tokens).toBe(3);
    expect(md.usage.output_tokens).toBe(2);
  });

  test("responses streaming via openai upstream emits full sequence", async () => {
    const r = await postJson(proxy.base, "/v1/responses", {
      model: "claude-via-openai",
      input: [{ role: "user", content: [{ type: "input_text", text: "hi" }] }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames)).toEqual([
      "response.created",
      "response.output_item.added",
      "response.content_part.added",
      "response.output_text.delta",
      "response.output_text.delta",
      "response.output_text.done",
      "response.output_item.done",
      "response.completed",
    ]);
    const completed = JSON.parse(frameData(frames, "response.completed")[0]);
    expect(completed.response.status).toBe("completed");
    expect(completed.response.output[0].content[0].text).toBe("Hello");
  });

  test("messages non-streaming translates openai response to anthropic", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-openai",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.type).toBe("message");
    expect(body.content[0].text).toBe("hi");
    expect(body.stop_reason).toBe("end_turn");
  });

  test("count_tokens returns an estimate", async () => {
    const r = await postJson(proxy.base, "/v1/messages/count_tokens", {
      model: "ignored",
      messages: [{ role: "user", content: "hello world how are you" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.input_tokens).toBeGreaterThan(0);
  });
});

describe("e2e: proxy against a mock upstream (anthropic upstream)", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("gpt-via-anthropic");
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("chat completions streaming via anthropic upstream -> OpenAI chunks", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "gpt-via-anthropic",
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    const chunks = frames
      .filter((f) => !f.event && f.data !== "[DONE]")
      .map((f) => JSON.parse(f.data));
    expect(chunks[0].choices[0].delta.role).toBe("assistant");
    const contents = chunks
      .map((c: any) => c.choices[0].delta.content)
      .filter((c: any) => typeof c === "string" && c.length > 0);
    expect(contents).toEqual(["oi"]);
    const finish = chunks.find((c: any) => c.choices[0].finish_reason);
    expect(finish.choices[0].finish_reason).toBe("stop");
    expect(finish.usage.completion_tokens).toBe(2);
    expect(frames.some((f) => !f.event && f.data === "[DONE]")).toBe(true);
  });

  test("responses streaming via anthropic upstream emits full sequence", async () => {
    const r = await postJson(proxy.base, "/v1/responses", {
      model: "gpt-via-anthropic",
      input: [{ role: "user", content: [{ type: "input_text", text: "hi" }] }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames)).toEqual([
      "response.created",
      "response.output_item.added",
      "response.content_part.added",
      "response.output_text.delta",
      "response.output_text.done",
      "response.output_item.done",
      "response.completed",
    ]);
  });

  test("messages passthrough to anthropic upstream keeps raw events", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "gpt-via-anthropic",
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const text = await readBody(r);
    expect(text).toContain("message_start");
    expect(text).toContain("oi");
    expect(text).toContain("message_stop");
  });

  test("chat completions non-streaming translates anthropic response to openai", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "gpt-via-anthropic",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.choices[0].message.content).toBe("hi");
    expect(body.choices[0].finish_reason).toBe("stop");
  });
});

describe("e2e: proxy against a mock upstream (responses upstream)", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("claude-via-responses");
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("messages streaming via responses upstream -> Anthropic events", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type") ?? "").toContain("text/event-stream");
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames)).toEqual([
      "message_start",
      "content_block_start",
      "content_block_delta",
      "content_block_delta",
      "content_block_stop",
      "message_delta",
      "message_stop",
    ]);
    const deltas = frameData(frames, "content_block_delta").map(
      (d) => JSON.parse(d).delta.text,
    );
    expect(deltas).toEqual(["Ol", "a"]);
    const md = JSON.parse(frameData(frames, "message_delta")[0]);
    expect(md.delta.stop_reason).toBe("end_turn");
    expect(md.usage.output_tokens).toBe(2);
  });

  test("messages non-streaming via responses upstream translates the response", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.type).toBe("message");
    expect(body.content[0].text).toBe("Ola");
    expect(body.usage.output_tokens).toBe(2);
  });

  test("chat completions streaming via responses upstream -> OpenAI chunks", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "gpt-via-responses",
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    const chunks = frames
      .filter((f) => !f.event && f.data !== "[DONE]")
      .map((f) => JSON.parse(f.data));
    expect(chunks[0].choices[0].delta.role).toBe("assistant");
    const contents = chunks
      .map((c: any) => c.choices[0].delta.content)
      .filter((c: any) => typeof c === "string" && c.length > 0);
    expect(contents).toEqual(["Ol", "a"]);
    const finish = chunks.find((c: any) => c.choices[0].finish_reason);
    expect(finish.choices[0].finish_reason).toBe("stop");
    expect(frames.some((f) => !f.event && f.data === "[DONE]")).toBe(true);
  });

  test("chat completions non-streaming via responses upstream translates the response", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "gpt-via-responses",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.object).toBe("chat.completion");
    expect(body.choices[0].message.content).toBe("Ola");
    expect(body.choices[0].finish_reason).toBe("stop");
  });

  test("responses request to a stream-only upstream is aggregated", async () => {
    // The proxy always streams upstream for this format (the real backend
    // rejects `stream: false`), so a non-streaming client is served by
    // reassembling the SSE stream: "Ol" + "a", not the mock's non-stream body.
    const r = await postJson(proxy.base, "/v1/responses", {
      model: "resp-native",
      input: [{ role: "user", content: [{ type: "input_text", text: "hi" }] }],
    });
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type") ?? "").toContain("application/json");
    const body = JSON.parse(await readBody(r));
    expect(body.object).toBe("response");
    expect(body.status).toBe("completed");
    expect(body.output[0].content[0].text).toBe("Ola");
    expect(body.usage.output_tokens).toBe(2);
  });

  test("responses streaming passes raw Responses events through", async () => {
    const r = await postJson(proxy.base, "/v1/responses", {
      model: "resp-native",
      input: [{ role: "user", content: [{ type: "input_text", text: "hi" }] }],
      stream: true,
    });
    expect(r.status).toBe(200);
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames)).toContain("response.output_text.delta");
    expect(eventNames(frames)).toContain("response.completed");
  });

  test("the responses provider appears as a connected model", async () => {
    const r = await get(proxy.base, "/v1/models");
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    const ids = body.data.map((m: any) => m.id);
    expect(ids).toContain("claude-via-responses");
    expect(ids).toContain("resp-native");
  });

  test("a client reasoning effort becomes a reasoning object upstream", async () => {
    // `reasoning_effort` is the chat spelling; forwarding it raw is a hard
    // error on the real backend ("Unsupported parameter: reasoning_effort"), so
    // it must arrive as `reasoning.effort`.
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-responses",
      max_tokens: 10,
      thinking: { type: "enabled", effort: "high" },
      // Claude Code sends these on every request; the Codex backend 400s on
      // both ("Unsupported parameter: metadata", "Invalid type for
      // context_management"). The IR must not forward them.
      metadata: { user_id: "u" },
      context_management: { edits: [] },
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const sent = mock.lastResponsesBody();
    expect(sent.reasoning?.effort).toBe("high");
    expect(sent.reasoning_effort).toBeUndefined();
    expect(sent.metadata).toBeUndefined();
    expect(sent.context_management).toBeUndefined();
    // the proxy still forces streaming for this backend
    expect(sent.stream).toBe(true);
  });
});

describe("e2e: oauth-backed responses provider", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    // Seed an expired OAuth entry so the proxy must refresh on first use.
    seedAuth({
      mock_oauth: {
        type: "oauth",
        access: "stale-token",
        refresh: "refresh-1",
        account_id: "acct-e2e",
        expires: Date.now() - 3_600_000,
      },
    });

    // Start the mock first: its port is the OAuth token endpoint the proxy
    // must call to refresh.
    mock = await startMockUpstream();
    proxy = await startProxy(
      mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: "oauth-via-responses" }),
      undefined,
      isolatedEnv,
    );
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("refreshes the token and sends the oauth headers upstream", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "oauth-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.content[0].text).toBe("Ola");

    expect(mock.tokenHits()).toBe(1);
    const headers = mock.lastResponsesHeaders();
    expect(headers.authorization).toBe("Bearer refreshed-token");
    expect(headers["chatgpt-account-id"]).toBe("acct-e2e");
  });

  test("migrates the legacy store and keeps a backup", () => {
    expect(existsSync(authPath)).toBe(false);
    expect(existsSync(join(configDir, "auth.json.migrated"))).toBe(true);
    expect(existsSync(join(configDir, "accounts.db"))).toBe(true);
  });

  test("a fresh token is reused without another refresh", async () => {
    const before = mock.tokenHits();
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "oauth-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "again" }],
    });
    expect(r.status).toBe(200);
    expect(mock.tokenHits()).toBe(before);
  });

  test("the refreshed token survives a proxy restart", async () => {
    stopProxy(proxy);
    proxy = await startProxy(
      mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: "oauth-via-responses" }),
      undefined,
      isolatedEnv,
    );
    const before = mock.tokenHits();
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "oauth-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "restart" }],
    });
    expect(r.status).toBe(200);
    // No refresh on the new process: the fresh token came from the store.
    expect(mock.tokenHits()).toBe(before);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer refreshed-token");
  });
});

describe("e2e: upstream error is reformatted to client shape", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("err");
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("anthropic client sees the upstream error reformatted", async () => {
    const a = await postJson(proxy.base, "/v1/messages", {
      model: "err",
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(a.status).toBe(401);
    const ab = JSON.parse(await readBody(a));
    expect(ab.type).toBe("error");
    expect(ab.error.message).toBe("bad key");
  });

  test("openai client sees the upstream error reformatted", async () => {
    const o = await postJson(proxy.base, "/v1/chat/completions", {
      model: "err",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(o.status).toBe(401);
    const ob = JSON.parse(await readBody(o));
    expect(ob.error.message).toBe("bad key");
  });
});

describe("e2e: auth with configured api_keys", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("gpt-via-anthropic", { apiKeys: ["sk-proxy"] });
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("rejects requests without a key", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "ignored",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(401);
  });

  test("accepts a valid X-API-Key", async () => {
    const r = await postJson(
      proxy.base,
      "/v1/chat/completions",
      { model: "gpt-via-anthropic", messages: [{ role: "user", content: "hi" }] },
      { "x-api-key": "sk-proxy" },
    );
    expect(r.status).toBe(200);
  });

  test("accepts a valid Bearer token", async () => {
    const r = await postJson(
      proxy.base,
      "/v1/chat/completions",
      { model: "gpt-via-anthropic", messages: [{ role: "user", content: "hi" }] },
      { authorization: "Bearer sk-proxy" },
    );
    expect(r.status).toBe(200);
  });
});

describe("e2e: client-provided model takes precedence", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    // Active model points at the anthropic upstream; the client will ask for a
    // different route to prove the client wins.
    const s = await startScenario("gpt-via-anthropic");
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("can route to a different route than the active model", async () => {
    // Active model = gpt-via-anthropic, but client asks for claude-via-openai.
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-openai",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    // openai upstream -> anthropic events (not the anthropic passthrough).
    const frames = parseSse(await readBody(r));
    expect(eventNames(frames).slice(0, 3)).toEqual([
      "message_start",
      "content_block_start",
      "content_block_delta",
    ]);
  });

  test("falls back to the active model when client sends no model", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
      stream: true,
    });
    expect(r.status).toBe(200);
    // Active model gpt-via-anthropic -> anthropic passthrough raw events.
    const text = await readBody(r);
    expect(text).toContain("message_start");
    expect(text).toContain("oi");
  });

  test("unknown client model is rejected with proxy: unknown model", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "no-such-route",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(404);
    const body = JSON.parse(await readBody(r));
    expect(body.error.message).toContain("proxy: unknown model no-such-route");
  });
});

describe("e2e: per-provider auto model", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    const s = await startScenario("", { autoModel: "gpt-4o" });
    mock = s.mock;
    proxy = s.proxy;
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("/v1/models lists the provider/auto alias", async () => {
    const r = await get(proxy.base, "/v1/models");
    expect(r.status).toBe(200);
    const ids = JSON.parse(await readBody(r)).data.map((m: any) => m.id);
    expect(ids).toContain("mock_openai/auto");
    expect(ids).not.toContain("mock_anthropic/auto");
  });

  test("provider/auto resolves to the provider's auto_model", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "mock_openai/auto",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.choices[0].message.content).toBe("hi");
  });

  test("bare auto resolves to a configured auto_model", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "auto",
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.content[0].text).toBe("hi");
  });

  test("provider/auto without auto_model is a clear 404", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "mock_anthropic/auto",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(404);
    const body = JSON.parse(await readBody(r));
    expect(body.error.message).toContain("no auto_model configured");
  });

  test("bare auto with no configured provider is a clear 404", async () => {
    const s = await startScenario("");
    try {
      const r = await postJson(s.proxy.base, "/v1/chat/completions", {
        model: "auto",
        messages: [{ role: "user", content: "hi" }],
      });
      expect(r.status).toBe(404);
      const body = JSON.parse(await readBody(r));
      expect(body.error.message).toContain("no provider has an auto_model configured");
    } finally {
      stopScenario(s);
    }
  });
});

describe("e2e: multiple accounts per provider", () => {
  let mock: MockUpstream;
  let proxy: ProxyHandle;
  const { env } = isolatedConfigDir();
  let cfgPath: string;

  beforeAll(async () => {
    mock = await startMockUpstream();
    const cfg = mockConfig(`http://127.0.0.1:${mock.port}`, {
      activeModel: "claude-via-responses",
    });
    cfgPath = writeConfig(cfg);
    // Two named accounts on the same provider, written through the real CLI.
    for (const [alias, key] of [
      ["work", "key-work"],
      ["personal", "key-personal"],
    ]) {
      const res = await runCli(
        ["--config", cfgPath, "connect", "mock_responses", "--account", alias, key],
        env,
      );
      expect(res.exit).toBe(0);
    }
    proxy = await startProxy(cfg, undefined, env, cfgPath);
  });

  afterAll(() => stopScenario({ mock, proxy }));

  test("the first connected account is selected; header-less requests use it", async () => {
    const r = await postJson(proxy.base, "/v1/messages", {
      model: "claude-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer key-work");
  });

  test("an unknown alias is rejected", async () => {
    const r = await postJson(
      proxy.base,
      "/v1/messages",
      { model: "claude-via-responses", max_tokens: 10, messages: [{ role: "user", content: "hi" }] },
      { "x-local-proxy-account": "nope" },
    );
    expect(r.status).toBe(400);
    expect(await readBody(r)).toContain("unknown account 'nope'");
  });

  test("each alias sends its own credential upstream", async () => {
    const work = await postJson(
      proxy.base,
      "/v1/messages",
      { model: "claude-via-responses", max_tokens: 10, messages: [{ role: "user", content: "hi" }] },
      { "x-local-proxy-account": "work" },
    );
    expect(work.status).toBe(200);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer key-work");

    const personal = await postJson(
      proxy.base,
      "/v1/messages",
      { model: "claude-via-responses", max_tokens: 10, messages: [{ role: "user", content: "hi" }] },
      { "x-local-proxy-account": "personal" },
    );
    expect(personal.status).toBe(200);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer key-personal");
  });

  test("providers lists every alias with its kind", async () => {
    const res = await runCli(["--config", cfgPath, "providers"], env);
    expect(res.exit).toBe(0);
    expect(res.output).toContain("mock_responses");
    expect(res.output).toContain("personal (api)");
    expect(res.output).toContain("work (api)");
  });

  test("selecting an account makes header-less requests use it (hot reload)", async () => {
    const res = await runCli(["--config", cfgPath, "account", "mock_responses/work"], env);
    expect(res.exit).toBe(0);
    expect(res.output).toContain("work");

    // The running proxy applies the selection via hot reload; poll briefly.
    let status = 0;
    for (let i = 0; i < 30; i++) {
      const r = await postJson(proxy.base, "/v1/messages", {
        model: "claude-via-responses",
        max_tokens: 10,
        messages: [{ role: "user", content: "hi" }],
      });
      status = r.status;
      if (status === 200) break;
      await Bun.sleep(100);
    }
    expect(status).toBe(200);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer key-work");
  });

  test("the account listing marks the active alias", async () => {
    const res = await runCli(["--config", cfgPath, "account"], env);
    expect(res.exit).toBe(0);
    expect(res.output).toContain("mock_responses/work (api) [ativa]");
    expect(res.output).toContain("mock_responses/personal (api)");
  });

  test("a header still overrides the selected account", async () => {
    const r = await postJson(
      proxy.base,
      "/v1/messages",
      { model: "claude-via-responses", max_tokens: 10, messages: [{ role: "user", content: "hi" }] },
      { "x-local-proxy-account": "personal" },
    );
    expect(r.status).toBe(200);
    expect(mock.lastResponsesHeaders().authorization).toBe("Bearer key-personal");
  });

  test("switching the selection takes effect; there is no clear", async () => {
    let res = await runCli(["--config", cfgPath, "account", "mock_responses/personal"], env);
    expect(res.exit).toBe(0);
    let auth = "";
    for (let i = 0; i < 30; i++) {
      const r = await postJson(proxy.base, "/v1/messages", {
        model: "claude-via-responses",
        max_tokens: 10,
        messages: [{ role: "user", content: "hi" }],
      });
      if (r.status === 200) {
        auth = mock.lastResponsesHeaders().authorization ?? "";
        if (auth === "Bearer key-personal") break;
      }
      await Bun.sleep(100);
    }
    expect(auth).toBe("Bearer key-personal");

    res = await runCli(["--config", cfgPath, "account", "clear", "mock_responses"], env);
    expect(res.exit).not.toBe(0);
  });

  test("the selected account cannot be disconnected while others remain", async () => {
    const res = await runCli(
      ["--config", cfgPath, "disconnect", "mock_responses", "--account", "personal"],
      env,
    );
    expect(res.exit).not.toBe(0);
    expect(res.output).toContain("selecionada");
  });

  test("each session keeps the account it started with", async () => {
    const chat = { model: "claude-via-responses", max_tokens: 10, messages: [{ role: "user", content: "hi" }] };
    const cmd = (alias: string) => ({
      model: "claude-via-responses",
      max_tokens: 10,
      messages: [{ role: "user", content: `$proxy account mock_responses/${alias}` }],
    });
    const sid = (id: string) => ({ "x-claude-code-session-id": id });
    const authOf = async (headers: Record<string, string>) => {
      const r = await postJson(proxy.base, "/v1/messages", chat, headers);
      expect(r.status).toBe(200);
      return mock.lastResponsesHeaders().authorization;
    };

    // Session A switches to work through the proxy itself.
    let r = await postJson(proxy.base, "/v1/messages", cmd("work"), sid("sess-A"));
    expect(r.status).toBe(200);
    expect(await readBody(r)).toContain("work");
    expect(await authOf(sid("sess-A"))).toBe("Bearer key-work");

    // Session B switches to personal: the selection moves, A keeps work.
    r = await postJson(proxy.base, "/v1/messages", cmd("personal"), sid("sess-B"));
    expect(r.status).toBe(200);
    expect(await authOf(sid("sess-B"))).toBe("Bearer key-personal");
    expect(await authOf(sid("sess-A"))).toBe("Bearer key-work");

    // A new session starts from the current selection.
    expect(await authOf(sid("sess-C"))).toBe("Bearer key-personal");

    // A header overrides A for one request without moving its snapshot.
    expect(await authOf({ ...sid("sess-A"), "x-local-proxy-account": "personal" })).toBe("Bearer key-personal");
    expect(await authOf(sid("sess-A"))).toBe("Bearer key-work");

    // Disconnecting A's account moves A to the current selection.
    const res = await runCli(["--config", cfgPath, "disconnect", "mock_responses", "--account", "work"], env);
    expect(res.exit).toBe(0);
    let auth = "";
    for (let i = 0; i < 30; i++) {
      const q = await postJson(proxy.base, "/v1/messages", chat, sid("sess-A"));
      if (q.status === 200) {
        auth = mock.lastResponsesHeaders().authorization ?? "";
        if (auth === "Bearer key-personal") break;
      }
      await Bun.sleep(100);
    }
    expect(auth).toBe("Bearer key-personal");
  });
});

describe("e2e: legacy auth.json migration", () => {
  const { dir, env } = isolatedConfigDir();

  test("migrates legacy entries to default accounts and keeps a backup", async () => {
    const cfgPath = writeConfig(mockConfig("http://127.0.0.1:9", {}));
    writeFileSync(
      join(dir, "auth.json"),
      JSON.stringify({ mock_openai: { type: "api", key: "k" } }),
    );

    const res = await runCli(["--config", cfgPath, "providers"], env);
    expect(res.exit).toBe(0);
    expect(res.output).toContain("mock_openai");
    expect(res.output).toContain("default (api)");
    expect(existsSync(join(dir, "auth.json"))).toBe(false);
    expect(existsSync(join(dir, "auth.json.migrated"))).toBe(true);
    expect(existsSync(join(dir, "accounts.db"))).toBe(true);
  });
});
