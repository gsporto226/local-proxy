import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import {
  eventNames,
  frameData,
  get,
  parseSse,
  postJson,
  readBody,
  startProxy,
  stopProxy,
  type ProxyHandle,
} from "./helpers";
import { mockConfig, startMockUpstream, type MockUpstream } from "./mock-upstream";

// Providers get their API keys only from the global auth store (auth.json), so
// seed the mock providers there for the duration of this suite, restoring the
// user's real auth file afterwards.
function globalConfigDir(): string {
  if (process.platform === "win32") {
    return join(process.env.APPDATA ?? "", "local-proxy", "config");
  }
  return join(process.env.HOME ?? process.env.USERPROFILE ?? "", ".config", "local-proxy");
}
const authPath = join(globalConfigDir(), "auth.json");
let authBackup: string | null = null;

beforeAll(() => {
  if (existsSync(authPath)) {
    authBackup = readFileSync(authPath, "utf8");
  }
  mkdirSync(globalConfigDir(), { recursive: true });
  const auth = existsSync(authPath) ? JSON.parse(readFileSync(authPath, "utf8")) : {};
  for (const p of ["mock_openai", "mock_anthropic"]) {
    auth[p] = { type: "api", key: "test-key" };
  }
  writeFileSync(authPath, JSON.stringify(auth, null, 2));
});

afterAll(() => {
  if (authBackup === null) {
    rmSync(authPath, { force: true });
  } else {
    writeFileSync(authPath, authBackup);
  }
});

/**
 * Start a mock upstream plus a proxy whose active model routes to a specific
 * provider. A client-sent model wins and routes via the defined routes; the
 * configured `activeModel` is only the fallback used when a client sends none.
 */
async function startScenario(activeModel: string, opts: { apiKeys?: string[] } = {}) {
  const mock = await startMockUpstream();
  const proxy = await startProxy(
    mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel, ...opts }),
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
