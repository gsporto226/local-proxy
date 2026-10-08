import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'
import { formatResetTime, fuzzyMatch, meter, plain, providerOrder } from './register'

const BASE = 'http://127.0.0.1:9999'
const ROUTES: Record<string, unknown> = {
  'GET /admin/status': { version: '0.25.2', port: 9999, pid: 1, model: 'openai/gpt-x', effort: 'high' },
  'GET /admin/accounts': [
    { provider: 'openai', alias: 'work', is_default: true },
    { provider: 'openai', alias: 'home', is_default: false },
    { provider: 'anthropic', alias: 'personal', is_default: false },
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
    { kind: 'unavailable', provider: 'anthropic', alias: 'personal', error: 'No usage endpoint' },
  ],
  'GET /admin/models': ['gpt-x', 'gpt-y'],
  'GET /admin/rate-limits': { h5: 12, week: 40 },
  'GET /admin/stats?since=session&session_id=sess-1': {
    scope: { kind: 'session', session_id: 'sess-1' },
    summary: {
      requests: 3,
      input_tokens: 10,
      output_tokens: 20,
      cost_usd: 0.25,
      cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 66.67 },
    },
    providers: [{ provider: 'openai', requests: 3, input_tokens: 10, output_tokens: 20, cost_usd: 0.25,
      cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 66.67 } }],
    accounts: [
      { provider: 'openai', alias: 'work', requests: 2, input_tokens: 10, output_tokens: 20, cost_usd: 0.25,
        cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 100 } },
      { provider: 'openai', alias: 'home', requests: 1, input_tokens: 0, output_tokens: 0, cost_usd: 0,
        cache: { hit_requests: 0, reported_requests: 0, rate_percent: null, coverage_percent: 0 } },
    ],
    recent: [],
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
  const routes = { ...(opts.routes ?? ROUTES) }
  mock.clock(on)
  mock.env(on, opts.baseUrl === undefined ? { ANTHROPIC_BASE_URL: BASE } : opts.baseUrl ? { ANTHROPIC_BASE_URL: opts.baseUrl } : {})
  on('http.fetch', ($, e) => {
    const key = `${e.init?.method ?? 'GET'} ${e.url.replace(BASE, '')}`
    calls.push(e.init?.body ? `${key} ${e.init.body}` : key)
    if (e.init?.method === 'PUT' && (key === 'PUT /admin/model' || key === 'PUT /admin/effort')) {
      const field = key.endsWith('model') ? 'model' : 'effort'
      const current = routes['GET /admin/status'] as Record<string, unknown>
      routes['GET /admin/status'] = { ...current, [field]: JSON.parse(e.init.body as string)[field] }
      return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify({ message: `${field} updated` }) } }
    }
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

test('/proxy model updates the running proxy and redraws status and panel', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const band = await $.ui.mount({ ...BAND, surface: 'terminal' })
  const pane = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await band.find({ text: /openai\/gpt-x/ })).toBeDefined()
  const { text } = await $.command.run(run('model chatgpt/gpt-6-sol'))
  expect(text).toBe('model updated')
  expect(calls).toContain('PUT /admin/model {"model":"chatgpt/gpt-6-sol"}')
  expect(await band.find({ text: /chatgpt\/gpt-6-sol/ })).toBeDefined()
  expect(await pane.find({ text: 'chatgpt/gpt-6-sol' })).toBeDefined()
})

test('/proxy effort and model clear update the running proxy', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  await $.command.run(run('effort medium'))
  await $.command.run(run('model clear'))
  expect(calls).toContain('PUT /admin/effort {"effort":"medium"}')
  expect(calls).toContain('PUT /admin/model {"model":null}')
  const pane = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await pane.find({ text: 'medium' })).toBeDefined()
  expect(await pane.find({ text: '(default)' })).toBeDefined()
})

