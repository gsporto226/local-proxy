import { atom, read, update } from 'claude-code'
import type { Elements, EngineInterface, Register } from 'claude-code'

import type { AccountUsageResult, ProxyAccount, ProxyPending, ProxyRateLimits, ProxyStats, ProxyStatus } from '../types'

type $ = EngineInterface

const PANE = 'proxy'
const TABS = ['Status', 'Accounts', 'Usage', 'Logs', 'Config'] as const
const KEY_COLS = 18
const BAR_COLS = 40

/** Block meter like Claude's /usage bars: eighths for a smooth edge. */
export function meter(pct: number, cols: number) {
  const eighths = Math.round((Math.min(100, Math.max(0, pct)) / 100) * cols * 8)
  const full = Math.floor(eighths / 8)
  const part = eighths % 8 ? ' ▏▎▍▌▋▊▉'[eighths % 8] : ''
  return '█'.repeat(full) + part + ' '.repeat(cols - full - (part ? 1 : 0))
}

/** Log text without ANSI escapes or other control characters: a Text refuses them. */
export const plain = (s: string) => s.replace(/\x1b\[[0-9;?]*[ -\/]*[@-~]/g, '').replace(/[\x00-\x08\x0b-\x1f\x7f]/g, '')

const compact = (n: number) => new Intl.NumberFormat('en', { notation: 'compact' }).format(n)
const resetTime = (raw: string | null) => {
  if (!raw) return undefined
  const timestamp = /^\d+$/.test(raw) ? Number(raw) * 1000 : Date.parse(raw)
  return Number.isFinite(timestamp) ? new Date(timestamp).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : undefined
}
const WINDOWS = ['day', 'week', 'month', 'all']
const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max']
export const SEGMENTS = ['model', 'effort', 'account', 'context', 'rate5h', 'rateWeek']

const tab = atom({ plugin: 'local-proxy', key: 'tab' } as const, 'Status')
const status = atom({ plugin: 'local-proxy', key: 'status' } as const, null)
const accounts = atom({ plugin: 'local-proxy', key: 'accounts' } as const, [])
const models = atom({ plugin: 'local-proxy', key: 'models' } as const, [])
const pins = atom({ plugin: 'local-proxy', key: 'pins' } as const, {})
const win = atom({ plugin: 'local-proxy', key: 'window' } as const, 'day')
const stats = atom({ plugin: 'local-proxy', key: 'stats' } as const, null)
const usage = atom({ plugin: 'local-proxy', key: 'usage' } as const, [])
const rate = atom({ plugin: 'local-proxy', key: 'rate' } as const, null)
const logs = atom({ plugin: 'local-proxy', key: 'logs' } as const, [])
const confirm = atom({ plugin: 'local-proxy', key: 'confirm' } as const, null)
const error = atom({ plugin: 'local-proxy', key: 'error' } as const, null)
const line = atom({ plugin: 'local-proxy', key: 'line' } as const, '')

// The proxy this session talks to: Claude's own base URL, when it is a loopback
// address (where `launch`/`serve` put it). Undefined means no proxy connection.
let origin: string | undefined

async function detect($: $) {
  const raw = await $.env.get('ANTHROPIC_BASE_URL')
  let url: URL
  try {
    url = new URL(raw ?? '')
  } catch {
    return undefined
  }
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)) return undefined
  // Only local-proxy answers /admin/status with a version; another local gateway does not.
  const r = await $.http.fetch(`${url.origin}/admin/status`, { method: 'GET' }).catch(() => null)
  return r?.ok && JSON.parse(r.text || '{}').version ? url.origin : undefined
}

async function base() {
  return origin ?? 'http://127.0.0.1:8787'
}

async function api<T>($: $, method: string, path: string, body?: unknown): Promise<T> {
  const r = await $.http.fetch(`${await base()}${path}`, {
    method,
    ...(body === undefined ? {} : { headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }),
  })
  const data = r.text ? JSON.parse(r.text) : null
  if (!r.ok) throw new Error(data?.error ?? `HTTP ${r.status}`)
  return data as T
}


// Every atom write redraws its readers; skip writes that would not change anything.
// (Atoms must be named at each read/update call, so this is a check, not a wrapper.)
const last = new Map<string, string>()
function changed(key: string, value: unknown) {
  const json = JSON.stringify(value)
  if (last.get(key) === json) return false
  last.set(key, json)
  return true
}

