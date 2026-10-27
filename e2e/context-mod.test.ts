import { expect, test } from 'bun:test';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { authStorePath, isolatedConfigDir, startProxy, type ProxyHandle } from './helpers';

const claude = Bun.which('claude');
const mod = join(import.meta.dir, '..', 'claude-mod');

for (const contextWindow of [128000, 400000, 1048576]) {
  test.skipIf(!claude)(`mod applies ${contextWindow} tokens and native compaction uses that limit`, async () => {
    const { dir, env } = isolatedConfigDir();
    const claudeDir = mkdtempSync(join(tmpdir(), 'claude-context-'));
    const file = join(claudeDir, 'probe.txt');
    writeFileSync(file, 'local context probe');
    const calls: { resumedAfterCompaction: boolean }[] = [];
    let proxy: ProxyHandle | undefined;
    const upstream = Bun.serve({ hostname: '127.0.0.1', port: 0, async fetch(req) {
      if (new URL(req.url).pathname === '/v1/models') return Response.json({ data: [{ id: 'context-probe', context_length: contextWindow }] });
      const body = await req.json();
      calls.push({ resumedAfterCompaction: JSON.stringify(body.messages).includes('conversation that ran out of context') });
      const first = calls.length === 1;
      const content = first ? { type: 'tool_use', id: 'tool_probe', name: 'Read', input: { file_path: file } } : { type: 'text', text: 'ok' };
      const usage = { input_tokens: first ? 130000 : 1000, output_tokens: 10 };
      const events = [
        { type: 'message_start', message: { id: `msg_${calls.length}`, type: 'message', role: 'assistant', model: body.model, content: [], stop_reason: null, stop_sequence: null, usage } },
        { type: 'content_block_start', index: 0, content_block: first ? content : { type: 'text', text: '' } },
        ...first ? [] : [{ type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'ok' } }],
        { type: 'content_block_stop', index: 0 },
        { type: 'message_delta', delta: { stop_reason: first ? 'tool_use' : 'end_turn', stop_sequence: null }, usage },
        { type: 'message_stop' },
      ];
      return new Response(events.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join(''), { headers: { 'content-type': 'text/event-stream' } });
    } });
    try {
      writeFileSync(authStorePath(dir), JSON.stringify({ probe: { type: 'api', key: 'test-only' } }));
      const configPath = join(dir, 'config.yaml');
      const cfg = `server:\n  host: 127.0.0.1\n  port: 0\nproviders:\n  - name: probe\n    base_url: http://127.0.0.1:${upstream.port}\n    format: anthropic\n`;
      writeFileSync(configPath, cfg);
      proxy = await startProxy(cfg, undefined, env, configPath);
      const childEnv: Record<string, string | undefined> = { ...process.env, CLAUDE_CONFIG_DIR: claudeDir, ANTHROPIC_BASE_URL: proxy.base, ANTHROPIC_API_KEY: 'test-only', ANTHROPIC_AUTH_TOKEN: '', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1' };
      delete childEnv.CLAUDE_CODE_MAX_CONTEXT_TOKENS;
      delete childEnv.CLAUDE_CODE_AUTO_COMPACT_WINDOW;
      const child = Bun.spawn([claude ?? 'claude', '--plugin-dir', mod, '-p', 'Read the probe file, then say ok.', '--model', 'anthropic:probe/context-probe', '--output-format', 'json', '--tools', 'Read', '--allowedTools', 'Read', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--no-session-persistence', '--setting-sources', '', '--max-turns', '4'], { cwd: claudeDir, env: childEnv, stdout: 'pipe', stderr: 'pipe' });
      const timeout = setTimeout(() => child.kill(), 20000);
      const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
      clearTimeout(timeout);
      expect(code, stderr).toBe(0);
      const result = JSON.parse(stdout);
      expect(result.is_error).toBe(false);
      expect(result.modelUsage['anthropic:probe/context-probe'].contextWindow).toBe(contextWindow);
      // Verify that Claude installed a summary and resumed the conversation,
      // rather than just reporting a different context size in its usage.
      const compacted = calls.some(call => call.resumedAfterCompaction);
      expect(compacted, JSON.stringify(calls)).toBe(contextWindow === 128000);
    } finally {
      if (proxy) { proxy.proc.kill(); await proxy.proc.exited; }
      upstream.stop(true);
      rmSync(dir, { recursive: true, force: true });
      rmSync(claudeDir, { recursive: true, force: true });
    }
  }, 30000);
}
