export type ProxyStatus = { version: string; port: number; pid: number; model: string | null; effort: string | null }
export type ProxyAccount = { provider: string; alias: string; is_default: boolean }
export type ProxyRateLimits = { h5: number | null; week: number | null }
export type ProxyStatsRow = { provider?: string; requests: number; input_tokens: number; output_tokens: number; errors?: number; cost_usd: number }
export type ProxyAccountStats = { provider: string; alias: string; requests: number; input_tokens: number; output_tokens: number; cost_usd: number }
export type ProxyStats = { summary: ProxyStatsRow; providers: ProxyStatsRow[]; accounts: ProxyAccountStats[] }
export type UsageWindow = { utilization: number; resets_at: string | null }
export type AccountUsage = {
  provider: string
  alias: string
  five_hour: UsageWindow | null
  seven_day: UsageWindow | null
  monthly: UsageWindow | null
  fetched_at: number
}
export type AccountUsageResult =
  | { kind: 'available'; usage: AccountUsage; stale: boolean; error: string | null }
  | { kind: 'unavailable'; provider: string; alias: string; error: string }
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
      usage: AccountUsageResult[]
      rate: ProxyRateLimits | null
      logs: string[]
      confirm: ProxyPending | null
      error: string | null
      /** the status row drawn above the prompt */
      line: string
    }
  }
}
