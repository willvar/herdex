<script setup>
import { computed, ref, onMounted, onUnmounted, watch, nextTick } from 'vue'
import { NCard, NButton, NDataTable } from 'naive-ui'
import Chart from 'chart.js/auto'
import { fmtTok } from '../api.js'
import { store, loadUsage } from '../store.js'
import { dailyLabels, seriesMeta, modelSeries, capacitySeries, forecast } from '../charts.js'

const range = ref(7)
const view = ref('tokens')  // tokens | capacity | models
const canvasEl = ref(null)
let chart = null

const usage = computed(() => (range.value === 30 ? store.usage30 : store.usage7) || { daily: [] })
const history = computed(() => store.history || { probes: [], model_daily: [] })

const today = computed(() => {
  const lb = new Date().toLocaleDateString('sv')
  const d = ((usage.value || {}).daily || []).find(x => x.label === lb) || {}
  return {
    requests: d.requests || 0,
    in: fmtTok(d.input || 0),
    cached: fmtTok(d.cached || 0),
    out: fmtTok(d.output || 0),
  }
})
const week = computed(() => {
  const daily = (usage.value || {}).daily || []
  return {
    requests: daily.reduce((a, x) => a + x.requests, 0),
    tokens: fmtTok(daily.reduce((a, x) => a + x.input + x.output, 0)),
  }
})

const calAccounts = computed(() => store.calibration?.accounts || [])

const fc = computed(() => {
  const daily = (usage.value || {}).daily || []
  const burn = daily.reduce((a, x) => a + x.input + x.output, 0) / range.value
  const poolTokPerPct = calAccounts.value.reduce((a, c) => a + (c.tokens_per_pct || 0), 0)
  const rows = calAccounts.value.filter(c => c.used_pct != null)
  const poolPct = rows.length ? rows.reduce((a, c) => a + c.used_pct, 0) / rows.length : null
  return forecast(store.calibration?.pool_remaining_tokens, burn, poolTokPerPct, poolPct)
})

const series = ref(['in', 'cached', 'out'])
function toggleSeries(k) {
  const i = series.value.indexOf(k)
  if (i >= 0) series.value.splice(i, 1); else series.value.push(k)
  render()
}

function chartData() {
  if (view.value === 'tokens') {
    const dl = dailyLabels(range.value, usage.value.daily)
    const labels = dl.labels
    const datasets = series.value.map(k => ({
      label: seriesMeta[k].label,
      data: labels.map(lb => seriesMeta[k].get(dl.map[new Date(Date.now() - (range.value - 1 - labels.indexOf(lb)) * 864e5).toLocaleDateString('sv')] || {})),
      backgroundColor: seriesMeta[k].color,
      stack: 't',
    }))
    return {
      type: 'bar', labels, datasets,
      opts: {
        scales: {
          x: { stacked: true, ticks: { color: '#8f8ca0' }, grid: { display: false } },
          y: { stacked: true, ticks: { color: '#8f8ca0', callback: v => fmtTok(v) } },
        },
        plugins: {
          legend: { labels: { color: '#8f8ca0', boxWidth: 12 } },
          tooltip: { callbacks: { label: c => ` ${c.dataset.label}: ${fmtTok(c.parsed.y)}` } },
        },
      },
    }
  }
  if (view.value === 'capacity') {
    const cs = capacitySeries(history.value.probes, calAccounts.value, range.value)
    return {
      type: 'line', labels: cs.labels,
      datasets: cs.datasets.map(d => ({
        label: d.label, data: d.data, borderColor: d.borderColor,
        borderWidth: d.borderWidth, borderDash: d.borderDash,
        tension: d.tension, pointRadius: 0, spanGaps: true,
      })),
      opts: {
        scales: {
          x: { ticks: { color: '#8f8ca0', maxTicksLimit: 14 }, grid: { color: '#2c2b38' } },
          y: { min: 0, max: 100, ticks: { color: '#8f8ca0', callback: v => v + '%' } },
        },
        plugins: {
          legend: { labels: { color: '#8f8ca0', boxWidth: 12 } },
          tooltip: { callbacks: { label: c => ` ${c.dataset.label}: ${c.parsed.y?.toFixed?.(1)}%` } },
        },
      },
    }
  }
  const ms = modelSeries(history.value.model_daily, range.value)
  return {
    type: 'line', labels: ms.labels, datasets: ms.datasets,
    opts: {
      scales: {
        x: { ticks: { color: '#8f8ca0' }, grid: { display: false } },
        y: { ticks: { color: '#8f8ca0', callback: v => fmtTok(v) } },
      },
      plugins: {
        legend: { labels: { color: '#8f8ca0', boxWidth: 12 } },
        tooltip: { callbacks: { label: c => ` ${c.dataset.label}: ${fmtTok(c.parsed.y)}` } },
      },
    },
  }
}

function render() {
  if (!canvasEl.value) return
  const d = chartData()
  const data = { labels: d.labels, datasets: d.datasets }
  const options = { responsive: true, maintainAspectRatio: false, ...d.opts }
  // Chart.js cannot switch type on a live instance (it reads config.type);
  // destroy and rebuild when the view's chart type differs.
  if (chart && chart.config.type !== d.type) { chart.destroy(); chart = null }
  if (chart) { chart.data = data; chart.options = options; chart.update(); return }
  chart = new Chart(canvasEl.value, { type: d.type, data, options })
}

onMounted(async () => { await nextTick(); render() })
onUnmounted(() => { chart?.destroy(); chart = null })
watch([() => store.usage7, () => store.usage30, () => store.history, () => store.calibration, view, range, series], () => render())

function setRange(r) { range.value = r; if (r === 30) loadUsage(30) }

