// Drives the real Claude Code headless through the proxy and records every
// exchange, so an agent can see exactly what Claude Code sent and got back.
//
//   bun scripts/drive-claude.ts --model chatgpt/gpt-6-sol "diga oi" [-- extra claude args]
//
// Claude Code -> tee (records) -> `local-proxy serve` on its own port -> upstream.
// Output dir (printed at the end): NNN-req.json, NNN-res.txt (raw SSE/JSON with
// status + headers), claude.jsonl (stream-json transcript), proxy.log.
// LOCAL_PROXY_BIN overrides the proxy binary (default: `local-proxy` on PATH).
import { mkdirSync, writeFileSync, createWriteStream } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const argv = process.argv.slice(2);
const sep = argv.indexOf("--");
const own = sep < 0 ? argv : argv.slice(0, sep);
const extra = sep < 0 ? [] : argv.slice(sep + 1);
const mi = own.indexOf("--model");
const model = mi < 0 ? undefined : own.splice(mi, 2)[1];
const prompt = own.join(" ") || "diga oi";

const out = join(tmpdir(), "drive-claude", new Date().toISOString().replace(/[:.]/g, "-"));
mkdirSync(out, { recursive: true });

const freePort = () => {
  const s = Bun.serve({ port: 0, fetch: () => new Response() });
  const p = s.port;
  s.stop(true);
  return p;
};

const proxyPort = freePort();
const proxyLog = createWriteStream(join(out, "proxy.log"));
const proxy = Bun.spawn([process.env.LOCAL_PROXY_BIN ?? "local-proxy", "serve", "--port", String(proxyPort)], {
  stdout: "pipe",
  stderr: "pipe",
});
for (const s of [proxy.stdout, proxy.stderr]) (async () => { for await (const c of s) proxyLog.write(c); })();

const upstream = `http://127.0.0.1:${proxyPort}`;
for (let i = 0; ; i++) {
  if (await fetch(`${upstream}/v1/models`).then((r) => r.ok, () => false)) break;
  if (i > 100) throw new Error("proxy did not start; see " + join(out, "proxy.log"));
  await Bun.sleep(100);
}

let n = 0;
const tee = Bun.serve({
  port: 0,
  idleTimeout: 0,
  async fetch(req) {
    const id = String(++n).padStart(3, "0");
    const url = new URL(req.url);
    const body = req.method === "GET" ? undefined : await req.text();
    writeFileSync(join(out, `${id}-req.json`), `${req.method} ${url.pathname}${url.search}\n${body ?? ""}`);
    const res = await fetch(upstream + url.pathname + url.search, {
      method: req.method,
      headers: req.headers,
      body,
      decompress: false,
    });
    const [mine, theirs] = res.body ? res.body.tee() : [null, null];
    (async () => {
      const head = `${res.status}\n${[...res.headers].map(([k, v]) => `${k}: ${v}`).join("\n")}\n\n`;
      writeFileSync(join(out, `${id}-res.txt`), head + (mine ? await new Response(mine).text() : ""));
    })();
    return new Response(theirs, { status: res.status, headers: res.headers });
  },
});

const claudeArgs = ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose", ...extra];
if (model) claudeArgs.push("--model", model);
const cc = Bun.spawn(claudeArgs, {
  env: {
    ...process.env,
    ANTHROPIC_BASE_URL: `http://127.0.0.1:${tee.port}`,
    ANTHROPIC_AUTH_TOKEN: process.env.LOCAL_PROXY_KEY ?? "sk-proxy",
    ANTHROPIC_API_KEY: "",
    CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY: "1",
  },
  stdout: "pipe",
  stderr: "inherit",
});
const transcript = await new Response(cc.stdout).text();
await cc.exited;
await Bun.sleep(200); // let the last response bodies flush to disk
writeFileSync(join(out, "claude.jsonl"), transcript);

const result = transcript.split("\n").filter(Boolean).map((l) => { try { return JSON.parse(l); } catch { return null; } })
  .find((e) => e?.type === "result");
console.log(JSON.stringify({ exit: cc.exitCode, result: result?.result, is_error: result?.is_error, usage: result?.usage }, null, 2));
console.log(`recorded ${n} exchanges in ${out}`);

tee.stop(true);
proxy.kill();
proxyLog.end();
