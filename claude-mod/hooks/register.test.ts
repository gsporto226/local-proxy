import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const PORT = '9999'
const ROUTES: Record<string, unknown> = {
  'GET /admin/status': { version: '0.25.2', port: 9999, pid: 1, model: 'gpt-x', effort: 'high' },
  'GET /admin/accounts': [
    { provider: 'openai', alias: 'work', is_default: true },
    { provider: 'openai', alias: 'home', is_default: false },
  ],
  'GET /admin/models': ['gpt-x', 'gpt-y'],
  'GET /admin/rate-limits': { h5: 12, week: 40 },
  'GET /admin/stats?since=day': {
    summary: { requests: 3, input_tokens: 10, output_tokens: 20, cost_usd: 0.25 },
    providers: [{ provider: 'openai', requests: 3, input_tokens: 10, output_tokens: 20, cost_usd: 0.25 }],
  },
  'GET /admin/session/sess-1': { account: {}, effort: null, stats: null },
  'GET /admin/logs?n=200': { lines: ['boot'] },
}

/** Mocks the proxy's HTTP API, the clock, env, session and the SSE child; records each call. */
function mockProxy(on: On, sse: string[] = []) {
  const calls: string[] = []
  const seen = { status: undefined as string | undefined, opened: undefined as string | undefined }
  mock.clock(on)
  mock.env(on, { LOCAL_PROXY_PORT: PORT })
  on('http.fetch', ($, e) => {
    const key = `${e.init?.method ?? 'GET'} ${e.url.replace(`http://127.0.0.1:${PORT}`, '')}`
    calls.push(e.init?.body ? `${key} ${e.init.body}` : key)
    return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify(ROUTES[key] ?? {}) } }
  })
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.model', () => ({ value: 'claude-x' }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 100, percent: 42 }, rateLimits: [], cost: { usd: 1.5 } } }))
  on('ui.status', ($, e) => {
    seen.status = e.text
    return { value: undefined }
  })
  on('ui.open', ($, e) => {
    seen.opened = e.id
    return { value: { isPlaced: true } }
  })
  on('process.spawn', async function* () {
    for (const text of sse) yield { stream: 'stdout' as const, text }
    return { value: { code: 0, signal: null } }
  })
  return { calls, seen }
}

const run = (args: string) =>
  ({ command: 'proxy', args, origin: { kind: 'composer' }, presentation: { isFullscreen: true, columns: 160 } }) as const

const PANE = {
  plugin: 'local-proxy',
  component: 'Pane',
  requestId: 'proxy',
  props: { title: 'local-proxy', isFocused: true, bodyColumns: 100, placement: 'dock', scroll: { offset: 0, bodyRows: 30 }, view: {} },
} as const

test('/proxy <args> runs the CLI and returns its output', async ($, on) => {
  mockProxy(on)
  let argv: readonly string[] = []
  on('process.run', ($, e) => {
    argv = e.argv
    return { value: { exitCode: 0, stdout: 'local-proxy 0.25.2\n', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  const { text } = await $.command.run(run('--version'))
  expect(argv).toEqual(['local-proxy', '--version'])
  expect(text).toBe('local-proxy 0.25.2')
})

test('/proxy with no args opens the panel; every tab renders on terminal and desktop', async ($, on) => {
  const { seen } = mockProxy(on)
  await $.command.run(run(''))
  expect(seen.opened).toBe('proxy')
  const expected = { Overview: /up/, Accounts: /openai\/home/, Usage: /3 req/, Logs: /boot|No log lines/, Config: /openai\/work/ }
  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    for (const [tab, text] of Object.entries(expected)) {
      await ui.press({ key: `tab-${tab}` })
      expect(await ui.find({ text }), `${surface} ${tab}`).toBeDefined()
    }
    await ui.press({ key: 'tab-Overview' })
    await ui.unmount()
  }
})

test('pinning an account calls the session route with this session id', async ($, on) => {
  const { calls } = mockProxy(on)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Accounts' })
  await ui.press({ key: 'pin-openai/home' })
  expect(calls).toContain('PUT /admin/session/sess-1/account {"provider":"openai","alias":"home"}')
})

test('disconnect waits for confirmation', async ($, on) => {
  const { calls } = mockProxy(on)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Config' })
  await ui.press({ key: 'dc-openai/home' })
  expect(calls.some(c => c.startsWith('DELETE'))).toBe(false)
  await ui.press({ key: 'confirm-yes' })
  expect(calls).toContain('DELETE /admin/accounts/openai/home')
})

test('status line shows all segments by default', async ($, on) => {
  const { seen } = mockProxy(on)
  await $.command.run(run(''))
  expect(seen.status).toBe('gpt-x · ctx 42% · 5h 12% · wk 40% · session $1.50 · today $0.25')
})

test('status line follows the segments option', { options: { segments: ['todayCost', 'model'] } }, async ($, on) => {
  const { seen } = mockProxy(on)
  await $.command.run(run(''))
  expect(seen.status).toBe('today $0.25 · gpt-x')
})

test('SSE log events from the curl child reach the Logs tab', async ($, on) => {
  mockProxy(on, ['event: log\r\ndata: {"line":"hel', 'lo sse"}\r\n\r\n'])
  on('command.register', () => ({ value: { command: 'proxy' } }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  await $.session.start({ cwd: '.', surface: 'terminal', isInteractive: true })
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Logs' })
  expect(await ui.find({ text: /hello sse/ })).toBeDefined()
})