const usage7rows = computed(() => (store.usage7 || {}).by_account || [])
const usage7rows2 = computed(() => (store.usage7 || {}).by_model || [])
const keyRange = ref(7)
watch(keyRange, r => { if (r === 30 && !store.usage30) loadUsage(30) })
const byKey = computed(() => keyRange.value === 30 ? (store.usage30?.by_key || []) : (store.usage7?.by_key || []))
const keyMap = computed(() => Object.fromEntries((store.st?.api_keys || []).map(k => [k.key, k.comment || k.key])))

function usageCol(title, key, fmt) {
  return { title, key, render: r => fmt(r) }
}
const accColumns = [
  usageCol('账号', 'label', r => r.label),
  { title: '请求', key: 'requests', width: 70, sorter: (a, b) => a.requests - b.requests },
  usageCol('输入', 'in', r => fmtTok(r.input)),
  usageCol('输出', 'out', r => fmtTok(r.output)),
]
const keyColumns = [
  { title: 'Key', key: 'label', render: r => {
    const s = keyMap.value[r.label] || r.label
    return s.length > 24 ? s.slice(0, 8) + '…' + s.slice(-8) : s
  } },
  { title: '请求', key: 'requests', width: 70 },
  usageCol('输入', 'input', r => fmtTok(r.input || 0)),
  usageCol('缓存', 'cached', r => fmtTok(r.cached || 0)),
  usageCol('输出', 'output', r => fmtTok(r.output || 0)),
]
const modelColumns = [
  usageCol('模型', 'label', r => r.label),
  { title: '请求', key: 'requests', width: 70 },
  usageCol('输入', 'in', r => fmtTok(r.input)),
  usageCol('输出', 'out', r => fmtTok(r.output)),
]
</script>

<template>
  <n-card
    title="用量"
    size="small"
  >
    <div class="cards">
      <div class="card">
        <div class="l">
          今日请求
        </div><div class="v">
          {{ today.requests }}
        </div>
      </div>
      <div class="card">
        <div class="l">
          今日输入 <span class="muted">(缓存)</span>
        </div><div class="v">
          {{ today.in }} <span
            class="muted"
            style="font-size:13px"
          >({{ today.cached }})</span>
        </div>
      </div>
      <div class="card">
        <div class="l">
          今日输出
        </div><div class="v">
          {{ today.out }}
        </div>
      </div>
      <div class="card">
        <div class="l">
          近{{ range }}天请求
        </div><div class="v">
          {{ week.requests }}
        </div>
      </div>
      <div class="card">
        <div class="l">
          近{{ range }}天 tokens
        </div><div class="v">
          {{ week.tokens }}
        </div>
      </div>
      <div
        v-if="fc"
        class="card"
        :style="fc.daysLeft < 3 ? 'border:1px solid var(--err)' : ''"
      >
        <div class="l">
          池子预计触顶
        </div>
        <div class="v">
          {{ fc.daysLeft }} 天后
        </div>
        <div class="l">
          约 {{ fc.reachedAt }} · 日均 {{ fmtTok(fc.dailyTokens) }}
        </div>
      </div>
    </div>
    <div class="row">
      <n-button-group size="tiny">
        <n-button
          :type="view === 'tokens' ? 'primary' : 'default'"
          size="tiny"
          @click="view = 'tokens'"
        >
          Token 堆叠
        </n-button>
        <n-button
          :type="view === 'capacity' ? 'primary' : 'default'"
          size="tiny"
          @click="view = 'capacity'"
        >
          容量%走势
        </n-button>
        <n-button
          :type="view === 'models' ? 'primary' : 'default'"
          size="tiny"
          @click="view = 'models'"
        >
          模型燃烧对比
        </n-button>
      </n-button-group>
      <span style="flex:1" />
      <n-button
        size="tiny"
        :type="range === 7 ? 'primary' : 'default'"
        @click="range = 7"
      >
        7天
      </n-button>
      <n-button
        size="tiny"
        :type="range === 30 ? 'primary' : 'default'"
        @click="setRange(30)"
      >
        30天
      </n-button>
    </div>
    <div
      v-if="view === 'tokens'"
      class="legend"
    >
      <span
        v-for="(m, k) in seriesMeta"
        :key="k"
      >
        <i
          :style="{ background: m.color, opacity: series.includes(k) ? 1 : 0.25, cursor: 'pointer' }"
          @click="toggleSeries(k)"
        />{{ m.label }}
      </span>
    </div>
    <div style="height:260px;margin-top:10px">
      <canvas ref="canvasEl" />
    </div>
    <div
      class="cols3"
      style="margin-top:14px"
    >
      <div>
        <h3
          class="muted"
          style="font-size:12px"
        >
          按账号（7天）
        </h3>
        <n-data-table
          :data="usage7rows"
          :columns="accColumns"
          size="small"
          :bordered="false"
        />
      </div>
      <div>
        <h3 style="font-size:12px;color:var(--dim);margin:0 0 6px">
          按 Key
          <n-button
            size="tiny"
            :type="keyRange === 7 ? 'primary' : 'default'"
            @click="keyRange = 7"
          >
            周
          </n-button>
          <n-button
            size="tiny"
            :type="keyRange === 30 ? 'primary' : 'default'"
            @click="keyRange = 30"
          >
            月
          </n-button>
        </h3>
        <n-data-table
          :data="byKey"
          :columns="keyColumns"
          size="small"
          :bordered="false"
        />
      </div>
      <div>
        <h3 style="font-size:12px;color:var(--dim);margin:0 0 6px">
          按模型（7天）
        </h3>
        <n-data-table
          :data="usage7rows2"
          :columns="modelColumns"
          size="small"
          :bordered="false"
        />
      </div>
    </div>
  </n-card>
</template>