/**
 * Re-reads what the panel and status line show; marks the proxy down on failure.
 * `full` adds accounts and models (config + vault reads), which only change on
 * open or after a write, so request events skip them.
 */
async function refresh($: $, full = false) {
  try {
    const sid = await $.session.id()
    const [st, rl, s, sess, acc, mod, accountUsage] = await Promise.all([
      api<ProxyStatus>($, 'GET', '/admin/status'),
      api<ProxyRateLimits>($, 'GET', '/admin/rate-limits'),
      api<ProxyStats>($, 'GET', `/admin/stats?since=${await read($, win)}`),
      api<{ account: Record<string, string> | null }>($, 'GET', `/admin/session/${sid}`),
      full ? api<ProxyAccount[]>($, 'GET', '/admin/accounts') : undefined,
      full ? api<string[]>($, 'GET', '/admin/models') : undefined,
      full ? api<AccountUsageResult[]>($, 'GET', '/admin/account-usage') : undefined,
    ])
    if (changed('status', st)) await update($, status, () => st)
    if (changed('rate', rl)) await update($, rate, () => rl)
    if (changed('stats', s)) await update($, stats, () => s)
    if (changed('pins', sess.account ?? {})) await update($, pins, () => sess.account ?? {})
    if (acc && changed('accounts', acc)) await update($, accounts, () => acc)
    if (mod && changed('models', mod)) await update($, models, () => mod)
    if (accountUsage && changed('usage', accountUsage)) await update($, usage, () => accountUsage)
    await update($, error, () => null)
  } catch (err) {
    if (changed('status', null)) await update($, status, () => null)
    await update($, error, () => String(err))
  }
  await statusLine($)
}

// Request events arrive per proxied call (several per turn); coalesce them.
let pending: Promise<void> | undefined
function refreshSoon($: $) {
  pending ??= $.clock.sleep(1500).then(() => {
    pending = undefined
    return refresh($)
  })
  return pending
}

let segments: readonly string[] = SEGMENTS
// Terminal height, from the band's last drawing: the pane asks for 80% of it.
let screenRows = 40
// Set once the pane has held the keyboard; losing it after that closes the pane.
let hadFocus = false
let interactive = false

// Drawn as an AbovePrompt row: `$.ui.status` gets a `⚠ <plugin>:` prefix from the engine.
async function statusLine($: $) {
  const [u, st, rl, acc, pinned] = await Promise.all([
    $.session.usage(),
    read($, status),
    read($, rate),
    read($, accounts),
    read($, pins),
  ])
  const usageRate = (kind: string) => u.rateLimits.find(r => r.kind === kind)?.percentUsed
  const pct = (n: number | null | undefined) => (n == null ? undefined : `${Math.round(n)}%`)
  const model = st?.model ?? (await $.session.model())
  const provider = model?.split('/')[0]
  const alias = provider && (pinned[provider] ?? acc.find(a => a.provider === provider && a.is_default)?.alias)
  const parts: Record<string, string | undefined> = {
    model,
    effort: st?.effort ?? undefined,
    account: alias ? `${provider}/${alias}` : undefined,
    context: pct(u.context.percent) && `${pct(u.context.percent)} ctx`,
    rate5h: pct(rl?.h5 ?? usageRate('five_hour')) && `${pct(rl?.h5 ?? usageRate('five_hour'))} 5h`,
    rateWeek: pct(rl?.week ?? usageRate('seven_day')) && `${pct(rl?.week ?? usageRate('seven_day'))} wk`,
  }
  const text = segments.map(s => parts[s]).filter(Boolean).join(' · ')
  if (changed('line', text)) await update($, line, () => text)
}

