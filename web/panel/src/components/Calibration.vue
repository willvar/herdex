<script setup>
import { computed, h } from 'vue'
import { NCard, NDataTable } from 'naive-ui'
import { store } from '../store.js'

const cal = computed(() => store.calibration || {})
const probesByAccount = computed(() => {
  const m = {}
  for (const a of cal.value.accounts || []) m[a.account_id] = []
  for (const p of (store.history || {}).probes || []) {
    if (m[p.account_id]) m[p.account_id].push(p.used_pct)
  }
  return m
})
// inline SVG sparkline of the probe sequence
function spark(vals) {
  if (!vals || vals.length < 2) return h('span', { class: 'muted' }, '—')
  const W = 90, H = 22
  const pts = vals.map((v, i) => `${(i / (vals.length - 1) * W).toFixed(1)},${(H - (v / 100) * H).toFixed(1)}`)
  return h('svg', { width: W, height: H }, [
    h('polyline', { points: pts.join(' '), fill: 'none', stroke: '#7c6cf0', 'stroke-width': 1.5 }),
  ])
}
function fmtTokens(v) {
  if (v == null) return '—'
  if (v >= 1e9) return (v / 1e9).toFixed(2) + 'B'
  if (v >= 1e6) return (v / 1e6).toFixed(2) + 'M'
  if (v >= 1e3) return (v / 1e3).toFixed(1) + 'K'
  return Math.round(v).toString()
}

const columns = [
  { title: '账号', key: 'email', minWidth: 170 },
  { title: 'Plan', key: 'plan_type', width: 80 },
  { title: '已用', key: 'used_pct', width: 70, render: a => a.used_pct != null ? `${a.used_pct}%` : '—' },
  { title: '实测容量', key: 'cap', width: 120, render: a => a.tokens_per_pct != null ? `${fmtTokens(a.tokens_per_pct)} tok/%` : '—' },
  { title: '剩余', key: 'rem', width: 90, render: a => a.remaining_tokens != null ? fmtTokens(a.remaining_tokens) : '—' },
  { title: '趋势', key: 'spark', width: 100, render: a => spark(probesByAccount.value[a.account_id]) },
  { title: '样本', key: 'samples', width: 70, render: a => h('span', { class: 'muted' }, `${a.samples} 样本`) },
]
</script>

<template>
  <n-card
    title="容量估算"
    size="small"
  >
    <template #header-extra>
      <span
        class="muted"
        style="font-size:12px"
      >est.，实测校准</span>
    </template>
    <div
      class="cards"
      style="margin-bottom:10px"
    >
      <div class="card">
        <div class="l">
          池子剩余总容量
        </div><div class="v">
          {{ fmtTokens(cal.pool_remaining_tokens) }}
        </div>
      </div>
      <div class="card">
        <div class="l">
          已校准账号
        </div><div class="v">
          {{ cal.calibrated_accounts || 0 }}/{{ (cal.accounts || []).length }}
        </div>
      </div>
    </div>
    <n-data-table
      :data="cal.accounts || []"
      :columns="columns"
      :row-key="a => a.account_id"
      size="small"
      :bordered="false"
    >
      <template #empty>
        暂无账号
      </template>
    </n-data-table>
  </n-card>
</template>
