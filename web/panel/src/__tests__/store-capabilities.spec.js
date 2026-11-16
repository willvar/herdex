import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { api, getKey, setKey } from '../api.js'
import { store, loadState, loadUsage, fetchQuota, mutate } from '../store.js'
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r }); return { promise, resolve } }
const snapshot = comment => ({ accounts: [{ id: 'a', email: 'shared@example.test' }, { id: 'b', email: 'shared@example.test' }], quota: {}, api_keys: [{ key: 'fixture-key', comment }] })
beforeEach(() => { setKey('fixture-manage'); vi.stubGlobal('fetch', vi.fn()) })
afterEach(() => { setKey(''); vi.unstubAllGlobals(); vi.useRealTimers() })
describe('Shared management state contracts', () => {
  it('does not let an old poll overwrite a successful mutation and newer refresh', async () => {
    const old = deferred()
    fetch.mockReturnValueOnce(old.promise).mockResolvedValueOnce(globalThis.Response.json({ ok: true })).mockResolvedValueOnce(globalThis.Response.json(snapshot('new')))
    const polling = loadState()
    await mutate('PATCH', '/keys/fixture-key', { comment: 'new' }); await loadState()
    old.resolve(globalThis.Response.json(snapshot('old'))); await polling
    expect(store.st.api_keys[0].comment).toBe('new')
    expect(fetch.mock.calls[0][1].cache).toBe('no-store')
  })
  it('isolates same-email quotas and accepts a newer cached observation', async () => {
    store.st = snapshot('test')
    fetch.mockResolvedValueOnce(globalThis.Response.json({ observed_at: 100, observed_seq: 1, main: { primary: { used_pct: 10 } } })).mockResolvedValueOnce(globalThis.Response.json({ observed_at: 100, observed_seq: 2, main: { primary: { used_pct: 90 } } }))
    await fetchQuota('a', 'shared@example.test'); await fetchQuota('b', 'shared@example.test')
    expect(store.liveQuota.a.main.primary.used_pct).toBe(10)
    expect(store.liveQuota.b.main.primary.used_pct).toBe(90)
    fetch.mockResolvedValueOnce(globalThis.Response.json({ ...snapshot('test'), quota: { a: { default: { observed_at: 100, observed_seq: 3, primary_pct: 80 } } } }))
    await loadState()
    expect(store.liveQuota.a).toBeUndefined()
    expect(store.liveQuota.b.main.primary.used_pct).toBe(90)
  })
  it('loads thirty-day usage independently even if a weekly query would fail', async () => {
    fetch.mockImplementation(async url => url.endsWith('days=7') ? globalThis.Response.json({ error: 'weekly failure' }, { status: 503 }) : globalThis.Response.json({ daily: [] }))
    await loadUsage(30)
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(fetch.mock.calls[0][0]).toContain('days=30')
    expect(store.usage30).toEqual({ daily: [] })
  })
  it('clears data after authorization fails and rejects old-session responses', async () => {
    const old = deferred()
    store.st = snapshot('private fixture')
    fetch.mockReturnValueOnce(old.promise).mockResolvedValueOnce(new globalThis.Response('invalid manage key', { status: 401 }))
    const polling = loadState()
    await expect(api('GET', '/usage?days=7')).rejects.toMatchObject({ status: 401 })
    expect(getKey()).toBe(''); expect(store.st).toBeNull(); expect(localStorage.getItem('herdex_mk')).toBeNull()
    old.resolve(globalThis.Response.json(snapshot('must not return'))); await polling
    expect(store.st).toBeNull()
  })
  it('passes owned cancellation to fetch and clears timeout resources', async () => {
    vi.useFakeTimers()
    fetch.mockImplementation((_url, options) => new Promise((_resolve, reject) => options.signal.addEventListener('abort', () => reject(new Error('aborted')))))
    const request = api('GET', '/state', undefined, { timeout: 100 })
    const failed = expect(request).rejects.toThrow('查询超时或已取消')
    await vi.advanceTimersByTimeAsync(101); await failed
    expect(vi.getTimerCount()).toBe(0)
  })
  it('does not resurrect a stale manual report after a newer background response', async () => {
    store.st = snapshot('test')
    const manual = deferred()
    fetch.mockReturnValueOnce(manual.promise).mockResolvedValueOnce(globalThis.Response.json({ ...snapshot('test'), quota: { a: { default: { observed_at: 100, observed_seq: 3, primary_pct: 80 } } } }))
    const querying = fetchQuota('a', 'shared@example.test')
    await loadState()
    manual.resolve(globalThis.Response.json({ observed_at: 100, observed_seq: 1, main: { primary: { used_pct: 10 } } }))
    await querying
    expect(store.liveQuota.a).toBeUndefined()
    expect(store.st.quota.a.default.primary_pct).toBe(80)
    expect(store.quotaLoading.a).toBeUndefined()
  })

})