// ponytail: the runtime has no streaming HTTP; SSE rides a `curl -sN` child via $.process.spawn.
let streaming = false
async function stream($: $) {
  if (streaming) return
  streaming = true
  try {
    let buf = ''
    for await (const { text } of $.process.spawn({ argv: ['curl', '-sN', `${await base()}/admin/events`] })) {
      buf += text.replace(/\r/g, '')
      let i
      while ((i = buf.indexOf('\n\n')) >= 0) {
        const block = buf.slice(0, i)
        buf = buf.slice(i + 2)
        const ev = /^event: ?(.*)$/m.exec(block)?.[1]
        const data = block.split('\n').filter(l => l.startsWith('data:')).map(l => l.slice(5).trimStart()).join('\n')
        if (data) await onEvent($, ev ?? 'message', JSON.parse(data))
      }
    }
  } catch {
    // curl missing or proxy gone: the tick retries
  } finally {
    streaming = false
  }
}

async function onEvent($: $, ev: string, data: any) {
  if (ev === 'log') return update($, logs, l => [...l, plain(String(data.line))].slice(-500))
  if (ev === 'request') return void refreshSoon($).catch(() => {})
  if (ev === 'config') await update($, status, s => (s ? { ...s, model: data.model, effort: data.effort } : s))
  else if (ev === 'rate_limits' && changed('rate', data)) await update($, rate, () => data as ProxyRateLimits)
  await statusLine($)
}

async function tick($: $) {
  if (streaming) return
  // Proxy down: one cheap status probe per tick; the full read happens once it is up.
  await refresh($)
  if (await read($, status)) {
    await refresh($, true)
    const l = await api<{ lines: string[] }>($, 'GET', '/admin/logs?n=200').catch(() => null)
    if (l?.lines) await update($, logs, () => l.lines.map(plain))
    void stream($)
  }
}

/** Runs an admin call from a press, then re-reads; errors land in the panel. */
async function act($: $, method: string, path: string, body?: unknown) {
  try {
    await api($, method, path, body)
  } catch (err) {
    await update($, error, () => String(err))
    return
  }
  await refresh($, true)
}

const ask = ($: $, p: ProxyPending) => update($, confirm, () => p)

/** Uses Claude Code's question dialog for a menu with any number of choices. */
async function choose($: $, question: string, choices: readonly string[]): Promise<string | undefined> {
  if (choices.length <= 2) {
    const answer = await $.ui.ask(question, choices.length ? choices : ['Cancel', 'Back']).catch(() => '')
    return choices.includes(answer) ? answer : undefined
  }
  // AskUserQuestion caps each page at four choices. Reserve one for More/Done.
  for (let start = 0; start < choices.length; start += 3) {
    const page = choices.slice(start, start + 3)
    const hasMore = start + 3 < choices.length
    const answer = await $.ui.ask(question, [...page, hasMore ? 'More…' : 'Cancel']).catch(() => '')
    if (page.includes(answer)) return answer
    if (!hasMore || answer !== 'More…') return undefined
  }
  return undefined
}

