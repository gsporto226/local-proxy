import { describe, expect, test } from "bun:test";
import { mkdtempSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { runCli } from "./helpers";

/**
 * E2E for `local-proxy statusline setup`: it writes the status-line script into
 * the config dir and registers it in Claude's settings.json (preserving other
 * keys). A custom `--settings` path plus an isolated `LOCAL_PROXY_CONFIG_DIR`
 * keep the test from touching the real per-user files.
 */

function freshDir(prefix: string): string {
  return mkdtempSync(join(tmpdir(), prefix));
}

describe("e2e: statusline setup wires the script into settings.json", () => {
  const cfgDir = freshDir("local-proxy-setup-e2e-");
  const settings = freshDir("local-proxy-setup-e2e-") + "/settings.json";

  // Point the platform home vars at empty dirs so any settings-path resolution
  // falls back to them (or is absent), never the real per-user file.
  const isolatedHome = freshDir("local-proxy-setup-e2e-home-");
  const homeEnv = {
    HOME: isolatedHome,
    USERPROFILE: isolatedHome,
  };

  test("writes the script and merges statusLine, preserving other keys", async () => {
    writeJson(settings, { theme: "dark", enabledPlugins: { "p@x": true } });

    const res = await runCli(["statusline", "--setup", "--settings", settings], {
      LOCAL_PROXY_CONFIG_DIR: cfgDir,
      ...homeEnv,
    });
    expect(res.exit).toBe(0);
    expect(res.output).toContain("status line script:");
    expect(res.output).toContain("settings.json atualizado");

    const scriptName = process.platform === "win32" ? "statusline.ps1" : "statusline.sh";
    const script = join(cfgDir, scriptName);
    expect(existsSync(script)).toBe(true);
    expect(readFileSync(script, "utf8")).toContain("local-proxy statusline");

    const obj = JSON.parse(readFileSync(settings, "utf8"));
    expect(obj.theme).toBe("dark");
    expect(obj.enabledPlugins["p@x"]).toBe(true);
    expect(obj.statusLine.type).toBe("command");
    expect(obj.statusLine.command).toContain(scriptName);
  });

  test("skips writing settings when none exist and no --settings is given", async () => {
    const res = await runCli(["statusline", "--setup"], {
      LOCAL_PROXY_CONFIG_DIR: cfgDir,
      ...homeEnv,
    });
    expect(res.exit).toBe(0);
    // no settings file -> script written + manual snippet printed
    expect(res.output).toContain("nenhum settings.json encontrado");
    expect(res.output).toContain(`"statusLine"`);
  });
});

function writeJson(path: string, value: unknown): void {
  writeFileSync(path, JSON.stringify(value, null, 2) + "\n", "utf8");
}
