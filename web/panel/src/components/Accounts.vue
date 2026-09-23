<script setup>
import { computed, h } from 'vue'
import { NCard, NDataTable, NButton, NSwitch, NTag, NPopconfirm, useMessage } from 'naive-ui'
import { api } from '../api.js'
import { store, loadState, fetchQuota } from '../store.js'

const message = useMessage()
const accounts = computed(() => store.st?.accounts || [])
const cachedQuota = computed(() => store.st?.quota || {})

function tsOf(v) { if (!v) return 0; return typeof v === 'number' ? (v < 1e12 ? v * 1000 : v) : Date.parse(v) }
function remPct(w) { return Math.round(100 - w.used_pct) }
function remColor(rem) { return rem < 10 ? '#e07070' : rem < 30 ? 'orange' : '#5fd0a0' }
function normWin(pct, resetEpoch, secs) {
  return { used_pct: pct, reset_at: resetEpoch ? resetEpoch * 1000 : null, window_seconds: secs || null }
}
function cdRel(epochMs) {
  if (!epochMs || epochMs < 0) return '—'
  const diff = epochMs - Date.now()
  if (diff <= 0) return '已重置'
  const h = Math.floor(diff / 36e5), m = Math.floor((diff % 36e5) / 6e4)
  if (h >= 24) return `${Math.floor(h / 24)}天${h % 24}时`
  if (h > 0) return `${h}时${m}分`
  return `${m}分`
}
function winLabel(w) {
  if (!w) return ''
  if (w.window_seconds === 18000) return '5h'
  if (w.window_seconds === 604800) return '周'
  if (w.window_seconds) return `${Math.round(w.window_seconds / 3600)}h`
  return ''
}
function hasData(w) {
  return w && w.used_pct !== undefined && w.used_pct !== null &&
    ((w.window_seconds && w.window_seconds > 0) || tsOf(w.reset_at) > 0)
}
function quotaRows(a) {
  const live = store.liveQuota[a.email]
  const cached = cachedQuota.value[a.email]
  const entries = []
  if (live) {
    entries.push(['主窗口', live.main?.primary, live.main?.secondary])
    for (const x of live.additional || []) entries.push([x.name, x.limit?.primary, x.limit?.secondary])
  } else if (cached) {
    for (const [m, q] of Object.entries(cached))
      entries.push([m, normWin(q.primary_pct, q.primary_reset_at, q.primary_window_secs), normWin(q.secondary_pct, q.secondary_reset_at, q.secondary_window_secs)])
  }
  const rows = []
  for (const [name, pri, sec] of entries) {
    if (hasData(pri)) rows.push({ key: name + ':p', name, win: winLabel(pri), rem: remPct(pri), reset: cdRel(tsOf(pri.reset_at)) })
    if (hasData(sec)) rows.push({ key: name + ':s', name, win: winLabel(sec), rem: remPct(sec), reset: cdRel(tsOf(sec.reset_at)) })
  }
  return rows
}

async function toggle(a, dis) {
  try { await api('POST', `/accounts/${a.id}/disable`, { disabled: dis }); loadState() } catch (e) { message.error(e.message) }
}
function delAcc(a) {
  api('DELETE', `/accounts/${a.id}`).then(loadState).catch(e => message.error(e.message))
}
async function consumeReset(a) {
  try {
    const r = await api('POST', `/accounts/${a.id}/quota/consume`)
    store.liveQuota[a.email] = r
    delete store.quotaErr[a.email]
    loadState()
    message.success('已消耗一个重置积分')
  } catch (e) { message.error(e.message) }
}
async function toggleAll() {
  const anyClosed = accounts.value.some(a => !store.quotaOpen[a.id])
  for (const a of accounts.value) store.quotaOpen[a.id] = anyClosed
  if (anyClosed) {
    for (const a of accounts.value) await fetchQuota(a.id, a.email, true)
  }
}

