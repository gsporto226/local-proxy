import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import {
  eventNames,
  findFreePort,
  frameData,
  framesJson,
  get,
  parseSse,
  postJson,
  readBody,
  startProxy,
  stopProxy,
  type ProxyHandle,
} from "./helpers";
import { mockConfig, startMockUpstream } from "./mock-upstream";

describe("e2e: proxy against a mock upstream", () => {
  let mock: Awaited<ReturnType<typeof startMockUpstream>>;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    mock = await startMockUpstream();
    proxy = await startProxy(mockConfig(`http://127.0.0.1:${mock.port}`));
  });

  afterAll(() => {
    stopProxy(proxy);
    mock.stop();
  });

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

  test("provider/model syntax routes without a route entry", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "mock_openai/gpt-anything",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.choices[0].message.content).toBe("hi");
  });

  test("upstream error is reformatted to client shape", async () => {
    // Anthropic client
    const a = await postJson(proxy.base, "/v1/messages", {
      model: "err",
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(a.status).toBe(401);
    const ab = JSON.parse(await readBody(a));
    expect(ab.type).toBe("error");
    expect(ab.error.message).toBe("bad key");

    // OpenAI client
    const o = await postJson(proxy.base, "/v1/chat/completions", {
      model: "err",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(o.status).toBe(401);
    const ob = JSON.parse(await readBody(o));
    expect(ob.error.message).toBe("bad key");
  });

  test("unknown model returns 404 in both formats", async () => {
    const o = await postJson(proxy.base, "/v1/chat/completions", {
      model: "nope",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(o.status).toBe(404);
    const ob = JSON.parse(await readBody(o));
    expect(ob.error.type).toBe("not_found_error");

    const a = await postJson(proxy.base, "/v1/messages", {
      model: "nope",
      max_tokens: 5,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(a.status).toBe(404);
    const ab = JSON.parse(await readBody(a));
    expect(ab.error.type).toBe("not_found_error");
  });

  test("count_tokens returns an estimate", async () => {
    const r = await postJson(proxy.base, "/v1/messages/count_tokens", {
      model: "claude-via-openai",
      messages: [{ role: "user", content: "hello world how are you" }],
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(await readBody(r));
    expect(body.input_tokens).toBeGreaterThan(0);
  });
});

describe("e2e: auth with configured api_keys", () => {
  let mock: Awaited<ReturnType<typeof startMockUpstream>>;
  let proxy: ProxyHandle;

  beforeAll(async () => {
    mock = await startMockUpstream();
    const port = await findFreePort();
    proxy = await startProxy(
      mockConfig(`http://127.0.0.1:${mock.port}`, { apiKeys: ["sk-proxy"] }),
      port,
    );
  });

  afterAll(() => {
    stopProxy(proxy);
    mock.stop();
  });

  test("rejects requests without a key", async () => {
    const r = await postJson(proxy.base, "/v1/chat/completions", {
      model: "gpt-via-anthropic",
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
