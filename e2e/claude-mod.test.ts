import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { authStorePath, isolatedConfigDir, startProxy, stopProxy, type ProxyHandle } from "./helpers";
import { mockConfig, startMockUpstream, type MockUpstream } from "./mock-upstream";

// Guards the Claude Code mod's startup cost: a headless `claude -p` session
// through the proxy (mock upstream) must not get noticeably slower with the mod
// loaded. Skipped where the `claude` CLI is not installed.
//
// The baseline is a plugin that registers nothing (fixtures/empty-mod), not
// "no plugin": loading any hooks module starts Claude Code's plugin runtime,
// which costs ~1-2s on its own and is outside this repo's control. What the
// test pins is what the mod's own code adds on top of that.
const CLAUDE = Bun.which("claude");
const MOD_DIR = join(import.meta.dir, "..", "claude-mod");
const EMPTY_DIR = join(import.meta.dir, "fixtures", "empty-mod");
const RUNS = 5;
// "Noticeable": the larger of an absolute and a relative margin over the
// baseline, so a fast machine is not held to a sub-noise threshold.
// Run-to-run noise on a busy dev box is ~±0.5s even on the fastest run; the
// regression this guards (polling the proxy during a -p run) cost +1.3-2.6s.
const MAX_ABS_MS = 750;
const MAX_REL = 0.25;

const { dir: configDir, env: isolatedEnv } = isolatedConfigDir();
let mock: MockUpstream;
let proxy: ProxyHandle;

beforeAll(async () => {
  writeFileSync(authStorePath(configDir), JSON.stringify({ mock_anthropic: { type: "api", key: "test-key" } }));
  mock = await startMockUpstream();
  proxy = await startProxy(
    mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: "gpt-via-anthropic" }),
    undefined,
    isolatedEnv,
  );
});

afterAll(() => {
  stopProxy(proxy);
  mock.stop();
});

/** One headless session with the given plugin dir (or none); resolves its wall time in ms. */
async function session(pluginDir?: string): Promise<number> {
  // A fresh Claude config dir per run: no user plugins (the installed mod
  // included), settings or caches leak into any side of the comparison.
  const claudeDir = mkdtempSync(join(tmpdir(), "claude-mod-e2e-"));
  const argv = [CLAUDE!, "-p", "hi", "--model", "gpt-via-anthropic", ...(pluginDir ? ["--plugin-dir", pluginDir] : [])];
  const started = performance.now();
  const proc = Bun.spawn(argv, {
    env: {
      ...process.env,
      CLAUDE_CONFIG_DIR: claudeDir,
      ANTHROPIC_BASE_URL: proxy.base,
      ANTHROPIC_API_KEY: "test-key",
      LOCAL_PROXY_PORT: new URL(proxy.base).port,
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const code = await proc.exited;
  const elapsed = performance.now() - started;
  if (code !== 0) throw new Error(`claude exited ${code}: ${await new Response(proc.stderr).text()}`);
  return elapsed;
}

// Fastest run per side: background load only ever adds time, so the minimum is
// the least noisy estimate of startup cost (medians swung ~1s run to run).
const fastest = (xs: number[]) => Math.min(...xs);

describe.skipIf(!CLAUDE)("e2e: Claude Code mod startup", () => {
  test(
    "a session with the mod starts about as fast as one with an empty plugin",
    async () => {
      await session(EMPTY_DIR); // warm disk caches before measuring
      const none: number[] = [];
      const empty: number[] = [];
      const mod: number[] = [];
      // Interleaved so drift (thermal, background load) hits every side alike.
      for (let i = 0; i < RUNS; i++) {
        none.push(await session());
        empty.push(await session(EMPTY_DIR));
        mod.push(await session(MOD_DIR));
      }
      const [n, e, m] = [fastest(none), fastest(empty), fastest(mod)];
      console.log(
        `startup (fastest of ${RUNS}): no plugin ${n.toFixed(0)}ms · empty plugin ${e.toFixed(0)}ms · mod ${m.toFixed(0)}ms (Δ mod-empty ${(m - e).toFixed(0)}ms)`,
      );
      expect(m - e).toBeLessThan(Math.max(MAX_ABS_MS, e * MAX_REL));
    },
    { timeout: 300_000 },
  );
});
