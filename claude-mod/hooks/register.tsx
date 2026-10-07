import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { ProxyAccount, ProxyPending, ProxyRateLimits, ProxyStats, ProxyStatus } from '../types'

type $ = EngineInterface

const PANE = 'proxy'
const TABS = ['Overview', 'Accounts', 'Usage', 'Logs', 'Config'] as const
const WINDOWS = ['day', 'week', 'month', 'all']
const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max']
export const SEGMENTS = ['model', 'context', 'rate5h', 'rateWeek', 'sessionCost', 'todayCost']

const tab = atom({ plugin: 'local-proxy', key: 'tab' } as const, 'Overview')
const status = atom({ plugin: 'local-proxy', key: 'status' } as const, null)
const accounts = atom({ plugin: 'local-proxy', key: 'accounts' } as const, [])
const models = atom({ plugin: 'local-proxy', key: 'models' } as const, [])
const pins = atom({ plugin: 'local-proxy', key: 'pins' } as const, {})
const win = atom({ plugin: 'local-proxy', key: 'window' } as const, 'day')
const stats = atom({ plugin: 'local-proxy', key: 'stats' } as const, null)
const today = atom({ plugin: 'local-proxy', key: 'today' } as const, null)
const rate = atom({ plugin: 'local-proxy', key: 'rate' } as const, null)
const logs = atom({ plugin: 'local-proxy', key: 'logs' } as const, [])
const confirm = atom({ plugin: 'local-proxy', key: 'confirm' } as const, null)
const error = atom({ plugin: 'local-proxy', key: 'error' } as const, null)

async function base($: $) {
  return `http://127.0.0.1:${(await $.env.get('LOCAL_PROXY_PORT')) ?? '8787'}`
}

async function api<T>($: $, method: string, path: string, body?: unknown): Promise<T> {
  const r = await $.http.fetch(`${await base($)}${path}`, {
    method,
    ...(body === undefined ? {} : { headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }),
  })
  const data = r.text ? JSON.parse(r.text) : null
  if (!r.ok) throw new Error(data?.error ?? `HTTP ${r.status}`)
  return data as T
}


/** Re-reads everything the panel and status line show; marks the proxy down on failure. */
async function refresh($: $) {
  try {
    const sid = await $.session.id()
    const [st, acc, mod, rl, s, d, sess] = await Promise.all([
      api<ProxyStatus>($, 'GET', '/admin/status'),
      api<ProxyAccount[]>($, 'GET', '/admin/accounts'),
      api<string[]>($, 'GET', '/admin/models'),
      api<ProxyRateLimits>($, 'GET', '/admin/rate-limits'),
      api<ProxyStats>($, 'GET', `/admin/stats?since=${await read($, win)}`),
      api<ProxyStats>($, 'GET', '/admin/stats?since=day'),
      api<{ account: Record<string, string> | null }>($, 'GET', `/admin/session/${sid}`),
    ])
    await update($, status, () => st)
    await update($, accounts, () => acc)
    await update($, models, () => mod)
    await update($, rate, () => rl)
    await update($, stats, () => s)
    await update($, today, () => d.summary.cost_usd)
    await update($, pins, () => sess.account ?? {})
    await update($, error, () => null)
  } catch (err) {
    await update($, status, () => null)
    await update($, error, () => String(err))
  }
  await statusLine($)
}

let segments: readonly string[] = SEGMENTS

async function statusLine($: $) {
  const [u, st, rl, t] = await Promise.all([$.session.usage(), read($, status), read($, rate), read($, today)])
  const usageRate = (kind: string) => u.rateLimits.find(r => r.kind === kind)?.percentUsed
  const pct = (n: number | null | undefined) => (n == null ? undefined : `${Math.round(n)}%`)
  const usd = (n: number | null | undefined) => (n == null ? undefined : `$${n.toFixed(2)}`)
  const parts: Record<string, string | undefined> = {
    model: st?.model ?? (await $.session.model()),
    context: pct(u.context.percent) && `ctx ${pct(u.context.percent)}`,
    rate5h: pct(rl?.h5 ?? usageRate('five_hour')) && `5h ${pct(rl?.h5 ?? usageRate('five_hour'))}`,
    rateWeek: pct(rl?.week ?? usageRate('seven_day')) && `wk ${pct(rl?.week ?? usageRate('seven_day'))}`,
    sessionCost: usd(u.cost?.usd) && `session ${usd(u.cost?.usd)}`,
    todayCost: usd(t) && `today ${usd(t)}`,
  }
  const line = segments.map(s => parts[s]).filter(Boolean).join(' · ')
  $.ui.status(line || undefined)
}

