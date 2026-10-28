import { expect, test } from 'bun:test';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { authStorePath, isolatedConfigDir, startProxy, type ProxyHandle } from './helpers';
import { mockConfig, startMockUpstream } from './mock-upstream';

const claude = Bun.which('claude');

test.skipIf(!claude)('mod selection changes a running Claude Code session without changing proxy defaults', async () => {
  const { dir, env } = isolatedConfigDir();
  const claudeDir = mkdtempSync(join(tmpdir(), 'claude-selection-'));
  const mock = await startMockUpstream({ models: [{ id: 'model-a' }, { id: 'model-b' }] });
  let proxy: ProxyHandle | undefined;
  let childToStop: { kill: () => unknown; exited: Promise<number> } | undefined;
  try {
    writeFileSync(authStorePath(dir), JSON.stringify({ mock_openai: { type: 'api', key: 'test-only' } }));
    const cfg = mockConfig(`http://127.0.0.1:${mock.port}`, { activeModel: 'mock_openai/model-a' })
      .replace('  active_model: mock_openai/model-a', '  active_model: mock_openai/model-a\n  active_effort: low');
    const path = join(dir, 'config.yaml');
    writeFileSync(path, cfg);
    proxy = await startProxy(cfg, undefined, env, path);
    const childEnv: Record<string, string | undefined> = { ...process.env, CLAUDE_CONFIG_DIR: claudeDir, ANTHROPIC_BASE_URL: proxy.base, ANTHROPIC_API_KEY: 'test-only', ANTHROPIC_AUTH_TOKEN: '', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1' };
    delete childEnv.CLAUDE_CODE_EFFORT_LEVEL;
    const child = Bun.spawn([claude ?? 'claude', '--plugin-dir', join(import.meta.dir, '..', 'claude-mod'), '-p', '--model', 'anthropic:mock_openai/model-a', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose', '--tools', '', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--setting-sources', '', '--no-session-persistence'], {
      cwd: claudeDir,
      env: childEnv,
      stdin: 'pipe', stdout: 'pipe', stderr: 'pipe',
    });
    childToStop = child;
    let output = '';
    const read = (async () => { for await (const chunk of child.stdout) output += new TextDecoder().decode(chunk); })();
    const errors = new Response(child.stderr).text();
    async function waitFor(predicate: () => boolean) {
      const deadline = Date.now() + 15000;
      while (!predicate()) {
        if (Date.now() > deadline) throw new Error(`Timed out: ${output.slice(-2000)}`);
        await Bun.sleep(20);
      }
    }
    const send = (content: string) => child.stdin.write(JSON.stringify({ type: 'user', message: { role: 'user', content } }) + '\n');
    send('say hello');
    await waitFor(() => output.includes('"type":"result"'));
    send('/proxy model anthropic:mock_openai/model-b');
    await waitFor(() => output.includes('Changing session model'));
    await waitFor(() => mock.requests().some(request => request.body.model === 'model-b'));
    send('/proxy effort medium');
    await waitFor(() => output.includes('Changing session effort'));
    send('say hello again');
    await waitFor(() => (output.match(/"type":"result"/g)?.length ?? 0) >= 2);
    send('/proxy effort');
    await waitFor(() => output.includes('medium (request)'));
    expect(mock.requests().at(-1)?.body).toMatchObject({ model: 'model-b', reasoning_effort: 'medium' });
    send('/model anthropic:mock_openai/model-a');
    send('/effort high');
    send('say hello a third time');
    await waitFor(() => (output.match(/"type":"result"/g)?.length ?? 0) >= 3);
    send('/proxy effort');
    await waitFor(() => output.includes('high (request)'));
    child.stdin.end();
    const timer = setTimeout(() => child.kill(), 15000);
    const code = await child.exited;
    clearTimeout(timer);
    await read;
    expect(code, await errors).toBe(0);
    const sent = mock.requests().at(-1)?.body;
    expect(sent?.model).toBe('model-a');
    expect(sent?.reasoning_effort).toBe('high');
    const defaults = await fetch(`${proxy.base}/admin/status`).then(response => response.json());
    expect(defaults.model).toBe('mock_openai/model-a');
    expect(defaults.effort).toBe('low');
  } finally {
    if (childToStop) { childToStop.kill(); await childToStop.exited; }
    if (proxy) { proxy.proc.kill(); await proxy.proc.exited; }
    mock.stop();
    rmSync(dir, { recursive: true, force: true });
    rmSync(claudeDir, { recursive: true, force: true });
  }
}, 45000);
