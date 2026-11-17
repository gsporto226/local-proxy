// Forward-proxy mode (default `launch claude`): the client reaches the proxy as an
// HTTPS_PROXY and trusts its local CA, so `ANTHROPIC_BASE_URL` stays unset.
// `api.anthropic.com` is never resolved here: `/v1/messages*` is answered by
// the proxy's router and every other path goes to LOCAL_PROXY_PASSTHROUGH_BASE.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { connect } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { authStorePath, isolatedConfigDir, readBody, startProxy, stopProxy, type ProxyHandle } from "./helpers";
import { mockConfig, startMockUpstream, type MockUpstream } from "./mock-upstream";

const { dir: configDir, env: isolatedEnv } = isolatedConfigDir();
const caPath = join(configDir, "forward-ca.pem");

let mock: MockUpstream;
let proxy: ProxyHandle;
let echo: ReturnType<typeof Bun.serve>;
let ca: string;

beforeAll(async () => {
  writeFileSync(authStorePath(configDir), JSON.stringify({ mock_anthropic: { type: "api", key: "test-anthropic" } }));
  mock = await startMockUpstream();
  // Stands in for the real api.anthropic.com (passthrough) and for an
  // arbitrary host reached through a plain tunnel.
  echo = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch: (req) =>
      Response.json({ path: new URL(req.url).pathname, auth: req.headers.get("authorization") }),
  });
  proxy = await startProxy(
    mockConfig(`http://127.0.0.1:${mock.port}`, { apiKeys: ["proxy-key"] }),
    undefined,
    { ...isolatedEnv, LOCAL_PROXY_PASSTHROUGH_BASE: `http://127.0.0.1:${echo.port}` },
  );
  ca = readFileSync(caPath, "utf8");
});

afterAll(() => {
  stopProxy(proxy);
  mock.stop();
  echo.stop(true);
});

const viaProxy = (path: string, init: RequestInit = {}) =>
  fetch(`https://api.anthropic.com${path}`, { ...init, proxy: proxy.base, tls: { ca } } as RequestInit);

describe("forward proxy", () => {
  test("serve creates the CA in the config dir", () => {
    expect(existsSync(caPath)).toBe(true);
    expect(ca).toContain("BEGIN CERTIFICATE");
  });

  test("/v1/messages on api.anthropic.com is routed locally, with client credentials swapped", async () => {
    const res = await viaProxy("/v1/messages?beta=true", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
        // A claude.ai subscription token: must not reach the provider, and must
        // not trip the proxy's `api_keys` check.
        authorization: "Bearer sk-ant-oat-user-token",
      },
      body: JSON.stringify({
        model: "gpt-via-anthropic",
        max_tokens: 16,
        messages: [{ role: "user", content: "hi" }],
      }),
    });
    const body = await readBody(res);
    expect(res.status, body).toBe(200);
    expect(JSON.parse(body).type).toBe("message");
    const seen = mock.requests().at(-1)!;
    expect(seen.path).toBe("/v1/messages");
    expect(seen.body.model).toBe("claude-sonnet-4-5");
    expect(JSON.stringify(seen.headers)).not.toContain("sk-ant-oat-user-token");
  });

  test("other api.anthropic.com paths pass through untouched", async () => {
    const res = await viaProxy("/api/oauth/profile", { headers: { authorization: "Bearer sk-ant-oat-user-token" } });
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ path: "/api/oauth/profile", auth: "Bearer sk-ant-oat-user-token" });
  });

  test("the interception certificate is only trusted with the local CA", async () => {
    await expect(
      fetch("https://api.anthropic.com/v1/messages", { method: "POST", proxy: proxy.base } as RequestInit),
    ).rejects.toThrow(/certificate|CERT|self.signed|issuer/i);
  });

  test("CONNECT to any other host is a plain tunnel", async () => {
    const reply = await new Promise<string>((resolve, reject) => {
      const sock = connect(proxy.port, "127.0.0.1");
      let buf = "";
      let tunneled = false;
      sock.on("error", reject);
      sock.on("data", (chunk) => {
        buf += chunk.toString();
        if (!tunneled && buf.includes("\r\n\r\n")) {
          if (!buf.startsWith("HTTP/1.1 200")) return reject(new Error(buf));
          tunneled = true;
          buf = "";
          sock.write(`GET /through-tunnel HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n`);
        }
      });
      sock.on("end", () => resolve(buf));
      sock.write(`CONNECT 127.0.0.1:${echo.port} HTTP/1.1\r\nHost: 127.0.0.1:${echo.port}\r\n\r\n`);
    });
    expect(reply).toContain('"path":"/through-tunnel"');
  });

  test("plain requests the router does not know still 404", async () => {
    const res = await fetch(`${proxy.base}/nope`);
    expect(res.status).toBe(404);
  });

  // The real CLI: no ANTHROPIC_BASE_URL, only HTTPS_PROXY + NODE_EXTRA_CA_CERTS,
  // exactly what `launch claude` sets. Its request must reach the mock.
  const claude = Bun.which("claude");
  test.skipIf(!claude)("claude reaches the proxy through HTTPS_PROXY alone", async () => {
    const claudeDir = mkdtempSync(join(tmpdir(), "claude-forward-"));
    try {
      const before = mock.requests().length;
      const env: Record<string, string | undefined> = {
        ...process.env,
        CLAUDE_CONFIG_DIR: claudeDir,
        HTTPS_PROXY: proxy.base,
        NODE_EXTRA_CA_CERTS: caPath,
        ANTHROPIC_API_KEY: "test-only",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
      };
      for (const k of ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "HTTP_PROXY", "NO_PROXY", "no_proxy"]) delete env[k];
      const child = Bun.spawn(
        [claude!, "-p", "say ok", "--model", "gpt-via-anthropic", "--output-format", "json", "--tools", "",
          "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--no-session-persistence", "--setting-sources", "", "--max-turns", "1"],
        { cwd: claudeDir, env, stdout: "pipe", stderr: "pipe" },
      );
      const timeout = setTimeout(() => child.kill(), 40000);
      const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
      clearTimeout(timeout);
      expect(code, stdout + stderr).toBe(0);
      expect(JSON.parse(stdout).is_error, stdout).toBe(false);
      const seen = mock.requests().slice(before);
      expect(seen.some((r) => r.path === "/v1/messages" && r.body.model === "claude-sonnet-4-5")).toBe(true);
    } finally {
      rmSync(claudeDir, { recursive: true, force: true });
    }
  }, 60000);
});
