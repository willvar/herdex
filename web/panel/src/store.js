import { reactive } from 'vue'
import { api, getKey, getKeyEpoch, onKeyChange } from './api.js'
const initial = () => ({
  ready: false, st: null, usage7: null, usage30: null, calibration: null, history: null,
  liveQuota: {}, quotaErr: {}, quotaOpen: {}, quotaLoading: {}, status: {}, authError: '',
})
export const store = reactive(initial())
let settingsRevision = 0
let mutationRevision = 0
const serials = {}
onKeyChange(reason => {
  mutationRevision++
  settingsRevision++
  Object.assign(store, initial(), { authError: reason })
})
export function commitSettings(settings) {
  settingsRevision++
  store.st = { ...store.st, settings }
}
export async function mutate(method, path, body) {
  mutationRevision++
  try { return body === undefined ? await api(method, path) : await api(method, path, body) }
  finally { mutationRevision++ }
}
export async function refreshAfterMutation({ capacity = false } = {}) {
  const results = await Promise.allSettled([loadState(), ...(capacity ? [loadCalibration()] : [])])
  return results.every(result => result.status === 'fulfilled' && result.value !== undefined)
}
async function load(name, path, commit) {
  if (!getKey()) return
  const session = getKeyEpoch()
  const serial = serials[name] = (serials[name] || 0) + 1
  const revision = mutationRevision
  const settings = settingsRevision
  const valid = () => session === getKeyEpoch() && serials[name] === serial
  store.status[name] = { ...store.status[name], loading: true, error: '' }
  try {
    const value = await api('GET', path)
    if (valid() && (name !== 'state' || revision === mutationRevision)) {
      commit(value, settings)
      store.status[name] = { loading: false, error: '', updatedAt: Date.now() }
      return value
    }
  } catch (error) {
    if (valid()) { store.status[name].error = error.message; throw error }
  } finally {
    if (valid()) store.status[name].loading = false
  }
}
export function loadState() {
  return load('state', '/state', (state, settings) => {
    store.st = settings === settingsRevision ? state : { ...state, settings: store.st?.settings }
    store.ready = true
    const ids = new Set((state.accounts || []).map(a => a.id))
    for (const id of Object.keys(store.liveQuota)) {
      const live = store.liveQuota[id]
      const cached = Object.values(state.quota?.[id] || {})
      if (!ids.has(id) || cached.some(q =>
        q.observed_at > live.observed_at || (q.observed_at === live.observed_at && q.observed_seq > live.observed_seq))) delete store.liveQuota[id]
    }
    for (const map of [store.quotaOpen, store.quotaErr, store.quotaLoading]) {
      for (const id of Object.keys(map)) if (!ids.has(id)) delete map[id]
    }
  })
}
export async function loadUsage(range) {
  const ranges = range ? [range] : store.usage30 ? [7, 30] : [7]
  const results = await Promise.allSettled(ranges.map(days => load(`usage${days}`, `/usage?days=${days}`, value => { store[`usage${days}`] = value })))
  const failed = results.find(r => r.status === 'rejected')
  if (failed) throw failed.reason
}
export const loadHistory = () => load('history', '/history?days=30', value => { store.history = value })
export const loadCalibration = () => load('calibration', '/calibration', value => { store.calibration = value })
export async function fetchQuota(id, _email, _silent) {
  if (!getKey() || store.quotaLoading[id]) return
  const session = getKeyEpoch()
  store.quotaLoading[id] = true
  delete store.quotaErr[id]
  try {
    const value = await api('POST', `/accounts/${encodeURIComponent(id)}/quota`)
    if (session === getKeyEpoch() && store.st?.accounts?.some(a => a.id === id)) {
      const newer = Object.values(store.st.quota?.[id] || {}).some(q => q.observed_at > value.observed_at || (q.observed_at === value.observed_at && q.observed_seq > value.observed_seq))
      if (newer) { delete store.liveQuota[id]; return }
      store.liveQuota[id] = { ...value, fetched_at: Math.floor(Date.now() / 1000) }
      delete store.quotaErr[id]
    }
  } catch (error) {
    if (session === getKeyEpoch()) store.quotaErr[id] = error.message
  } finally {
    if (session === getKeyEpoch()) delete store.quotaLoading[id]
  }
}
