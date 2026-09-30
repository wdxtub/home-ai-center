/** 管理面 API 客户端。
 *
 * 令牌存在内存 + sessionStorage，不放 localStorage：
 * 关掉标签页就没了，比长期留在磁盘上安全一点（家里多人共用机器的场景）。
 */

const TOKEN_KEY = 'hac_admin_token'

export function getToken(): string | null {
  return sessionStorage.getItem(TOKEN_KEY)
}

export function setToken(t: string | null) {
  if (t) sessionStorage.setItem(TOKEN_KEY, t)
  else sessionStorage.removeItem(TOKEN_KEY)
}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
  }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    ...((init.headers as Record<string, string>) ?? {}),
  }
  const token = getToken()
  if (token) headers['x-admin-token'] = token

  const res = await fetch(`/api/admin${path}`, { ...init, headers })
  if (res.status === 401) {
    setToken(null)
    throw new ApiError('会话已过期，请重新登录', 401)
  }
  const text = await res.text()
  const body = text ? safeJson(text) : null
  if (!res.ok) {
    throw new ApiError(extractMessage(body) ?? `请求失败（${res.status}）`, res.status)
  }
  return body as T
}

function safeJson(t: string): unknown {
  try {
    return JSON.parse(t)
  } catch {
    return { error: { message: t } }
  }
}

function extractMessage(body: unknown): string | null {
  if (!body || typeof body !== 'object') return null
  const b = body as Record<string, unknown>
  const err = b.error
  if (typeof err === 'string') return err
  if (err && typeof err === 'object') {
    const m = (err as Record<string, unknown>).message
    if (typeof m === 'string') return m
  }
  if (typeof b.message === 'string') return b.message
  return null
}

export const api = {
  get: <T,>(p: string) => request<T>(p),
  post: <T,>(p: string, body?: unknown) =>
    request<T>(p, { method: 'POST', body: JSON.stringify(body ?? {}) }),
  put: <T,>(p: string, body?: unknown) =>
    request<T>(p, { method: 'PUT', body: JSON.stringify(body ?? {}) }),
  del: <T,>(p: string) => request<T>(p, { method: 'DELETE' }),
}

// ── 类型 ────────────────────────────────────────────────────────────────────

export interface Account {
  id: number
  name: string
  api_key_prefix: string
  enabled: boolean
  balance_micro: number
  held_micro: number
  available_micro: number
  llm_max_concurrency: number
  llm_max_queue: number
  image_max_concurrency: number
  image_max_queue: number
  rpm_limit: number | null
  created_at: number
}

export interface NodeKey {
  id: number
  node_id: number
  label: string
  masked: string
  sort_order: number
  enabled: boolean
  state: 'active' | 'cooling' | 'quarantined' | 'disabled'
  cooldown_until: number | null
  cooldown_remaining: number | null
  reset_source: string | null
  quota_class: string | null
  matched_rule: string | null
  matched_signal: string | null
  rate_limit_scope: string
  soft_cap_window_ms: number | null
  soft_cap_tokens: number | null
  window_tokens_used: number
  count_429: number
  count_rotations: number
  last_used_at: number | null
}

export interface LlmNode {
  id: number
  name: string
  kind: string
  base_url: string
  lan_base_url: string | null
  max_concurrency: number
  default_max_output_tokens: number | null
  enabled: boolean
  sort_order: number
  disabled_start: number | null
  disabled_end: number | null
  disabled_timezone: string | null
  in_disabled_window: boolean
  extra_headers: Record<string, unknown>
  extra_body: Record<string, unknown>
  active: number
  waiting: number
  usable_keys: number
  total_keys: number
  healthy: boolean
  cooldown_secs_remaining: number
  last_error: string | null
  lan_configured: boolean
  lan_down: boolean
  lan_retry_after_secs: number
}

export interface ComfyNode {
  id: number
  name: string
  base_url: string
  lan_base_url: string | null
  has_password: boolean
  max_concurrency: number
  enabled: boolean
  sort_order: number
  disabled_start: number | null
  disabled_end: number | null
  in_disabled_window: boolean
  active: number
  waiting: number
}

export interface Route {
  id: number
  model_name: string
  node_id: number
  node_name: string
  upstream_model: string
  enabled: boolean
  priority: number
}

export interface ModelPrice {
  model_name: string
  input_micro_per_1k: number
  output_micro_per_1k: number
  cached_input_micro_per_1k: number | null
}

export interface PriceList {
  items: ModelPrice[]
  default_model: ModelPrice | null
  default_image_micro: number
  models_without_price: string[]
}

export interface Workflow {
  id: number
  name: string
  mode: 'template' | 'raw'
  node_ids: number[]
  param_slots: string[]
  price_micro: number
  enabled: boolean
  archive: boolean
  archive_retention_days: number | null
  has_workflow_json: boolean
}

export interface LogRow {
  id: number
  request_id: string
  account_id: number
  kind: 'llm' | 'image'
  protocol: string | null
  target: string
  node_name: string | null
  key_id: number | null
  rotated_count: number
  stream: boolean
  status: 'ok' | 'error' | 'rejected'
  error_kind: string | null
  error_message: string | null
  queue_wait_ms: number
  latency_ms: number
  prompt_tokens: number
  completion_tokens: number
  cached_tokens: number
  reasoning_tokens: number
  tokens_source: string | null
  image_count: number
  cost_micro: number
  created_at: number
}

export interface SummaryRow {
  day: string
  account_id: number
  target: string
  node_id: number
  requests: number
  errors: number
  prompt_tokens: number
  completion_tokens: number
  image_count: number
  cost_micro: number
}

export interface Overview {
  today: { day: string; cost_micro: number; requests: number; errors: number; images: number }
  month: { month: string; cost_micro: number; requests: number; tokens: number }
  balance_micro: number
  held_micro: number
  nodes: { total: number; healthy: number; cooling: number }
  comfy_nodes: number
  models: number
  workflows: number
}

export interface RuntimeView {
  nodes: Array<{
    id: number
    name: string
    enabled: boolean
    max_concurrency: number
    active: number
    waiting: number
    in_disabled_window: boolean
    usable_keys: number
    total_keys: number
    healthy: boolean
    cooldown_secs_remaining: number
    last_error: string | null
  }>
  comfy_nodes: Array<{
    id: number
    name: string
    enabled: boolean
    max_concurrency: number
    active: number
    waiting: number
  }>
  accounts: Array<{
    id: number
    name: string
    balance_micro: number
    held_micro: number
    active: number
    waiting: number
  }>
}

export interface BackupRecord {
  id: number
  path: string
  size_bytes: number
  status: 'ok' | 'error'
  error: string | null
  started_at: number
  finished_at: number | null
}

export interface KeyEvent {
  id: number
  request_id: string | null
  quota_class: string
  matched_rule: string
  matched_signal: string | null
  cooldown_ms: number
  reset_source: string
  created_at: number
}

export interface BalanceTxn {
  id: number
  kind: string
  amount_micro: number
  balance_after: number
  note: string | null
  created_at: number
}
