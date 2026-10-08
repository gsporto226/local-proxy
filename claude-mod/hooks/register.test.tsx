import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'
import { meter, plain } from './register'

const BASE = 'http://127.0.0.1:9999'
const ROUTES: Record<string, unknown> = {
  'GET /admin/status': { version: '0.25.2', port: 9999, pid: 1, model: 'openai/gpt-x', effort: 'high' },
  'GET /admin/accounts': [
    { provider: 'openai', alias: 'work', is_default: true },
    { provider: 'openai', alias: 'home', is_default: false },
  ],
  'GET /admin/account-usage': [
    {
      kind: 'available',
      usage: {
        provider: 'openai',
        alias: 'work',
        five_hour: { utilization: 25, resets_at: '2000000000' },
        seven_day: { utilization: 40, resets_at: '2026-10-12T00:00:00Z' },
        monthly: null,
        fetched_at: 1_800_000_000,
      },
      stale: false,
      error: null,
    },
    { kind: 'unavailable', provider: 'openai', alias: 'home', error: 'OAuth required' },
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

/**
 * Mocks the proxy's HTTP API, the clock, env, session and the SSE child; records
 * each call and each registered command. `baseUrl` is Claude's ANTHROPIC_BASE_URL.
 */
function mockProxy(on: On, opts: { sse?: string[]; baseUrl?: string; routes?: Record<string, unknown> } = {}) {
  const calls: string[] = []
  const commands: string[] = []
  const opened: string[] = []
  const openArgs: unknown[] = []
  const routes = opts.routes ?? ROUTES
  mock.clock(on)
  mock.env(on, opts.baseUrl === undefined ? { ANTHROPIC_BASE_URL: BASE } : opts.baseUrl ? { ANTHROPIC_BASE_URL: opts.baseUrl } : {})
  on('http.fetch', ($, e) => {
    const key = `${e.init?.method ?? 'GET'} ${e.url.replace(BASE, '')}`
    calls.push(e.init?.body ? `${key} ${e.init.body}` : key)
    return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify(routes[key] ?? {}) } }
  })
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.model', () => ({ value: 'claude-x' }))
  on('session.usage', () => ({ value: { startedAt: 0, context: { window: 100, percent: 42 }, rateLimits: [], cost: { usd: 1.5 } } }))
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  // Stand-ins for the engine beneath: a submitted prompt passes, an empty band draws nothing.
  on('prompt.submit', ($, e) => ({ text: e.text }))
  on('ui.render', { component: 'AbovePrompt' }, ($, e) => {
    const { Box } = $.ui.resolve(e)
    return <Box />
  })
  on('ui.open', ($, e) => {
    opened.push(e.id)
    openArgs.push(e)
    return { value: { isPlaced: true } }
  })
  on('ui.close', ($, e) => {
    opened.splice(opened.indexOf(e.id) >>> 0, 1)
    return { value: undefined }
  })
  on('command.register', ($, e) => {
    commands.push(e.name)
    return { value: { command: e.name } }
  })
  on('process.spawn', async function* () {
    for (const text of opts.sse ?? []) yield { stream: 'stdout' as const, text }
    return { value: { code: 0, signal: null } }
  })
  return { calls, commands, opened, openArgs }
}

type Dollar = Parameters<Parameters<typeof test>[1] & (($: any, on: On) => unknown)>[0]
const start = ($: Dollar) => $.session.start({ cwd: '.', surface: 'terminal', isInteractive: true })

const run = (args: string) =>
  ({ command: 'proxy', args, origin: { kind: 'composer' }, presentation: { isFullscreen: true, columns: 160 } }) as const

const PANE = {
  plugin: 'local-proxy',
  component: 'Pane',
  requestId: 'proxy',
  props: { title: 'local-proxy', isFocused: true, bodyColumns: 100, placement: 'inline', scroll: { offset: 0, bodyRows: 18 }, view: {} },
} as const

const BAND = {
  plugin: 'local-proxy',
  component: 'AbovePrompt',
  props: { hasSurvey: false, isWorking: false, maxRows: 30, bodyColumns: 100, scroll: { offset: 0, bodyRows: 30 }, view: {} },
} as const

test('/proxy <args> runs the CLI and returns its output', async ($, on) => {
  mockProxy(on)
  let argv: readonly string[] = []
  on('process.run', ($, e) => {
    argv = e.argv
    return { value: { exitCode: 0, stdout: 'local-proxy 0.25.2\n', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  await start($)
  const { text } = await $.command.run(run('--version'))
  expect(argv).toEqual(['local-proxy', '--version'])
  expect(text).toBe('local-proxy 0.25.2')
})

test('/proxy opens a focused pane; every tab renders on terminal and desktop', async ($, on) => {
  mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const expected = { Accounts: /openai\/home/, Usage: /40% used/, Logs: /boot|No log lines/, Config: /openai\/work/, Status: /running/ }
  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ text: /running/ }), `${surface} Status`).toBeDefined()
    for (const [tab, text] of Object.entries(expected)) {
      await ui.press({ key: `tab-${tab}` })
      expect(await ui.find({ text }), `${surface} ${tab}`).toBeDefined()
    }
    await ui.unmount()
  }
})

