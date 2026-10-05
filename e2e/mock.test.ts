import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readFileSync, writeFileSync } from "node:fs";

import {
  authStorePath,
  eventNames,
  frameData,
  get,
  isolatedConfigDir,
  parseSse,
  postJson,
  readBody,
  startProxy,
  stopProxy,
  type ProxyHandle,
} from "./helpers";
import { mockConfig, startMockUpstream, type MockUpstream } from "./mock-upstream";

// Providers get their API keys only from `auth.json`, so seed the mock providers
// there. This suite runs against a throwaway config dir (`LOCAL_PROXY_CONFIG_DIR`)
// so the user's real auth store is never read, written, or restored.
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
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const sent = mock.lastResponsesBody();
    expect(sent.reasoning?.effort).toBe("high");
    expect(sent.reasoning_effort).toBeUndefined();
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

  test("persists the refreshed token in auth.json", () => {
    const auth = JSON.parse(readFileSync(authPath, "utf8"));
    expect(auth.mock_oauth.access).toBe("refreshed-token");
    expect(auth.mock_oauth.refresh).toBe("refresh-2");
    expect(auth.mock_oauth.type).toBe("oauth");
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
