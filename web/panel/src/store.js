// Shared reactive store — one place for fetched state + loaders.
import { reactive } from 'vue'
import { api, getKey } from './api.js'

export const store = reactive({
  ready: false,
  st: null,          // /state payload
  usage7: null,      // /usage?days=7
  usage30: null,     // /usage?days=30 (fetched on demand)
  calibration: null,
  history: null,
  liveQuota: {},     // email -> live quota report
  quotaErr: {},      // email -> error string
  quotaOpen: {},     // email -> detail row expanded
  quotaLoading: {},  // email -> live fetch in flight (查询中…)
})

let settingsRevision = 0

export function commitSettings(settings) {
  settingsRevision++
  store.st = { ...store.st, settings }
}

export async function loadState() {
  if (!getKey()) return
  const revision = settingsRevision
  const state = await api('GET', '/state')
  // A poll started before a successful save may still contain old settings.
  // Keep the saved settings while accepting the poll's other refreshed data.
  store.st = revision === settingsRevision
    ? state
    : { ...state, settings: store.st.settings }
  store.ready = true
}

export async function loadUsage(range) {
  if (!getKey()) return
  store.usage7 = await api('GET', '/usage?days=7')
  // refresh 30d on demand AND whenever it's already loaded (keeps the
  // month view fresh under the periodic reload instead of going stale)
  if (range === 30 || store.usage30) store.usage30 = await api('GET', '/usage?days=30')
}

export async function loadHistory() {
  if (!getKey()) return
  store.history = await api('GET', '/history?days=30')
}

export async function loadCalibration() {
  if (!getKey()) return
  store.calibration = await api('GET', '/calibration')
}

export async function fetchQuota(id, email, silent) {
  if (store.quotaLoading[email]) return
  store.quotaLoading[email] = true
  delete store.quotaErr[email]
  try {
    const r = await api('POST', `/accounts/${id}/quota`)
    store.liveQuota[email] = r
    delete store.quotaErr[email]
  } catch (e) {
    store.quotaErr[email] = e.message
    if (!silent) alert(e.message)
  } finally {
    delete store.quotaLoading[email]
  }
}