const detailColumns = [
  { title: '窗口', key: 'name', width: 140, render: r => `${r.name} ${r.win}` },
  { title: '剩余额度', key: 'rem', render: r =>
    h('span', { style: 'display:inline-flex;align-items:center;gap:8px' }, [
      h('div', { class: 'bar', style: 'width:110px' }, [h('i', { style: `width:${Math.max(0, Math.min(100, r.rem))}%;background:${remColor(r.rem)}` })]),
      h('span', `${r.rem}%`),
    ]) },
  { title: '重置倒计时', key: 'reset', width: 130 },
]

function renderQuota(row) {
  const loading = store.quotaLoading[row.email]
  const error = store.quotaErr[row.email]
  const rows = quotaRows(row)
  const live = store.liveQuota[row.email]
  const stale = rows.length > 0 && (loading || error || !live)
  return h('div', { style: 'padding:8px 12px 4px' }, [
    loading && h('div', { role: 'status', class: 'muted' }, '查询中…'),
    error && h('div', { role: 'alert', class: 'err' }, `查询失败：${error}`),
    stale && h('div', { class: 'muted' }, '以下为上次观测的额度，可能已过期。'),
    rows.length > 0 && h(NDataTable, {
        data: rows, columns: detailColumns, size: 'small',
        bordered: false,
    }),
    !loading && !error && rows.length === 0 && h('div', { class: 'muted' }, '尚无观测，查询额度不消耗模型配额。'),
    h(NButton, {
      size: 'tiny', disabled: !!loading, style: 'margin-top:8px',
      onClick: e => { e.stopPropagation(); fetchQuota(row.id, row.email, true) },
    }, { default: () => error ? '重试' : '刷新额度' }),
    live?.credits && h('div', { style: 'margin-top:8px;display:flex;gap:8px;align-items:center' }, [
        `banked 重置积分：${live.credits.available}`,
        h(NButton, {
          size: 'tiny', disabled: live.credits.available < 1,
          onClick: () => consumeReset(row),
        }, { default: () => '消耗一个立即重置' }),
    ]),
  ])
}

const columns = [
  { type: 'expand', expandable: () => true, renderExpand: renderQuota },
  { title: '邮箱', key: 'email', minWidth: 180 },
  { title: 'Plan', key: 'plan_type', width: 80 },
  { title: 'Token 过期', key: 'expires_at', width: 140 },
  { title: '状态', key: 'status', width: 170, render: (r) => {
    if (r.disabled) return h(NTag, { type: 'error', size: 'small' }, { default: () => '已禁用' })
    if (r.last_error) return h(NTag, { type: 'error', size: 'small' }, { default: () => r.last_error.slice(0, 40) })
    return h(NTag, { type: 'success', size: 'small' }, { default: () => '正常' })
  } },
  { title: '操作', key: 'actions', width: 170, render: (r) =>
    h('span', { style: 'display:inline-flex;gap:8px;align-items:center', onClick: e => e.stopPropagation() }, [
      h(NSwitch, { size: 'small', value: !r.disabled, 'onUpdateValue': v => toggle(r, !v) }),
      h(NPopconfirm, { onPositiveClick: () => delAcc(r) }, {
        trigger: () => h(NButton, { size: 'tiny', type: 'error', ghost: true }, { default: () => '删除' }),
        default: () => '删除该账号？',
      }),
    ]) },
]

const rowProps = (r) => ({
  style: 'cursor:pointer',
  onClick: () => {
    store.quotaOpen[r.id] = !store.quotaOpen[r.id]
    if (store.quotaOpen[r.id]) fetchQuota(r.id, r.email, true)
  },
})
</script>

<template>
  <n-card
    title="账号池"
    size="small"
  >
    <template #header-extra>
      <n-button
        size="small"
        @click="toggleAll"
      >
        一键展开/收起
      </n-button>
    </template>
    <n-data-table
      :columns="columns"
      :data="accounts"
      :row-key="r => r.id"
      :row-props="rowProps"
      :expanded-row-keys="accounts.filter(a => store.quotaOpen[a.id]).map(a => a.id)"
      size="small"
    >
      <template #empty>
        还没有账号，展开“添加账号”
      </template>
    </n-data-table>
  </n-card>
</template>
