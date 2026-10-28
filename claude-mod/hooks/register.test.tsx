import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'
import { cacheTokenText, formatResetTime, fuzzyMatch, meter, plain, providerOrder, tailFit } from './register'

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
      cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 66.67, read_tokens: 600, write_tokens: 200, reported_input_tokens: 1000, uncached_tokens: 200, read_percent: 60 },
    },
    providers: [{ provider: 'openai', requests: 3, input_tokens: 10, output_tokens: 20, cost_usd: 0.25,
      cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 66.67, read_tokens: 600, write_tokens: 200, reported_input_tokens: 1000, uncached_tokens: 200, read_percent: 60 } }],
    accounts: [
      { provider: 'openai', alias: 'work', requests: 2, input_tokens: 10, output_tokens: 20, cost_usd: 0.25,
        cache: { hit_requests: 2, reported_requests: 2, rate_percent: 100, coverage_percent: 100 } },
      { provider: 'openai', alias: 'home', requests: 1, input_tokens: 0, output_tokens: 0, cost_usd: 0,
        cache: { hit_requests: 0, reported_requests: 0, rate_percent: null, coverage_percent: 0 } },
    ],
    recent: [{ ts: 1, provider: "openai", alias: "work", model: "gpt-x", cache_read_tokens: null, cache_write_tokens: 0, cache_diagnostics: ["telemetry_unavailable", "stable_prefix_changed"] }],
  },
  'GET /admin/session/sess-1': { account: {}, effort: null, stats: null },
  'GET /admin/logs?n=200': { lines: ['boot'] },
}

/**
 * Mocks the proxy's HTTP API, the clock, env, session and the SSE child; records
 * each call and each registered command. `baseUrl` is Claude's ANTHROPIC_BASE_URL.
 */
function mockProxy(on: On, opts: { sse?: string[]; baseUrl?: string; routes?: Record<string, unknown>; env?: Record<string, string>; model?: () => string; contextDown?: () => boolean } = {}) {
  const calls: string[] = []
  const commands: string[] = []
  const nativeCommands: string[] = []
  let selectedModel = 'anthropic:openai/gpt-x'
  const opened: string[] = []
  const openArgs: unknown[] = []
  const routes = { ...(opts.routes ?? ROUTES) }
  const clock = mock.clock(on)
  mock.env(on, { ...(opts.baseUrl === undefined ? { ANTHROPIC_BASE_URL: BASE } : opts.baseUrl ? { ANTHROPIC_BASE_URL: opts.baseUrl } : {}), ...opts.env })
  on('http.fetch', ($, e) => {
    const key = `${e.init?.method ?? 'GET'} ${e.url.replace(BASE, '')}`
    calls.push(e.init?.body ? `${key} ${e.init.body}` : key)
    if (key.startsWith('GET /admin/model-context?') && opts.contextDown?.()) return { value: { status: 503, ok: false, headers: {}, text: '{}' } }
    if (e.init?.method === 'PUT' && (key === 'PUT /admin/model' || key === 'PUT /admin/effort')) {
      const field = key.endsWith('model') ? 'model' : 'effort'
      const current = routes['GET /admin/status'] as Record<string, unknown>
      routes['GET /admin/status'] = { ...current, [field]: JSON.parse(e.init.body as string)[field] }
      return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify({ message: `${field} updated` }) } }
    }
    return { value: { status: 200, ok: true, headers: {}, text: JSON.stringify(routes[key] ?? {}) } }
  })
  on('session.id', () => ({ value: 'sess-1' }))
  on('session.model', () => ({ value: opts.model?.() ?? selectedModel }))
  on('command.run', { command: 'model' }, ($, e) => {
    nativeCommands.push(`model ${e.args}`)
    selectedModel = e.args === 'default' ? 'claude-x' : e.args
    return { text: 'model updated' }
  })
  on('command.run', { command: 'effort' }, ($, e) => {
    nativeCommands.push(`effort ${e.args}`)
    return { text: 'effort updated' }
  })
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
  return { calls, commands, nativeCommands, opened, openArgs, clock }
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

// The statusline mod is optional at run time; the tests that watch the publish
// call load a stub that provides `$.statusline`, the noun its engine.create adds.
const STATUSLINE = {
  name: 'statusline',
  register(on: On) {
    on('engine.create', async ($: any, e: any, next: any) => {
      const built = await next(e)
      return { ...built, statusline: { publish: () => {} } }
    })
  },
}

