<script setup>
import { computed, h, reactive, ref } from 'vue'
import { NCard, NDataTable, NButton, NSwitch, NTag, NPopconfirm, useMessage } from 'naive-ui'
import { planLabel } from '../plans.js'
import { store, mutate, refreshAfterMutation, fetchQuota } from '../store.js'

const message = useMessage()
const emit = defineEmits(['reauthorize'])
const pending = reactive({})
const refreshing = ref(false), progress = ref('')
const accounts = computed(() => store.st?.accounts || [])
const cachedQuota = computed(() => store.st?.quota || {})
const resets = reactive({})

function tsOf(v) { if (!v) return 0; return typeof v === 'number' ? (v < 1e12 ? v * 1000 : v) : Date.parse(v) }
function remPct(w) { return Math.round(100 - w.used_pct) }
function remColor(rem) { return rem < 10 ? '#e07070' : rem < 30 ? 'orange' : '#5fd0a0' }
function normWin(pct, resetEpoch, secs, known) {
  return { used_pct: known === false ? null : pct, reset_at: resetEpoch ? resetEpoch * 1000 : null, window_seconds: secs || null }
}
function cdRel(epochMs) {
  if (!epochMs || epochMs < 0) return '—'
  const diff = epochMs - Date.now()
  if (diff <= 0) return '预计已到期，待确认'
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
function creditTime(value, empty = '未知') {
  if (!value) return empty
  const time = Date.parse(value)
  if (!Number.isFinite(time)) return value
  const date = new Date(time)
  const pad = n => String(n).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}
function creditStatus(status) {
  return { available: '可用', redeemed: '已使用', expired: '已过期' }[status] || status || '未知'
}
function resetCreditRows(details) {
  if (!Array.isArray(details?.credits)) return []
  return details.credits.map((credit, index) => ({
    key: credit.id || index,
    status: creditStatus(credit.status),
    granted: creditTime(credit.granted_at),
    expires: creditTime(credit.expires_at, '永不过期'),
  }))
}
function hasData(w) {
  return w && w.used_pct !== undefined && w.used_pct !== null &&
    ((w.window_seconds && w.window_seconds > 0) || tsOf(w.reset_at) > 0)
}
function quotaRows(a) {
  const live = store.liveQuota[a.id]
  const cached = cachedQuota.value[a.id]
  const entries = []
  if (live) {
    entries.push(['主窗口', live.main?.primary, live.main?.secondary])
    for (const x of live.additional || []) entries.push([x.name, x.limit?.primary, x.limit?.secondary])
  } else if (cached) {
    for (const [m, q] of Object.entries(cached))
      entries.push([m, normWin(q.primary_pct, q.primary_reset_at, q.primary_window_secs, q.primary_known), normWin(q.secondary_pct, q.secondary_reset_at, q.secondary_window_secs, q.secondary_known)])
  }
  const rows = []
  for (const [name, pri, sec] of entries) {
    if (hasData(pri)) rows.push({ key: name + ':p', name, win: winLabel(pri), rem: remPct(pri), reset: cdRel(tsOf(pri.reset_at)) })
    if (hasData(sec)) rows.push({ key: name + ':s', name, win: winLabel(sec), rem: remPct(sec), reset: cdRel(tsOf(sec.reset_at)) })
  }
  return rows
}

async function accountAction(a, method, path, body, label) {
  if (pending[a.id] || resets[a.id]?.loading) return
  pending[a.id] = true
  try {
    await mutate(method, path, body)
    if (!await refreshAfterMutation({ capacity: true })) message.warning(`${label}，但列表刷新失败，请刷新核对，不要重复操作`)
    else message.success(label)
  } catch (error) { message.error(error.message) }
  finally { delete pending[a.id] }
}
const toggle = (a, dis) => accountAction(a, 'POST', `/accounts/${a.id}/disable`, { disabled: dis }, dis ? '账号已禁用' : '账号已启用')
const delAcc = a => accountAction(a, 'DELETE', `/accounts/${a.id}`, undefined, '账号已删除')
function freshQuota(a) {
  const q = store.liveQuota[a.id]
  const observed = q?.observed_at || q?.fetched_at
  return !!observed && Date.now() / 1000 - observed < 180
}
async function consumeReset(a) {
  if (!canReset(a)) return false
  const result = resets[a.id] = reactive({ loading: true, type: 'info', text: '正在消耗 1 张最早过期券并重置…' })
  try {
    const r = await mutate('POST', `/accounts/${a.id}/quota/consume`)
    if (r?.consumed !== true) throw new Error('未收到有效的重置结果')
    result.type = 'success'
    result.text = '重置成功：已消耗 1 张最早过期券。'
  } catch (e) {
    result.type = 'error'
    result.text = `重置请求失败：${e.message}`
  } finally {
    result.loading = false
  }
  message[result.type](`${a.email}：${result.text}`, { duration: 8000, closable: true })
  if (result.type === 'success') void fetchQuota(a.id, a.email, true)
}
function canReset(a) {
  return !resets[a.id]?.loading && !store.quotaLoading[a.id] && !store.quotaErr[a.id] &&
    freshQuota(a) && (store.liveQuota[a.id]?.credits?.available ?? 0) > 0
}
function toggleAll() {
  const open = accounts.value.some(a => !store.quotaOpen[a.id])
  for (const a of accounts.value) store.quotaOpen[a.id] = open
}
async function refreshAll() {
  if (refreshing.value) return
  refreshing.value = true
  try {
    let done = 0
    for (const a of accounts.value) {
      progress.value = `${++done}/${accounts.value.length}`
      if (!resets[a.id]?.loading) await fetchQuota(a.id, a.email, true)
    }
  } finally { refreshing.value = false; progress.value = '' }
}
function expandRows(keys) {
  for (const a of accounts.value) {
    const newlyOpened = keys.includes(a.id) && !store.quotaOpen[a.id]
    store.quotaOpen[a.id] = keys.includes(a.id)
    if (newlyOpened && !resets[a.id]?.loading) void fetchQuota(a.id, a.email, true)
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
const resetCreditColumns = [
  { title: '状态', key: 'status', width: 80 },
  { title: '发放时间', key: 'granted', width: 150 },
  { title: '到期时间', key: 'expires', width: 150 },
]

function renderQuota(row) {
  const loading = store.quotaLoading[row.id]
  const error = store.quotaErr[row.id]
  const rows = quotaRows(row)
  const live = store.liveQuota[row.id]
  const reset = resets[row.id]
  const resetDetails = live?.reset_credits
  const resetRows = resetCreditRows(resetDetails)
  const stale = rows.length > 0 && (loading || error || !live || !freshQuota(row))
  return h('div', { style: 'padding:8px 12px 4px' }, [
    h('div', { class: 'muted' }, `工作区：${row.workspace_id || '未知'} · 额度观测时间：${live?.observed_at || live?.fetched_at ? new Date((live.observed_at || live.fetched_at) * 1000).toLocaleString('zh-CN', { timeZone: 'Asia/Hong_Kong' }) + ' HKT' : '见缓存 / 未知'}`),
    loading && h('div', { role: 'status', class: 'muted' }, '查询中…'),
    error && h('div', { role: 'alert', class: 'err' }, `查询失败：${error}`),
    stale && h('div', { class: 'muted' }, '以下为上次观测的额度，可能已过期。'),
    rows.length > 0 && h(NDataTable, {
        data: rows, columns: detailColumns, size: 'small',
        bordered: false,
    }),
    !loading && !error && rows.length === 0 && h('div', { class: 'muted' }, '尚无观测，查询额度不消耗模型配额。'),
    h(NButton, {
      size: 'tiny', disabled: !!loading || !!reset?.loading, style: 'margin-top:8px',
      onClick: e => { e.stopPropagation(); fetchQuota(row.id, row.email, true) },
    }, { default: () => error ? '重试' : '刷新额度' }),
    live?.credits && h('div', { style: 'margin-top:8px;display:flex;gap:8px;align-items:center' }, [
        `banked 重置券：${live.credits.available}`,
        h(NPopconfirm, {
          positiveText: '确认消耗 1 张并重置', negativeText: '取消',
          positiveButtonProps: { loading: !!reset?.loading, disabled: !canReset(row) },
          negativeButtonProps: { disabled: !!reset?.loading },
          onPositiveClick: () => consumeReset(row),
        }, {
          trigger: () => h(NButton, {
            size: 'tiny', type: 'warning', loading: !!reset?.loading, disabled: !canReset(row),
          }, { default: () => reset?.loading ? '正在重置…' : '消耗最早过期券并重置' }),
          default: () => `确认消耗账号 ${row.email} 的 1 张最早过期可用券并重置额度？券消耗后不可撤销。`,
        }),
    ]),
    resetDetails && h('div', { style: 'margin-top:10px' }, [
      h('div', { class: 'muted', style: 'margin-bottom:4px' }, `banked 重置券明细：${resetDetails.available_count} 张可用`),
      resetRows.length > 0
        ? h(NDataTable, { data: resetRows, columns: resetCreditColumns, size: 'small', bordered: false })
        : h('div', { class: 'muted' }, '上游未提供逐张重置券明细。'),
    ]),
    live?.credits && !resetDetails && h('div', { class: 'muted', style: 'margin-top:8px' }, '重置券明细暂时不可用。'),
    reset && h('div', {
      role: reset.type === 'error' ? 'alert' : 'status',
      class: reset.type === 'success' ? 'ok' : reset.type === 'error' ? 'err' : 'muted',
      style: 'margin-top:8px;white-space:pre-wrap;overflow-wrap:anywhere',
    }, reset.text),
  ])
}

const columns = [
  { type: 'expand', expandable: () => true, renderExpand: renderQuota },
  { title: '账号 / 工作区', key: 'email', minWidth: 180, render: r => `${r.email} · ${r.workspace_id?.slice(-6) || r.id.slice(-6)}` },
  { title: 'Plan', key: 'plan_type', minWidth: 140, render: r =>
    h('span', { title: r.plan_type }, planLabel(r.plan_type)) },
  { title: 'Token 过期', key: 'expires_at', width: 140 },
  { title: '状态', key: 'status', width: 170, render: (r) => {
    if (r.disabled) return h(NTag, { type: 'error', size: 'small' }, { default: () => '已禁用' })
    if (r.last_error) return h(NTag, { type: 'error', size: 'small' }, { default: () => r.last_error.slice(0, 40) })
    return h(NTag, { type: 'success', size: 'small' }, { default: () => '已启用 · 健康未确认' })
  } },
  { title: '操作', key: 'actions', width: 270, render: (r) =>
    h('span', { style: 'display:inline-flex;gap:8px;align-items:center', onClick: e => e.stopPropagation() }, [
      h(NSwitch, { size: 'small', 'aria-label': '账号启用状态', loading: !!pending[r.id], disabled: !!pending[r.id] || !!resets[r.id]?.loading, value: !r.disabled, 'onUpdateValue': v => toggle(r, !v) }),
      h(NButton, { size: 'tiny', disabled: !!pending[r.id] || !!resets[r.id]?.loading, onClick: () => emit('reauthorize', r.id) }, { default: () => '更新凭据' }),
      h(NPopconfirm, { onPositiveClick: () => delAcc(r) }, {
        trigger: () => h(NButton, { size: 'tiny', type: 'error', ghost: true, disabled: !!pending[r.id] || !!resets[r.id]?.loading }, { default: () => '删除' }),
        default: () => '删除该账号？',
      }),
    ]) },
]

const rowProps = (r) => ({
  style: 'cursor:pointer',
  onClick: event => {
    if (event.target?.closest?.('.n-data-table-expand-trigger')) return
    store.quotaOpen[r.id] = !store.quotaOpen[r.id]
    if (store.quotaOpen[r.id] && !resets[r.id]?.loading) fetchQuota(r.id, r.email, true)
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
        :loading="refreshing"
        @click="refreshAll"
      >
        刷新全部额度 {{ progress }}
      </n-button>
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
      :scroll-x="1000"
      :expanded-row-keys="accounts.filter(a => store.quotaOpen[a.id]).map(a => a.id)"
      size="small"
      @update:expanded-row-keys="expandRows"
    >
      <template #empty>
        还没有账号，展开“添加账号”
      </template>
    </n-data-table>
  </n-card>
</template>