test('/proxy opens a focused pane; every tab renders on terminal and desktop', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  expect(calls).toContain('GET /admin/stats?since=session&session_id=sess-1')
  const expected = { Accounts: /home/, Usage: /40% used/, Logs: /boot|No log lines/, Config: /openai\/work/, Status: /running/ }
  for (const surface of ['terminal', 'desktop'] as const) {
    const ui = await $.ui.mount({ ...PANE, surface })
    expect(await ui.find({ text: /work/ }), `${surface} opens Accounts`).toBeDefined()
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

test('the pane requests 90% of the terminal and closes once it loses focus', async ($, on) => {
  const { opened, openArgs } = mockProxy(on)
  await start($)
  const band = await $.ui.mount({ ...BAND, surface: 'terminal', viewport: { columns: 120, rows: 50 } } as never)
  await band.unmount()
  await $.command.run(run(''))
  expect(openArgs[0]).toMatchObject({ rows: 45, columns: 108 })
  const focused = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await focused.unmount()
  expect(opened).toEqual(['proxy'])
  const blurred = await $.ui.mount({ ...PANE, props: { ...PANE.props, isFocused: false }, surface: 'terminal' })
  await blurred.unmount()
  expect(opened).toEqual([])
})

test('pinning an account calls the session route with this session id', async ($, on) => {
  const { calls, opened } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Accounts' })
  await ui.press({ key: 'pin-openai/home' })
  expect(calls).toContain('PUT /admin/session/sess-1/account {"provider":"openai","alias":"home"}')
  expect(opened).toEqual(['proxy'])
})

test('Accounts tab shows each account’s upstream quota windows and unavailable state', async ($, on) => {
  const { calls } = mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Accounts' })
  expect(await ui.find({ text: /work/ }), 'account alias').toBeDefined()
  expect(await ui.find({ text: /5h/ }), 'five-hour usage').toBeDefined()
  expect(await ui.find({ text: /25% used/ }), 'five-hour percent').toBeDefined()
  expect(await ui.find({ text: /7d/ }), 'weekly window').toBeDefined()
  expect(await ui.find({ text: /40% used/ }), 'weekly percent').toBeDefined()
  expect(await ui.find({ text: /resets/ }), 'reset label').toBeDefined()
  expect(await ui.find({ text: /2033/ }), 'full reset date').toBeDefined()
  expect(await ui.find({ text: /cache 100%/ }), 'account cache rate').toBeDefined()
  expect(await ui.find({ text: /cache n\/a/ }), 'cache rate remains visible without quota data').toBeDefined()
  expect(await ui.find({ text: /OAuth required/ }), 'unavailable account').toBeDefined()
  const before = calls.filter(c => c === 'GET /admin/account-usage').length
  await ui.press({ key: 'usage-refresh' })
  expect(calls.filter(c => c === 'GET /admin/account-usage').length).toBe(before + 1)
})

test('Accounts fuzzy filter matches provider and alias', async ($, on) => {
  mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await ui.find({ key: 'account-filter' })).toBeDefined()
  expect(await ui.find({ key: 'filter-focus' })).toBeDefined()
  await ui.input({ key: 'account-filter', text: 'oa/hm' })
  expect(await ui.find({ text: /home/ })).toBeDefined()
  expect(await ui.find({ text: /work/ })).toBeUndefined()
  await ui.input({ key: 'account-filter', text: '' })
  expect(await ui.find({ text: /work/ })).toBeDefined()
})

test('Usage shows request cache hit rate and telemetry coverage', async ($, on) => {
  mockProxy(on)
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await ui.press({ key: 'tab-Usage' })
  expect(await ui.find({ text: /Cache hit rate/ })).toBeDefined()
  expect(await ui.find({ text: /100%.*2\/2/ })).toBeDefined()
  expect(await ui.find({ text: /openai\/work.*100%/ })).toBeDefined()
})

test('an older proxy never presents global stats as current-session stats', async ($, on) => {
  mockProxy(on, {
    routes: {
      ...ROUTES,
      'GET /admin/stats?since=session&session_id=sess-1': {
        summary: { requests: 8, input_tokens: 80, output_tokens: 20, cost_usd: 0.1 },
        providers: [],
        accounts: [{ provider: 'openai', alias: 'work', requests: 8, input_tokens: 80, output_tokens: 20, cost_usd: 0.1 }],
      },
    },
  })
  await start($)
  await $.command.run(run(''))
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' })
  expect(await ui.find({ text: /update local-proxy/i })).toBeDefined()
  await ui.press({ key: 'tab-Usage' })
  expect(await ui.find({ text: /update local-proxy.*this session/i })).toBeDefined()
  expect(await ui.find({ text: /8 req/ })).toBeUndefined()
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

test('fuzzy account search matches provider and alias characters in order', async () => {
  expect(fuzzyMatch('oa/hm', 'openai/home')).toBe(true)
  expect(fuzzyMatch('opw', 'openai/work')).toBe(true)
  expect(fuzzyMatch('zz', 'openai/work')).toBe(false)
})

test('provider groups put pinned and default providers first', async () => {
  expect(providerOrder([
    { provider: 'zeta', alias: 'x', is_default: false },
    { provider: 'openai', alias: 'work', is_default: true },
    { provider: 'anthropic', alias: 'personal', is_default: false },
    { provider: 'google', alias: 'default', is_default: true },
  ], { anthropic: 'personal' })).toEqual(['anthropic', 'google', 'openai', 'zeta'])
})

test('quota reset time includes a local date-time and relative countdown', async () => {
  const reset = Date.UTC(2026, 9, 8, 12)
  const label = formatResetTime(String(reset / 1000), reset - 30 * 60 * 1000)
  expect(label).toMatch(/2026/)
  expect(label).toContain('(in 30m)')
  expect(label).not.toContain('UTC')
})