test('context follows the selected model and restores the original limit for unknown models', async ($, on) => {
  const routes = { ...ROUTES,
    'GET /admin/model-context?model=provider%2Fsmall': { context_window: 128000 },
    'GET /admin/model-context?model=provider%2Fmedium': { context_window: 400000 },
    'GET /admin/model-context?model=provider%2Flarge': { context_window: 1048576 },
    'GET /admin/model-context?model=provider%2Funknown': { context_window: null },
  }
  let model = 'provider/small'
  mockProxy(on, { routes, model: () => model, env: { CLAUDE_CODE_MAX_CONTEXT_TOKENS: '256000', CLAUDE_CODE_AUTO_COMPACT_WINDOW: '110000' } })
  const writes: { name: string; value: unknown }[] = []
  on('env.set', ($, e) => { writes.push({ name: e.name, value: e.value }); return { value: undefined } })
  await start($)
  expect(writes.at(-1)).toEqual({ name: 'CLAUDE_CODE_MAX_CONTEXT_TOKENS', value: '128000' })
  for (const { selected, expected } of [{ selected: 'provider/medium', expected: '400000' }, { selected: 'provider/large', expected: '1048576' }, { selected: 'provider/unknown', expected: '256000' }]) {
    model = selected
    await $.prompt.submit({ text: 'hi', origin: { kind: 'composer' } } as never)
    expect(writes.at(-1)).toEqual({ name: 'CLAUDE_CODE_MAX_CONTEXT_TOKENS', value: expected })
  }
  expect(writes.every(write => write.name === 'CLAUDE_CODE_MAX_CONTEXT_TOKENS')).toBe(true)
})

test('an unavailable context lookup removes the previous model limit', async ($, on) => {
  let model = 'provider/small'
  let down = false
  mockProxy(on, { model: () => model, contextDown: () => down, routes: { ...ROUTES, 'GET /admin/model-context?model=provider%2Fsmall': { context_window: 128000 } } })
  const writes: unknown[] = []
  on('env.set', ($, e) => { writes.push(e.value); return { value: undefined } })
  await start($)
  expect(writes.at(-1)).toBe('128000')
  model = 'unknown'
  down = true
  await $.prompt.submit({ text: 'hi', origin: { kind: 'composer' } } as never)
  expect(writes.at(-1)).toBeUndefined()
})

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

test('/proxy model updates Claude Code and the displayed session without changing proxy defaults', { plugins: [STATUSLINE] }, async ($, on) => {
  const { calls, nativeCommands, clock } = mockProxy(on)
  const out = exported(on)
  await start($)
  await $.command.run(run(''))
  const pane = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await pane.press({ key: 'tab-Status' })
  expect(out.value?.values?.model).toBe('openai/gpt-x')
  const { text } = await $.command.run(run('model chatgpt/gpt-6-sol'))
  expect(text).toBe('Changing session model to chatgpt/gpt-6-sol.')
  await clock.advance(1)
  await clock.settle()
  expect(nativeCommands).toContain('model chatgpt/gpt-6-sol')
  expect(calls.some(call => call.startsWith('PUT /admin/model'))).toBe(false)
  expect(out.value?.values?.model).toBe('chatgpt/gpt-6-sol')
  expect(await pane.find({ text: 'chatgpt/gpt-6-sol' })).toBeDefined()
})

test('/proxy effort and model clear call the native session commands', { plugins: [STATUSLINE] }, async ($, on) => {
  const { calls, nativeCommands, clock } = mockProxy(on)
  const out = exported(on)
  await start($)
  await $.command.run(run(''))
  await $.command.run(run('effort medium'))
  await clock.advance(1)
  await clock.settle()
  expect(out.value?.values).toMatchObject({ effort: 'medium', effortSource: 'selection' })
  await $.command.run(run('effort clear'))
  await clock.advance(1)
  await clock.settle()
  expect(out.value?.values).toMatchObject({ effort: null, effortSource: null })
  await $.command.run(run('model clear'))
  await clock.advance(1)
  await clock.settle()
  expect(nativeCommands).toEqual(['effort medium', 'effort auto', 'model default'])
  expect(calls.some(call => call.startsWith('PUT /admin/'))).toBe(false)
  expect(out.value?.values).toMatchObject({ model: 'claude-x', effort: null, effortSource: null })
})

