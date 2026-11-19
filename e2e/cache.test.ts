// Prompt-cache regression guard: Claude Code's real request shape (billing
// block first, then the OAuth identity) must reach the upstream with a prefix
// that is byte-stable across turns. v0.27.x prepended a second identity block,
// pushing the per-request billing block into the cached prefix: every turn
// missed the cache. Runs through forward mode, the default `launch claude`.
// The same shape routed to a ChatGPT-style (Responses) provider must drop the
// billing block: it changes every request and broke OpenAI prefix caching.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { authStorePath, isolatedConfigDir, parseSse, readBody, startProxy, stopProxy, type ProxyHandle } from "./helpers";
import { startMockUpstream, type MockUpstream } from "./mock-upstream";

const IDENTITY = "You are Claude Code, Anthropic's official CLI for Claude.";
const { dir: configDir, env: isolatedEnv } = isolatedConfigDir();

let mock: MockUpstream;
let proxy: ProxyHandle;
let ca: string;

beforeAll(async () => {
  writeFileSync(authStorePath(configDir), JSON.stringify({
    mock_claude_oauth: { type: "oauth", access: "sub-token", refresh: "r", expires: Date.now() + 3_600_000 },
    mock_chatgpt: { type: "api", key: "test-chatgpt" },
  }));
  mock = await startMockUpstream({ simulateCache: true });
  const base = `http://127.0.0.1:${mock.port}`;
  proxy = await startProxy(`
server:
  host: 127.0.0.1
  port: 0
  api_keys: [proxy-key]
  passthrough_keys: false
providers:
  - name: mock_claude_oauth
    base_url: ${base}
    format: anthropic
    oauth:
      token_url: ${base}/token
      identity: "${IDENTITY}"
  - name: mock_chatgpt
    base_url: ${base}
    format: openai-responses
routes:
  - model: gpt-mock
    provider: mock_chatgpt
    upstream_model: gpt-mock
  - model: claude-opus-5-5
    provider: mock_claude_oauth
    upstream_model: claude-opus-5-5
defaults:
  provider: ""
`, undefined, isolatedEnv);
  ca = readFileSync(join(configDir, "forward-ca.pem"), "utf8");
});

afterAll(() => {
  stopProxy(proxy);
  mock.stop();
});

const system = [
  { type: "text", text: "x-anthropic-billing-header: cc_version=9.9.9; cc_entrypoint=cli; cch=abc12;" },
  { type: "text", text: IDENTITY, cache_control: { type: "ephemeral" } },
  { type: "text", text: "Project instructions.", cache_control: { type: "ephemeral" } },
];
const tools = [{ name: "Read", description: "read a file", input_schema: { type: "object", properties: {} } }];

async function turn(messages: unknown[], model = "claude-opus-5-5", billing = system[0]) {
  const res = await fetch("https://api.anthropic.com/v1/messages?beta=true", {
    method: "POST",
    proxy: proxy.base,
    tls: { ca },
    headers: {
      "content-type": "application/json",
      "anthropic-version": "2023-06-01",
      authorization: "Bearer sk-ant-oat-user-token",
      "x-claude-code-session-id": "cache-e2e-session",
    },
    body: JSON.stringify({ model, max_tokens: 16, stream: true, system: [billing, ...system.slice(1)], tools, messages }),
  } as RequestInit);
  const text = await readBody(res);
  expect(res.status, text).toBe(200);
  const frames = parseSse(text);
  const start = frames.find((f) => f.event === "message_start")!;
  // Translated streams (Responses) only know usage at the end: merge it in.
  const delta = frames.filter((f) => f.event === "message_delta").at(-1);
  const usage = { ...JSON.parse(start.data).message.usage, ...(delta ? JSON.parse(delta.data).usage : {}) };
  return { usage, sent: mock.requests().at(-1)!.body };
}

describe("prompt cache", () => {
  test("claude -p shape (SDK identity): the billing block stays first, identity right after", async () => {
    const sdk = [system[0], { type: "text", text: "You are a Claude agent, built on Anthropic's Claude Agent SDK." }, system[2]];
    const res = await fetch("https://api.anthropic.com/v1/messages?beta=true", {
      method: "POST", proxy: proxy.base, tls: { ca },
      headers: { "content-type": "application/json", "anthropic-version": "2023-06-01", authorization: "Bearer sk-ant-oat-user-token" },
      body: JSON.stringify({ model: "claude-opus-5-5", max_tokens: 16, system: sdk, tools, messages: [{ role: "user", content: "hi" }] }),
    } as RequestInit);
    expect(res.status, await res.text()).toBe(200);
    expect(mock.requests().at(-1)!.body.system).toEqual([sdk[0], { type: "text", text: IDENTITY }, sdk[1], sdk[2]]);
  });

  test("system reaches the upstream unchanged and the prefix is stable across turns", async () => {
    const one = await turn([{ role: "user", content: "hi" }]);
    const two = await turn([
      { role: "user", content: "hi" },
      { role: "assistant", content: "oi" },
      { role: "user", content: "again" },
    ]);
    expect(one.sent.system).toEqual(system);
    expect(two.sent.system).toEqual(one.sent.system);
    expect(two.sent.tools).toEqual(one.sent.tools);
  });

  test("a repeated prefix is read from cache ", async () => {
    const messages = [{ role: "user", content: "same turn" }];
    const cold = await turn(messages);
    const warm = await turn(messages);
    expect(cold.usage.cache_creation_input_tokens).toBeGreaterThan(0);
    expect(warm.usage.cache_read_input_tokens).toBeGreaterThan(0);
  });
});

describe("prompt cache: ChatGPT (Responses) via a Code Mode id", () => {
  // Claude Code changes cch/prompt id/turn index on every request.
  const billing = (n: number) => ({ type: "text", text: `x-anthropic-billing-header: cc_version=9.9.9; cch=${n}; cc_turn_index=${n};` });
  const MODEL = "claude-opus-5-5+gpt-mock";

  test("the billing block is dropped and instructions stay byte-stable across turns", async () => {
    const one = await turn([{ role: "user", content: "hi" }], MODEL, billing(1));
    const two = await turn([
      { role: "user", content: "hi" },
      { role: "assistant", content: "oi" },
      { role: "user", content: "again" },
    ], MODEL, billing(2));
    expect(mock.requests().at(-1)!.path).toBe("/responses");
    expect(one.sent.model).toBe("gpt-mock");
    expect(one.sent.instructions).not.toContain("x-anthropic-billing-header");
    expect(two.sent.instructions).toBe(one.sent.instructions);
    expect(two.sent.tools).toEqual(one.sent.tools);
    // Append-only input: the earlier turn is a prefix of the later one.
    expect(two.sent.input.slice(0, one.sent.input.length)).toEqual(one.sent.input);
  });

  test("a repeated prefix is read from cache even when the billing block changes", async () => {
    const messages = [{ role: "user", content: "same chatgpt turn" }];
    await turn(messages, MODEL, billing(10));
    const warm = await turn(messages, MODEL, billing(11));
    expect(warm.usage.cache_read_input_tokens).toBeGreaterThan(0);
  });
});
