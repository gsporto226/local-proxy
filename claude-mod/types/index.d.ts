export type ProxyStatus = { version: string; port: number; pid: number; model: string | null; effort: string | null }
export type SessionEffort = { model: string; value: string; source: 'selection' | 'request' } | null
export type ProxyAccount = { provider: string; alias: string; is_default: boolean }
export type ProxyRateLimits = { h5: number | null; week: number | null }
export type ProxyStatsWindow = 'session' | 'day' | 'week' | 'month' | 'all'
export type ProxyStatsScope =
  | { kind: 'all' }
  | { kind: 'session'; session_id: string }
export type ProxyCacheStats = {
  hit_requests: number
  reported_requests: number
  rate_percent: number | null
  coverage_percent: number | null
  read_tokens?: number
  write_tokens?: number
  reported_input_tokens?: number
  uncached_tokens?: number
  read_percent?: number | null
}
export type ProxyStatsRow = {
  provider?: string
  requests: number
  input_tokens: number
  output_tokens: number
  errors?: number
  cost_usd: number
  cache?: ProxyCacheStats
}
export type ProxyAccountStats = {
  provider: string
  alias: string
  requests: number
  input_tokens: number
  output_tokens: number
  cost_usd: number
  cache?: ProxyCacheStats
}
export type CacheDiagnostic =
  | "missing_cache_control" | "missing_session_key" | "stable_prefix_changed"
  | "model_changed" | "account_changed" | "cache_key_changed"
  | "telemetry_unavailable" | "cache_miss_unknown" | "cache_warming" | "upstream_error"
export type ProxyRecentRequest = {
  ts: number
  provider: string
  alias: string
  model: string
  cache_read_tokens?: number | null
  cache_write_tokens?: number
  cache_diagnostics?: CacheDiagnostic[]
}
export type ProxyStats = {
  scope?: ProxyStatsScope
  summary: ProxyStatsRow
  providers: ProxyStatsRow[]
  accounts: ProxyAccountStats[]
  recent?: ProxyRecentRequest[]
}
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
/** What a status line mod renders; the values are published under `id: 'local-proxy'` via `$.statusline.publish`. */
export type ProxyStatusline = {
  model: string | null
  effort: string | null
  effortSource: 'selection' | 'request' | null
  /** the session pin, else the provider's default account */
  account: { provider: string; alias: string } | null
  contextPercent: number | null
  rate5h: number | null
  rateWeek: number | null
}
/** An admin call waiting on the person's confirmation. */
export type ProxyPending = { label: string; method: string; path: string; body?: unknown }

/** The noun a status line mod adds to `$` in engine.create. */
export type Statusline = {
  publish: (args: { id: string; values: Record<string, unknown> }) => void
}

declare module 'claude-code' {
  interface EngineInterface {
    statusline: Statusline
  }
  interface PluginState {
    'local-proxy': {
      tab: string
      status: ProxyStatus | null
      sessionEffort: SessionEffort
      accounts: ProxyAccount[]
      models: string[]
      /** provider -> alias pinned for this session */
      pins: Record<string, string>
      window: ProxyStatsWindow
      stats: ProxyStats | null
      accountFilter: string
      usage: AccountUsageResult[]
      rate: ProxyRateLimits | null
      logs: string[]
      confirm: ProxyPending | null
      error: string | null
    }
  }
}