export const register: Register = (on, options) => {
  segments = (options.segments as readonly string[] | undefined) ?? SEGMENTS

  on('session.start', async ($, e, next) => {
    // Without a proxy connection the mod stays inert: no command, no row, no polling.
    origin = await detect($)
    if (!origin) return next(e)
    await $.command.register({
      name: 'proxy',
      description: 'local-proxy: no args opens the panel; otherwise runs the CLI (e.g. /proxy account)',
      immediate: true,
    })
    // `-p` and SDK runs draw nothing (no panel, no status row): polling the
    // proxy there only delays their exit (e2e/claude-mod.test.ts pins this).
    interactive = e.isInteractive
    if (interactive) {
      // Background work: a failure (proxy gone, module reloading) waits for the next tick.
      const quietTick = () => void tick($).catch(() => {})
      quietTick()
      $.clock.every(5000, quietTick)
    }

    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if (origin && interactive) await statusLine($)
    return next(e)
  })

  on('prompt.submit', async ($, e, next) => {
    if (origin) {
      hadFocus = false
      await $.ui.close({ id: PANE })
    }
    return next(e)
  })

  on('command.run', { command: 'proxy' }, async ($, e) => {
    // ponytail: whitespace split, no quoting; port exec::parse_args if quoted args show up
    const args = e.args.trim().split(/\s+/).filter(Boolean)
    if (args.length === 0) {
      await refresh($, true)
      // Focused, so digits and Enter go to the panel, not the prompt; Esc closes it.
      hadFocus = false
      await $.ui.open({ id: PANE, title: 'local-proxy', focus: true, closeOnEscape: true, rows: Math.floor(screenRows * 0.8) })
      return {}
    }
    try {
      if ((args[0] === 'model' || args[0] === 'effort') && args.length === 2) {
        const field = args[0]
        const result = await api<{ message: string }>($, 'PUT', `/admin/${field}`, { [field]: args[1] === 'clear' ? null : args[1] })
        await refresh($, true)
        return { text: result.message }
      }
      const { exitCode, stdout, stderr } = await $.process.run(['local-proxy', ...args], { timeoutMs: 60_000 })
      const out = [stdout.trimEnd(), stderr.trimEnd()].filter(Boolean).join('\n')
      return { text: exitCode === 0 ? out || '(no output)' : `${out}\n(exit ${exitCode})` }
    } catch (err) {
      return { text: `local-proxy failed to run: ${String(err)}` }
    }
  })

  // The status row, in the band above the prompt.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (e.viewport) screenRows = e.viewport.rows
    if (!origin || e.props.hasSurvey) return next(e)
    const text = await read($, line)
    if (!text) return next(e)
    const { Text } = $.ui.resolve(e)
    return <Text dimColor>{text}</Text>
  })

  // The panel: a pane opened focused by `/proxy`, closed by Esc or the next prompt.
  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    // Like a popup: clicking or ctrl+x tab away from it closes it.
    if (e.props.isFocused) hadFocus = true
    else if (hadFocus) {
      hadFocus = false
      void $.ui.close({ id: PANE }).catch(() => {})
    }
    // Mobile has no Select; the engine draws an omitted element as a fragment.
    const { Box, Text, Button, Select } = $.ui.resolve(e) as Elements['terminal']
    const [current, st, err, pending] = await Promise.all([read($, tab), read($, status), read($, error), read($, confirm)])
    const rows = Math.max(3, e.props.scroll.bodyRows - 6)

    // Mirrors Claude Code's own /status dialog: title, inverse active tab, dim siblings.
    const header = (
      <Box flexDirection="row" gap={2}>
        <Text bold color="claude">local-proxy</Text>
        {TABS.map((t, i) =>
          t === current ? (
            <Text key={`tab-${t}`} inverse bold>{` ${t} `}</Text>
          ) : (
            <Button key={`tab-${t}`} label={t} plain dimColor hotkey={String(i + 1)} onPress={() => update($, tab, () => t)} />
          ),
        )}
      </Box>
    )
    const kv = (k: string, v: unknown) => (
      <Box key={`kv-${k}`} flexDirection="row">
        <Box width={KEY_COLS}>
          <Text dimColor>{k}</Text>
        </Box>
        <Text>{v as string}</Text>
      </Box>
    )
    const bar = (label: string, p: number | null | undefined) => (
      <Box key={`bar-${label}`} flexDirection="column">
        <Text bold>{label}</Text>
        {p == null ? (
          <Text dimColor>no data yet</Text>
        ) : (
          <Text>
            <Text color={p >= 90 ? 'error' : p >= 70 ? 'warning' : 'claude'}>{meter(p, BAR_COLS)}</Text>
            {'  '}
            {Math.round(p)}% used
          </Text>
        )}
      </Box>
    )

    let body
    if (pending) {
      body = (
        <Box flexDirection="column" gap={0}>
          <Text bold>{pending.label}?</Text>
          <Box flexDirection="row" gap={2}>
            <Button
              key="confirm-yes"
              label="Confirm"
              variant="primary"
              onPress={async () => {
                await update($, confirm, () => null)
                await act($, pending.method, pending.path, pending.body)
                await $.ui.invalidate('ui.render')
              }}
            />
            <Button key="confirm-no" label="Cancel" autoFocus onPress={() => update($, confirm, () => null)} />
          </Box>
        </Box>
      )
    } else if (current === 'Status') {
      const [acc, pinned] = await Promise.all([read($, accounts), read($, pins)])
      const active = Object.entries(pinned).map(([p, a]) => `${p}/${a} (pinned)`)
      const defaults = acc.filter(a => a.is_default && !pinned[a.provider]).map(a => `${a.provider}/${a.alias}`)
      body = st ? (
        <Box flexDirection="column">
          <Button key="refresh" label="Refresh" plain autoFocus onPress={() => tick($)} />
          {kv('Proxy', <Text color="success">● running</Text>)}
          {kv('Version', st.version)}
          {kv('Address', `127.0.0.1:${st.port}`)}
          {kv('PID', st.pid)}
          {kv('Model', st.model ?? '(default)')}
          {kv('Effort', st.effort ?? '(default)')}
          {kv('Accounts', [...active, ...defaults].join(', ') || '(none)')}
        </Box>
      ) : (
        <Box flexDirection="column" gap={0}>
          <Text>
            <Text color="error">● down</Text> <Text dimColor>{err ?? ''}</Text>
          </Text>
          <Button
            key="start"
            label="Start proxy"
            variant="primary"
            onPress={async () => {
              const r = await $.process.run(['local-proxy', 'serve', '--background'], { timeoutMs: 30_000 }).catch(x => ({
                exitCode: 1,
                stderr: String(x),
              }))
              if (r.exitCode !== 0) await update($, error, () => r.stderr.trim() || `exit ${r.exitCode}`)
              await tick($)
            }}
          />
        </Box>
      )
    } else if (current === 'Accounts') {
      const [acc, pinned, sid, quota] = await Promise.all([
        read($, accounts),
        read($, pins),
        $.session.id(),
        read($, usage),
      ])
      body = (
        <Box flexDirection="column">
          <Text dimColor>Pins apply to this session only.</Text>
          <Button key="usage-refresh" label="Refresh quotas" plain onPress={() => refresh($, true)} />
          {acc.length === 0 && <Text dimColor>No accounts. Run `local-proxy connect` in a terminal.</Text>}
          {acc.map(a => {
            const isPinned = pinned[a.provider] === a.alias
            const id = `${a.provider}/${a.alias}`
            const accountUsage = quota.find(entry =>
              entry.kind === 'available'
                ? entry.usage.provider === a.provider && entry.usage.alias === a.alias
                : entry.provider === a.provider && entry.alias === a.alias,
            )
            return (
              <Box key={`acc-${id}`} flexDirection="column">
                <Box flexDirection="row" gap={1}>
                  <Text>
                    {isPinned ? '◆' : a.is_default ? '★' : ' '} {id}
                    {a.is_default ? <Text dimColor> default</Text> : ''}
                  </Text>
                  <Button
                    key={`pin-${id}`}
                    label={isPinned ? 'Unpin' : 'Pin'}
                    plain
                    autoFocus
                    onPress={() =>
                      isPinned
                        ? act($, 'DELETE', `/admin/session/${sid}/account/${encodeURIComponent(a.provider)}`)
                        : act($, 'PUT', `/admin/session/${sid}/account`, { provider: a.provider, alias: a.alias })
                    }
                  />
                </Box>
                {accountUsage?.kind === 'available' ? (
                  <Box flexDirection="column">
                    {accountUsage.usage.five_hour && (
                      <Box flexDirection="column">
                        {bar('Current session (5h)', accountUsage.usage.five_hour.utilization)}
                        {resetTime(accountUsage.usage.five_hour.resets_at) && (
                          <Text dimColor>5h resets {resetTime(accountUsage.usage.five_hour.resets_at)}</Text>
                        )}
                      </Box>
                    )}
                    {accountUsage.usage.seven_day && (
                      <Box flexDirection="column">
                        {bar('Current week (7d)', accountUsage.usage.seven_day.utilization)}
                        {resetTime(accountUsage.usage.seven_day.resets_at) && (
                          <Text dimColor>7d resets {resetTime(accountUsage.usage.seven_day.resets_at)}</Text>
                        )}
                      </Box>
                    )}
                    {accountUsage.usage.monthly && bar('Monthly extra usage', accountUsage.usage.monthly.utilization)}
                    <Text dimColor>Updated {new Date(accountUsage.usage.fetched_at * 1000).toLocaleTimeString()}</Text>
                    {accountUsage.stale && <Text dimColor>{accountUsage.error ?? 'Using cached quota'}</Text>}
                  </Box>
                ) : accountUsage?.kind === 'unavailable' ? (
                  <Text dimColor>{accountUsage.error}</Text>
                ) : (
                  <Text dimColor>Quota not available for this provider</Text>
                )}
              </Box>
            )
          })}
        </Box>
      )
    } else if (current === 'Usage') {
      const [s, w, rl] = await Promise.all([read($, stats), read($, win), read($, rate)])
      const row = (r: { requests: number; input_tokens: number; output_tokens: number; cost_usd: number }) =>
        `${r.requests} req · ${compact(r.input_tokens)} in · ${compact(r.output_tokens)} out · $${r.cost_usd.toFixed(2)}`
      body = (
        <Box flexDirection="column" gap={0}>
          {bar('Current session (5h)', rl?.h5)}
          {bar('Current week', rl?.week)}
          <Box flexDirection="column" gap={0}>
            <Text bold>Window</Text>
            <Box flexDirection="row" gap={1}>
              {WINDOWS.map((v, i) => (
                <Button
                  key={`window-${v}`}
                  label={v}
                  plain
                  dimColor={w !== v}
                  autoFocus={w === v ? true : undefined}
                  onPress={async () => {
                    await update($, win, () => v)
                    await refresh($)
                  }}
                />
              ))}
            </Box>
          </Box>
          <Box flexDirection="column">
            {s ? kv('Total', row(s.summary)) : <Text dimColor>No stats.</Text>}
            {s?.providers.map(p => kv(p.provider ?? '?', row(p)))}
          </Box>
        </Box>
      )
    } else if (current === 'Logs') {
      const l = await read($, logs)
      body = (
        <Box flexDirection="column">
          {l.length === 0 && <Text dimColor>No log lines.</Text>}
          {l.slice(-rows).map(line => <Text wrap="truncate-end">{line}</Text>)}
        </Box>
      )
    } else {
      const [acc, mods] = await Promise.all([read($, accounts), read($, models)])
      const def = acc.find(a => a.is_default)
      const modelChoices = ['(clear)', ...mods]
      body = (
        <Box flexDirection="column" gap={0}>
          <Button key="model" label={`Model: ${st?.model ?? '(default)'}`} autoFocus onPress={async () => {
            const selected = await choose($, 'Choose a model', modelChoices)
            if (selected === '(clear)') await ask($, { label: 'Clear model', method: 'PUT', path: '/admin/model', body: { model: null } })
            else if (selected) await act($, 'PUT', '/admin/model', { model: selected })
            await $.ui.invalidate('ui.render')
          }} />
          <Button key="effort" label={`Effort: ${st?.effort ?? '(default)'}`} onPress={async () => {
            const selected = await choose($, 'Choose effort', ['(clear)', ...EFFORTS])
            if (selected === '(clear)') await ask($, { label: 'Clear effort', method: 'PUT', path: '/admin/effort', body: { effort: null } })
            else if (selected) await act($, 'PUT', '/admin/effort', { effort: selected })
            await $.ui.invalidate('ui.render')
          }} />
          <Button key="default" label={`Default account: ${def ? `${def.provider}/${def.alias}` : '(none)'}`} onPress={async () => {
            const selected = await choose($, 'Choose the default account', acc.map(a => `${a.provider}/${a.alias}`))
            if (selected) {
              const [provider, alias] = selected.split('/')
              await act($, 'PUT', '/admin/account', { provider, alias })
              await $.ui.invalidate('ui.render')
            }
          }} />
          <Box flexDirection="column">
            {acc.map((a, i) => (
              <Box key={`dc-row-${a.provider}/${a.alias}`} flexDirection="row" gap={1}>
                <Text>{a.provider}/{a.alias}</Text>
                <Button
                  key={`dc-${a.provider}/${a.alias}`}
                  label="Disconnect"
                  plain
                  autoFocus
                  onPress={() =>
                    ask($, {
                      label: `Disconnect ${a.provider}/${a.alias}`,
                      method: 'DELETE',
                      path: `/admin/accounts/${encodeURIComponent(a.provider)}/${encodeURIComponent(a.alias)}`,
                    })
                  }
                />
              </Box>
            ))}
          </Box>
        </Box>
      )
    }

    return (
      <Box flexDirection="column" gap={0} paddingX={1}>
        {header}
        {body}
        {err && st && <Text color="error">{err}</Text>}
        <Text dimColor>
          {e.props.isFocused ? '1–5 tabs · tab/shift+tab move · enter select · esc close' : 'ctrl+x tab to focus · esc or your next prompt closes'}
        </Text>
      </Box>
    )
  })
}