// ponytail: the runtime has no streaming HTTP; SSE rides a `curl -sN` child via $.process.spawn.
let streaming = false
async function stream($: $) {
  if (streaming) return
  streaming = true
  try {
    let buf = ''
    for await (const { text } of $.process.spawn({ argv: ['curl', '-sN', `${await base($)}/admin/events`] })) {
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
  if (ev === 'log') await update($, logs, l => [...l, String(data.line)].slice(-500))
  else if (ev === 'config') await update($, status, s => (s ? { ...s, model: data.model, effort: data.effort } : s))
  else if (ev === 'rate_limits') await update($, rate, () => data as ProxyRateLimits)
  else if (ev === 'request') return refresh($)
  await statusLine($)
}

async function tick($: $) {
  if (streaming) return
  await refresh($)
  if (await read($, status)) {
    const l = await api<{ lines: string[] }>($, 'GET', '/admin/logs?n=200').catch(() => null)
    if (l?.lines) await update($, logs, () => l.lines)
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
  await refresh($)
}

const ask = ($: $, p: ProxyPending) => update($, confirm, () => p)

export const register: Register = (on, options) => {
  segments = (options.segments as readonly string[] | undefined) ?? SEGMENTS

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'proxy',
      description: 'local-proxy: no args opens the panel; otherwise runs the CLI (e.g. /proxy account)',
    })
    void tick($)
    $.clock.every(5000, () => void tick($))

    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    await statusLine($)
    return next(e)
  })

  on('command.run', { command: 'proxy' }, async ($, e) => {
    // ponytail: whitespace split, no quoting; port exec::parse_args if quoted args show up
    const args = e.args.trim().split(/\s+/).filter(Boolean)
    if (args.length === 0) {
      await refresh($)
      await $.ui.open({ id: PANE, title: 'local-proxy', focus: true })
      return { text: 'local-proxy panel opened.' }
    }
    try {
      const { exitCode, stdout, stderr } = await $.process.run(['local-proxy', ...args], { timeoutMs: 60_000 })
      const out = [stdout.trimEnd(), stderr.trimEnd()].filter(Boolean).join('\n')
      return { text: exitCode === 0 ? out || '(no output)' : `${out}\n(exit ${exitCode})` }
    } catch (err) {
      return { text: `local-proxy failed to run: ${String(err)}` }
    }
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button, Select } = $.ui.resolve(e)
    const [current, st, err, pending] = await Promise.all([read($, tab), read($, status), read($, error), read($, confirm)])
    const rows = Math.max(3, (e.viewport?.rows ?? 24) - 8)

    const header = (
      <Box flexDirection="row" gap={1}>
        {TABS.map((t, i) => (
          <Button
            key={`tab-${t}`}
            label={t}
            plain
            hotkey={String(i + 1)}
            variant={t === current ? 'primary' : undefined}
            dimColor={t !== current}
            onPress={() => update($, tab, () => t)}
          />
        ))}
      </Box>
    )

    let body
    if (pending) {
      body = (
        <Box flexDirection="column" gap={1}>
          <Text bold>{pending.label}?</Text>
          <Box flexDirection="row" gap={2}>
            <Button
              key="confirm-yes"
              label="Confirm"
              variant="primary"
              onPress={async () => {
                await update($, confirm, () => null)
                await act($, pending.method, pending.path, pending.body)
              }}
            />
            <Button key="confirm-no" label="Cancel" onPress={() => update($, confirm, () => null)} />
          </Box>
        </Box>
      )
    } else if (current === 'Overview') {
      body = st ? (
        <Box flexDirection="column">
          <Text>
            <Text color="green">● up</Text> v{st.version} · port {st.port} · pid {st.pid}
          </Text>
          <Text>model   <Text bold>{st.model ?? '(default)'}</Text></Text>
          <Text>effort  <Text bold>{st.effort ?? '(default)'}</Text></Text>
        </Box>
      ) : (
        <Box flexDirection="column" gap={1}>
          <Text>
            <Text color="red">● down</Text> <Text dimColor>{err ?? ''}</Text>
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
      const [acc, pinned, sid] = await Promise.all([read($, accounts), read($, pins), $.session.id()])
      body = (
        <Box flexDirection="column">
          <Text dimColor>Pins apply to this session only.</Text>
          {acc.length === 0 && <Text dimColor>No accounts. Run `local-proxy connect` in a terminal.</Text>}
          {acc.map(a => {
            const isPinned = pinned[a.provider] === a.alias
            const id = `${a.provider}/${a.alias}`
            return (
              <Box key={`acc-${id}`} flexDirection="row" gap={1}>
                <Text>
                  {isPinned ? '◆' : a.is_default ? '★' : ' '} {id}
                  {a.is_default ? <Text dimColor> default</Text> : ''}
                </Text>
                <Button
                  key={`pin-${id}`}
                  label={isPinned ? 'Unpin' : 'Pin'}
                  plain
                  onPress={() =>
                    isPinned
                      ? act($, 'DELETE', `/admin/session/${sid}/account/${encodeURIComponent(a.provider)}`)
                      : act($, 'PUT', `/admin/session/${sid}/account`, { provider: a.provider, alias: a.alias })
                  }
                />
              </Box>
            )
          })}
        </Box>
      )
    } else if (current === 'Usage') {
      const [s, w, rl] = await Promise.all([read($, stats), read($, win), read($, rate)])
      const row = (r: { requests: number; input_tokens: number; output_tokens: number; cost_usd: number }) =>
        `${r.requests} req · in ${r.input_tokens} · out ${r.output_tokens} · $${r.cost_usd.toFixed(4)}`
      body = (
        <Box flexDirection="column">
          <Select
            key="window"
            label="Window"
            value={w}
            options={WINDOWS.map(v => ({ value: v, label: v }))}
            onSelect={async v => {
              await update($, win, () => v)
              await refresh($)
            }}
          />
          {s ? <Text bold>{row(s.summary)}</Text> : <Text dimColor>No stats.</Text>}
          {s?.providers.map(p => <Text key={`prov-${p.provider}`}>  {p.provider}: {row(p)}</Text>)}
          <Text>
            Rate limits: 5h {rl?.h5 == null ? '–' : `${rl.h5}%`} · week {rl?.week == null ? '–' : `${rl.week}%`}
          </Text>
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
      body = (
        <Box flexDirection="column" gap={1}>
          <Select
            key="model"
            label="Model"
            value={st?.model ?? ''}
            options={[{ value: '', label: '(clear)' }, ...mods.map(m => ({ value: m, label: m }))]}
            onSelect={v =>
              v ? act($, 'PUT', '/admin/model', { model: v }) : ask($, { label: 'Clear model', method: 'PUT', path: '/admin/model', body: { model: null } })
            }
          />
          <Select
            key="effort"
            label="Effort"
            value={st?.effort ?? ''}
            options={[{ value: '', label: '(clear)' }, ...EFFORTS.map(v => ({ value: v, label: v }))]}
            onSelect={v =>
              v ? act($, 'PUT', '/admin/effort', { effort: v }) : ask($, { label: 'Clear effort', method: 'PUT', path: '/admin/effort', body: { effort: null } })
            }
          />
          <Select
            key="default"
            label="Default account"
            value={def ? `${def.provider}/${def.alias}` : ''}
            options={acc.map(a => ({ value: `${a.provider}/${a.alias}`, label: `${a.provider}/${a.alias}` }))}
            onSelect={v => {
              const [provider, alias] = v.split('/')
              return act($, 'PUT', '/admin/account', { provider, alias })
            }}
          />
          <Box flexDirection="column">
            {acc.map(a => (
              <Box key={`dc-row-${a.provider}/${a.alias}`} flexDirection="row" gap={1}>
                <Text>{a.provider}/{a.alias}</Text>
                <Button
                  key={`dc-${a.provider}/${a.alias}`}
                  label="Disconnect"
                  plain
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
      <Box flexDirection="column" gap={1}>
        {header}
        {body}
        {err && st && <Text color="red">{err}</Text>}
      </Box>
    )
  })
}
