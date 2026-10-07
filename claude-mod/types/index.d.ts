export type ProxyStatus = { version: string; port: number; pid: number; model: string | null; effort: string | null }
export type ProxyAccount = { provider: string; alias: string; is_default: boolean }
export type ProxyRateLimits = { h5: number | null; week: number | null }
export type ProxyStatsRow = { provider?: string; requests: number; input_tokens: number; output_tokens: number; errors?: number; cost_usd: number }
export type ProxyStats = { summary: ProxyStatsRow; providers: ProxyStatsRow[] }
/** An admin call waiting on the person's confirmation. */
export type ProxyPending = { label: string; method: string; path: string; body?: unknown }

declare module 'claude-code' {
  interface PluginState {
    'local-proxy': {
      tab: string
      status: ProxyStatus | null
      accounts: ProxyAccount[]
      models: string[]
      /** provider -> alias pinned for this session */
      pins: Record<string, string>
      window: string
      stats: ProxyStats | null
      today: number | null
      rate: ProxyRateLimits | null
      logs: string[]
      confirm: ProxyPending | null
      error: string | null
    }
  }
}
