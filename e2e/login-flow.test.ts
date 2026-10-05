// End-to-end check of the `callback` OAuth login flow: a mock authorization
// server, plus the real `local-proxy connect chatgpt --oauth` driving it,
// verifying the code exchange and the auth.json entry it writes.
//
// Everything runs against a throwaway config dir (LOCAL_PROXY_CONFIG_DIR), so
// the user's real auth store is never read or written. A config overlay points
// the chatgpt `oauth:` recipe at the mock. The browser is never opened: the
// test reads the authorization URL the CLI prints and fetches it, which is what
// a browser would do.
import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

import { BINARY, authStorePath, findFreePort } from "./helpers";

// The JWT the token endpoint returns; the proxy reads the account id from this
// claim namespace.
function idToken(accountId: string): string {
  const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  return `${b64({ alg: "none" })}.${b64({
    "https://api.openai.com/auth": { chatgpt_account_id: accountId },
  })}.sig`;
}

/** Spawn `connect chatgpt --oauth` with the recipe pointed at the mock server. */
async function spawnLogin(configDir: string, serverPort: number) {
  const redirectPort = await findFreePort();
  const configPath = join(configDir, "config.yaml");
  writeFileSync(
    configPath,
    `providers:
  - name: chatgpt
    base_url: http://127.0.0.1:${serverPort}
    format: openai-responses
    oauth:
      flow: callback
      authorize_url: http://127.0.0.1:${serverPort}/authorize
      token_url: http://127.0.0.1:${serverPort}/token
      client_id: test-client
      scopes: [openid]
      redirect_uri: http://localhost:${redirectPort}/auth/callback
      token_encoding: form
      account_id_claim: https://api.openai.com/auth
`,
  );
  const child = spawn(BINARY, ["--config", configPath, "connect", "chatgpt", "--oauth"], {
    env: {
      ...process.env,
      LOCAL_PROXY_CONFIG_DIR: configDir,
      // Do not launch a real browser; the test plays the browser's part.
      LOCAL_PROXY_OAUTH_NO_BROWSER: "1",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  // The CLI prints the authorization URL; the caller fetches it.
  let buffered = "";
  const authorizeUrl = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`no authorize URL in output:\n${buffered}`)),
      20000,
    );
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      buffered += chunk;
      const match = buffered.match(/https?:\/\/\S+authorize\?\S+/);
      if (match) {
        clearTimeout(timer);
        resolve(match[0]);
      }
    });
    child.on("close", () => {
      clearTimeout(timer);
      reject(new Error(`login exited before printing a URL:\n${buffered}`));
    });
  });
  const exit = new Promise<number>((resolve) => child.on("close", (c) => resolve(c ?? 1)));
  return { authorizeUrl, exit };
}

test("connect --oauth stores an oauth entry with the account id from the id_token", async () => {
  const configDir = mkdtempSync(join(tmpdir(), "local-proxy-login-"));
  let authorizeCalls = 0;

  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url);
      if (url.pathname === "/authorize") {
        // Act like the identity provider: bounce straight back to the redirect
        // URI with a code, echoing the state the CLI sent.
        authorizeCalls += 1;
        const state = url.searchParams.get("state") ?? "";
        const redirect = url.searchParams.get("redirect_uri") ?? "";
        void fetch(`${redirect}?code=auth-code-123&state=${encodeURIComponent(state)}`);
        return new Response("<h1>consent</h1>", {
          headers: { "content-type": "text/html" },
        });
      }
      if (url.pathname === "/token") {
        const form = new URLSearchParams(await req.text());
        expect(form.get("grant_type")).toBe("authorization_code");
        expect(form.get("code")).toBe("auth-code-123");
        expect(form.get("code_verifier") ?? "").not.toBe("");
        return Response.json({
          access_token: "access-abc",
          refresh_token: "refresh-xyz",
          id_token: idToken("acct-777"),
          expires_in: 3600,
        });
      }
      return new Response("not found", { status: 404 });
    },
  });

  try {
    const { authorizeUrl, exit } = await spawnLogin(configDir, server.port);
    expect(authorizeUrl).toContain("code_challenge_method=S256");
    await fetch(authorizeUrl);

    expect(await exit).toBe(0);
    expect(authorizeCalls).toBe(1);

    const auth = JSON.parse(readFileSync(authStorePath(configDir), "utf8"));
    expect(auth.chatgpt.type).toBe("oauth");
    expect(auth.chatgpt.access).toBe("access-abc");
    expect(auth.chatgpt.refresh).toBe("refresh-xyz");
    expect(auth.chatgpt.account_id).toBe("acct-777");
    expect(auth.chatgpt.expires).toBeGreaterThan(Date.now());
  } finally {
    server.stop(true);
    rmSync(configDir, { recursive: true, force: true });
  }
}, 30000);

test("connect --oauth rejects a state that does not match", async () => {
  const configDir = mkdtempSync(join(tmpdir(), "local-proxy-login-csrf-"));

  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const url = new URL(req.url);
      if (url.pathname === "/authorize") {
        const redirect = url.searchParams.get("redirect_uri") ?? "";
        // Tamper with the state: the callback must reject it.
        void fetch(`${redirect}?code=auth-code-123&state=not-the-right-state`);
        return new Response("consent");
      }
      // A rejected state must never reach the token endpoint.
      if (url.pathname === "/token") {
        return Response.json({ access_token: "should-not-happen" });
      }
      return new Response("not found", { status: 404 });
    },
  });

  try {
    const { authorizeUrl, exit } = await spawnLogin(configDir, server.port);
    await fetch(authorizeUrl);

    expect(await exit).not.toBe(0);
    // No auth.json written on a rejected login.
    expect(() => readFileSync(authStorePath(configDir), "utf8")).toThrow();
  } finally {
    server.stop(true);
    rmSync(configDir, { recursive: true, force: true });
  }
}, 30000);