test('panel session controls call Claude Code while proxy defaults remain separate', { plugins: [STATUSLINE] }, async ($, on) => {
  const { calls, nativeCommands } = mockProxy(on)
  const out = exported(on)
  on('tool.call', { tool: 'AskUserQuestion' }, () => ({ result: { answers: { 'Choose a model': 'gpt-y', 'Choose session effort': 'medium' } } }))
  await start($)
  await $.command.run(run(''))
  const pane = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await pane.press({ key: 'tab-Config' })
  expect(await pane.find({ text: /Proxy defaults apply to new sessions/ })).toBeDefined()
  await pane.press({ key: 'session-model' })
  expect(nativeCommands).toContain('model gpt-y')
  expect(calls.some(call => call.startsWith('PUT /admin/model'))).toBe(false)
  expect(out.value?.values?.model).toBe('gpt-y')
  await pane.press({ key: 'session-effort' })
  expect(nativeCommands).toContain('effort medium')
  expect(out.value?.values).toMatchObject({ effort: 'medium', effortSource: 'selection' })
  expect(calls.some(call => call.startsWith('PUT /admin/effort'))).toBe(false)
})

test('explicit default commands update the proxy defaults', async ($, on) => {
  const { calls, nativeCommands } = mockProxy(on)
  await start($)
  await $.command.run(run('default effort medium'))
  await $.command.run(run('default model clear'))
  expect(calls).toContain('PUT /admin/effort {"effort":"medium"}')
  expect(calls).toContain('PUT /admin/model {"model":null}')
  expect(nativeCommands).toEqual([])
  const pane = await $.ui.mount({ ...PANE, surface: 'terminal' })
  await pane.press({ key: 'tab-Status' })
  expect(await pane.find({ text: 'medium' })).toBeDefined()
  expect(await pane.find({ text: '(first available)' })).toBeDefined()
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
  expect(await ui.find({ text: /Input token reuse/ })).toBeDefined()
  expect(await ui.find({ text: /60% reused.*600 read.*200 written.*200 uncached/ })).toBeDefined()
  expect(await ui.find({ text: /Cache telemetry unavailable/ })).toBeDefined()
  expect(await ui.find({ text: /Stable prompt prefix changed/ })).toBeDefined()
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

// What a status line mod would render: the last `$.statusline.publish` call.
function exported(on: On) {
  const box: { value?: any } = {}
  on('statusline.publish' as never, ($: any, e: any) => {
    box.value = { id: e.id, values: e.values }
    return { value: undefined }
  })
  return box
}

test('publishes status line data for a status line mod', { plugins: [STATUSLINE] }, async ($, on) => {
  mockProxy(on)
  const out = exported(on)
  await start($)
  await $.command.run(run(''))
  expect(out.value).toEqual({
    id: 'local-proxy',
    values: {
      model: 'openai/gpt-x', effort: null, effortSource: null, account: { provider: 'openai', alias: 'work' },
      contextPercent: 42, rate5h: 12, rateWeek: 40,
    },
  })
})

test('exported account prefers the session pin', { plugins: [STATUSLINE] }, async ($, on) => {
  mockProxy(on, { routes: { ...ROUTES, 'GET /admin/session/sess-1': { account: { openai: 'home' }, effort: null, stats: null } } })
  const out = exported(on)
  await start($)
  await $.command.run(run(''))
  expect(out.value?.values?.account).toEqual({ provider: 'openai', alias: 'home' })
})

for (const [why, opts] of [
  ['no ANTHROPIC_BASE_URL', { baseUrl: '' }],
  ['a remote base URL', { baseUrl: 'https://api.anthropic.com' }],
  ['a local gateway that is not local-proxy', { routes: {} }],
] as const) {
  test(`the mod stays inert with ${why}`, { plugins: [STATUSLINE] }, async ($, on) => {
    const { commands, calls } = mockProxy(on, opts)
    const out = exported(on)
    await start($)
    expect(commands).toEqual([])
    expect(out.value).toBeUndefined()
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

test('log tail wraps long lines and keeps the newest that fit', () => {
  expect(tailFit(['a', 'b'.repeat(25), 'c'], 4, 10)).toEqual(['b'.repeat(25), 'c'])
  expect(tailFit(['x'.repeat(100)], 2, 10)).toEqual(['x'.repeat(100)])
})


test('token reuse distinguishes missing telemetry from a reported zero', () => {
  expect(cacheTokenText(undefined)).toContain('unavailable')
  expect(cacheTokenText({ hit_requests: 0, reported_requests: 0, rate_percent: null, coverage_percent: 0,
    read_tokens: 0, write_tokens: 0, reported_input_tokens: 0, uncached_tokens: 0, read_percent: null })).toContain('n/a')
  expect(cacheTokenText({ hit_requests: 0, reported_requests: 1, rate_percent: 0, coverage_percent: 100,
    read_tokens: 0, write_tokens: 200, reported_input_tokens: 1000, uncached_tokens: 800, read_percent: 0 })).toBe('0% reused · 0 read · 200 written · 800 uncached')
})
