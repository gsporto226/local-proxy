import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  get,
  postJson,
  readBody,
  runCli,
  startProxy,
  stopProxy,
  type ProxyHandle,
} from "./helpers";
import { mockConfig, startMockUpstream, type MockUpstream } from "./mock-upstream";

/**
 * E2E for the status line: the proxy records each request's `session_id` (from
 * the `X-Claude-Code-Session-Id` header) into `stats.db`, and `local-proxy
 * statusline --session` renders a template from that session's aggregated
 * stats. Both processes point at the same isolated config dir via
 * `LOCAL_PROXY_CONFIG_DIR` so the CLI reads the same `stats.db` the proxy
 * wrote, without touching the real per-user database.
 */

function freshConfigDir(): string {
  return mkdtempSync(join(tmpdir(), "local-proxy-statusline-e2e-"));
}

describe("e2e: statusline renders the session's recorded stats", () => {
  const SESSION = "sess-statusline-e2e";
  const OTHER = "sess-statusline-other";
  let mock: MockUpstream;
  let proxy: ProxyHandle;
  let cfgDir: string;

  beforeAll(async () => {
    cfgDir = freshConfigDir();
    mock = await startMockUpstream();
    proxy = await startProxy(
      mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: "claude-via-openai" }),
      undefined,
      { LOCAL_PROXY_CONFIG_DIR: cfgDir },
    );
  });

  afterAll(() => {
    stopProxy(proxy);
    mock.stop();
  });

  test("records stats for the session id sent in the header", async () => {
    for (const session of [SESSION, SESSION, SESSION, OTHER]) {
      const r = await postJson(
        proxy.base,
        "/v1/messages",
        { model: "ignored", max_tokens: 10, messages: [{ role: "user", content: "hi" }] },
        { "x-claude-code-session-id": session },
      );
      expect(r.status).toBe(200);
    }
    // one for OTHER, plus the health probes bear no session header
    const r = await get(proxy.base, "/v1/models");
    expect(r.status).toBe(200);
    await readBody(r);

    // Poll: the CLI may start while the db is still settling, so retry briefly.
    const tpl = "`r=${requests} tin=${tokens_in} tout=${tokens_out} cost=${cost_session}`";
    let line = "";
    for (let i = 0; i < 20 && !line; i++) {
      const res = await runCli(
        ["statusline", "--session", SESSION, "--template", tpl],
        { LOCAL_PROXY_CONFIG_DIR: cfgDir },
      );
      expect(res.exit).toBe(0);
      const l = res.output.trim();
      if (!l.startsWith("statusline: erro")) line = l;
      else await Bun.sleep(100);
    }

    // 3 requests for SESSION (the OTHER request does not count toward it);
    // each non-streaming OpenAI mock response reports prompt_tokens=3,
    // completion_tokens=2, cost=0.0042.
    expect(line).toContain("r=3");
    expect(line).toContain("tin=9"); // 3 x 3 input tokens
    expect(line).toContain("tout=6"); // 3 x 2 output tokens
    // 3 x 0.0042, rendered raw by the template
    expect(line).toContain("cost=0.0126");
  });
});