test('/proxy opens the pane focused with Esc closing it; the next prompt closes it', async ($, on) => {
  const { opened, openArgs } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  expect(openArgs[0]).toMatchObject({ id: 'proxy', focus: true, closeOnEscape: true })
  expect(opened).toEqual(['proxy'])
  await $.prompt.submit({ text: 'hi', origin: { kind: 'composer' } } as never)
  expect(opened).toEqual([])
})

test('the pane takes 80% of the terminal and closes once it loses focus', async ($, on) => {
  const { opened, openArgs } = mockProxy(on)
  await start($)
  const band = await $.ui.mount({ ...BAND, surface: 'terminal', viewport: { columns: 120, rows: 50 } } as never)
  await band.unmount()
  await $.command.run(run(''))
  expect(openArgs[0]).toMatchObject({ rows: 40 })
  const focused = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await focused.unmount()
  expect(opened).toEqual(['proxy'])
  const blurred = await $.ui.mount({ ...PANE, props: { ...PANE.props, isFocused: false }, surface: 'terminal' })
  await blurred.unmount()
  expect(opened).toEqual([])
})

test('pinning an account calls the session route with this session id', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Accounts' })
  await ui.press({ key: 'pin-openai/home' })
  expect(calls).toContain('PUT /admin/session/sess-1/account {"provider":"openai","alias":"home"}')
})

test('Accounts tab shows each account’s upstream quota windows and unavailable state', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Accounts' })
  expect(await ui.find({ text: /openai\/work/ })).toBeDefined()
  expect(await ui.find({ text: /5h/ })).toBeDefined()
  expect(await ui.find({ text: /25% used/ })).toBeDefined()
  expect(await ui.find({ text: /7d/ })).toBeDefined()
  expect(await ui.find({ text: /40% used/ })).toBeDefined()
  expect(await ui.find({ text: /resets/ })).toBeDefined()
  expect(await ui.find({ text: /OAuth required/ })).toBeDefined()
  const before = calls.filter(c => c === 'GET /admin/account-usage').length
  await ui.press({ key: 'usage-refresh' })
  expect(calls.filter(c => c === 'GET /admin/account-usage').length).toBe(before + 1)
})

test('disconnect waits for confirmation', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Config' })
  await ui.press({ key: 'dc-openai/home' })
  expect(calls.some(c => c.startsWith('DELETE'))).toBe(false)
  await ui.press({ key: 'confirm-yes' })
  expect(calls).toContain('DELETE /admin/accounts/openai/home')
})

test('status row shows all segments by default', async ($, on) => {
  mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  expect(await ui.find({ text: 'openai/gpt-x · high · openai/work · 42% ctx · 12% 5h · 40% wk' })).toBeDefined()
})

test('status row prefers the session pin and follows the segments option', { options: { segments: ['account', 'model'] } }, async ($, on) => {
  mockProxy(on, { routes: { ...ROUTES, 'GET /admin/session/sess-1': { account: { openai: 'home' }, effort: null, stats: null } } })
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
  expect(await ui.find({ text: 'openai/home · openai/gpt-x' })).toBeDefined()
})

for (const [why, opts] of [
  ['no ANTHROPIC_BASE_URL', { baseUrl: '' }],
  ['a remote base URL', { baseUrl: 'https://api.anthropic.com' }],
  ['a local gateway that is not local-proxy', { routes: {} }],
] as const) {
  test(`the mod stays inert with ${why}`, async ($, on) => {
    const { commands, calls } = mockProxy(on, opts)
    await start($)
    expect(commands).toEqual([])
    const ui = await $.ui.mount({ ...BAND, surface: 'terminal' })
    expect(await ui.find({ text: /ctx|running/ })).toBeUndefined()
    expect(calls.filter(c => !c.endsWith('/admin/status'))).toEqual([])
  })
}

test('meter fills proportionally with an eighth-block edge', async () => {
  expect(meter(0, 4)).toBe('    ')
  expect(meter(50, 4)).toBe('██  ')
  expect(meter(100, 4)).toBe('████')
  expect(meter(31, 4)).toBe('█▎  ')
})

test('Logs tab draws ANSI-coloured proxy lines as plain text', async ($, on) => {
  // A real line from a proxy whose tracing wrote colour codes into its log file.
  const ansi = '\x1b[2m2026-10-07T18:50:55Z\x1b[0m \x1b[34mDEBUG\x1b[0m \x1b[2mtower_http\x1b[0m: finished'
  mockProxy(on, { routes: { ...ROUTES, 'GET /admin/logs?n=200': { lines: [ansi] } } })
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Logs' })
  expect(await ui.find({ text: '2026-10-07T18:50:55Z DEBUG tower_http: finished' })).toBeDefined()
  expect(plain(ansi)).toBe('2026-10-07T18:50:55Z DEBUG tower_http: finished')
})

test('SSE log events from the curl child reach the Logs tab', async ($, on) => {
  mockProxy(on, { sse: ['event: log\r\ndata: {"line":"hel', 'lo sse"}\r\n\r\n'] })
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Logs' })
  expect(await ui.find({ text: /hello sse/ })).toBeDefined()
})
