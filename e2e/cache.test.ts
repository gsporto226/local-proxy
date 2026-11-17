// Prompt-cache regression guard: Claude Code's real request shape (billing
// block first, then the OAuth identity) must reach the upstream with a prefix
// that is byte-stable across turns. v0.27.x prepended a second identity block,
// pushing the per-request billing block into the cached prefix: every turn
// missed the cache. Runs through forward mode, the default `launch claude`.
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
routes:
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

async function turn(messages: unknown[]) {
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
    body: JSON.stringify({ model: "claude-opus-5-5", max_tokens: 16, stream: true, system, tools, messages }),
  } as RequestInit);
  const text = await readBody(res);
  expect(res.status, text).toBe(200);
  const start = parseSse(text).find((f) => f.event === "message_start")!;
  return { usage: JSON.parse(start.data).message.usage, sent: mock.requests().at(-1)!.body };
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
