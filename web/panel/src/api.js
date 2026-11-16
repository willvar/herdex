// Credential changes cancel owned requests; late responses cannot cross sessions.
let MK = ''
try { MK = localStorage.getItem('herdex_mk') || '' } catch { /* memory-only login */ }
let epoch = 0
const listeners = new Set()
const requests = new Set()
export const getKey = () => MK
export const getKeyEpoch = () => epoch
export function onKeyChange(listener) { listeners.add(listener); return () => listeners.delete(listener) }
export function setKey(key, reason = '') {
  MK = key || ''
  epoch++
  for (const controller of requests) controller.abort()
  requests.clear()
  try { if (MK) localStorage.setItem('herdex_mk', MK); else localStorage.removeItem('herdex_mk') } catch { /* memory-only login */ }
  for (const listener of listeners) listener(reason)
}
export class ApiError extends Error {
  constructor(message, status = 0, data = null) { super(message); this.status = status; this.data = data }
}
export async function api(method, path, body, { signal, timeout = 60000 } = {}) {
  const controller = new globalThis.AbortController()
  const abort = () => controller.abort()
  const session = epoch
  const key = MK
  requests.add(controller)
  signal?.addEventListener('abort', abort, { once: true })
  if (signal?.aborted) controller.abort()
  const timer = setTimeout(abort, timeout)
  try {
    const res = await fetch('/manage/api' + path, {
      method, cache: 'no-store', signal: controller.signal,
      headers: { 'Content-Type': 'application/json', ...(key ? { Authorization: `Bearer ${key}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
    const text = await res.text()
    if (session !== epoch) throw new ApiError('管理会话已更换')
    let data
    try { data = text ? JSON.parse(text) : null } catch { data = null }
    if (!res.ok) {
      const error = new ApiError(typeof data?.error === 'string' ? data.error : text.slice(0, 120) || `HTTP ${res.status}`, res.status, data)
      if (res.status === 401) setKey('', '管理凭据无效，请重新连接')
      throw error
    }
    return data
  } catch (error) {
    if (controller.signal.aborted && session === epoch) throw new ApiError(method === 'GET' ? '查询超时或已取消，可重试；旧数据未更新' : '请求超时或已取消；写入结果可能尚未确认，请先刷新核对，勿重复提交')
    throw error
  } finally {
    globalThis.clearTimeout(timer)
    requests.delete(controller)
    signal?.removeEventListener('abort', abort)
  }
}
export function fmtTok(n) {
  n = n || 0
  if (n >= 1e9) return (n / 1e9).toFixed(2) + 'B'
  if (n >= 1e6) return (n / 1e6).toFixed(2) + 'M'
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k'
  return String(n)
}
