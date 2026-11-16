<script setup>
import { computed, h, ref, reactive, onMounted, onUnmounted, watch } from 'vue'
import { NCard, NDataTable, NSelect, NCheckbox, NInput, NButton, NPagination, NAlert, NModal } from 'naive-ui'
import { api, getKeyEpoch } from '../api.js'
import { store } from '../store.js'
import { copyText } from '../clipboard.js'
const props = defineProps({ active: { type: Boolean, default: true } })
const filters = reactive({ model: '', account_id: null, api_key: null, service_tier: null, errors_only: false, request_id: '', since: '', until: '' })
let applied = {}, serial = 0, controller
const page = ref(1), total = ref(0), items = ref([]), loading = ref(false), error = ref(''), updated = ref(0), newer = ref(false), expanded = ref([]), manualCopy = ref('')
const accountOptions = computed(() => (store.st?.accounts || []).map(a => ({ value: a.id, label: `${a.email} · ${a.workspace_id?.slice(-6) || a.id.slice(-6)}` })))
const keyOptions = computed(() => (store.st?.api_keys || []).map(k => ({ value: k.key, label: k.comment || `…${k.key.slice(-4)}` })))
const tiers = [{ label: '普通 / 未指定', value: '' }, { label: 'Fast', value: 'priority' }, { label: 'Ultrafast', value: 'ultrafast' }]
const keyLabel = key => store.st?.api_keys?.find(k => k.key === key)?.comment || (key ? `…${key.slice(-4)}` : '未记录')
function hasError(log) { return log.status === 0 || log.status >= 400 || !!log.error }
function tierLabel(tier) { return tier == null ? '未记录' : tier === 'priority' || tier === 'fast' ? 'Fast' : tier === 'ultrafast' ? 'Ultrafast' : tier || '未指定' }
function epoch(text) {
  if (!text) return undefined
  if (!/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/.test(text)) throw new Error('时间请按 HKT YYYY-MM-DD HH:mm 输入')
  const value = Date.parse(text.replace(' ', 'T') + ':00+08:00')
  if (!Number.isFinite(value)) throw new Error('时间无效')
  return Math.floor(value / 1000)
}
async function search() {
  try {
    const since = epoch(filters.since), until = epoch(filters.until)
    if (since && until && since > until) throw new Error('开始时间不能晚于结束时间')
    applied = { ...filters, since, until }
    page.value = 1; expanded.value = []; await load()
  } catch (e) { error.value = e.message }
}
async function load() {
  const current = ++serial, session = getKeyEpoch()
  controller?.abort(); controller = new globalThis.AbortController()
  loading.value = true; error.value = ''
  const query = new globalThis.URLSearchParams({ page: String(page.value), limit: '50' })
  for (const [key, value] of Object.entries(applied)) if (value !== null && value !== undefined && value !== '' && value !== false) query.set(key, String(value))
  // An empty requested tier is meaningful, unlike an unset tier selector.
  if (applied.service_tier === '') query.set('service_tier', '')
  try {
    const result = await api('GET', `/logs?${query}`, undefined, { signal: controller.signal })
    if (current !== serial || session !== getKeyEpoch()) return
    items.value = result.items; total.value = result.total; updated.value = Date.now(); newer.value = false
  } catch (e) { if (current === serial && session === getKeyEpoch()) error.value = e.message }
  finally { if (current === serial) loading.value = false }
}
const groups = computed(() => {
  const grouped = new Map()
  for (const item of items.value) {
    const key = item.diagnostics?.request_id || `attempt-${item.seq}`
    if (!grouped.has(key)) grouped.set(key, { ...item, key, attempts: [] })
    grouped.get(key).attempts.push(item)
  }
  return [...grouped.values()]
})
async function copyId(id) { if (!await copyText(id)) manualCopy.value = id }
const attemptColumns = [
  { title: '时间（HKT）', key: 'ts', width: 170, render: l => new Date(l.ts * 1000).toLocaleString('zh-CN', { timeZone: 'Asia/Hong_Kong' }) },
  { title: '账号', key: 'account_email', minWidth: 180, render: l => `${l.account_email} · ${l.account_id?.slice(-6) || '未知'}` },
  { title: '模型', key: 'model', width: 140 },
  { title: '请求档位', key: 'service_tier', width: 90, render: l => tierLabel(l.service_tier) },
  { title: '状态', key: 'status', width: 110, render: l => h('span', { class: hasError(l) ? 'err' : 'ok' }, l.status === 0 ? '未收到 HTTP' : `${l.status}${l.status < 400 && l.error ? ' · 异常' : ''}`) },
  { title: '首部耗时', key: 'latency_ms', width: 90, render: l => l.status === 0 ? '—' : `${l.latency_ms}ms` },
  { title: 'in/cached/out', key: 'tokens', width: 150, render: l => `${l.input_tokens}/${l.cached_tokens}/${l.output_tokens}` },
  { title: '错误', key: 'error', render: l => h('span', { title: l.error || '' }, l.error || '—') },
  { title: '诊断', key: 'diagnostics', render: l => l.diagnostics ? h('details', [h('summary', `第 ${l.diagnostics.attempt} 次 · ${l.diagnostics.stage}`), h('pre', { style: 'max-width:480px;overflow-wrap:anywhere' }, JSON.stringify(l.diagnostics, null, 2))]) : '未记录' },
]
const columns = [
  { type: 'expand', renderExpand: row => h(NDataTable, { columns: attemptColumns, data: row.attempts, rowKey: r => r.seq, size: 'small', scrollX: 1200 }) },
  attemptColumns[0],
  { title: '请求 / 本页尝试', key: 'request', minWidth: 170, render: r => h('div', [h('span', `${r.attempts.length} 次 · ${r.diagnostics?.request_id ? r.diagnostics.request_id.slice(0, 12) + '…' : '未记录请求编号'}`), r.diagnostics?.request_id && h(NButton, { size: 'tiny', onClick: () => copyId(r.diagnostics.request_id) }, { default: () => '复制编号' })]) },
  attemptColumns[1], attemptColumns[2],
  { title: 'Key', key: 'api_key', width: 110, render: r => keyLabel(r.api_key) },
  { title: '最近观测', key: 'observed', width: 150, render: r => hasError(r) ? '观察到异常尝试' : r.diagnostics?.completed === true ? '已观察到完成' : '完成状态未知' },
]
onMounted(load)
onUnmounted(() => { serial++; controller?.abort() })
watch(() => props.active, active => { if (active) void load() })
watch(() => store.status.state?.updatedAt, () => {
  if (!props.active) return
  if (page.value === 1 && expanded.value.length === 0) void load()
  else newer.value = true
})
</script>
<template>
  <n-card
    title="请求日志"
    size="small"
  >
    <div
      class="row"
      style="margin-top:0"
    >
      <n-input
        v-model:value="filters.model"
        placeholder="模型（精确匹配）"
        aria-label="日志模型"
        style="width:170px"
      />
      <n-select
        v-model:value="filters.account_id"
        :options="accountOptions"
        clearable
        placeholder="全部账号"
        aria-label="日志账号"
        style="width:230px"
      />
      <n-select
        v-model:value="filters.api_key"
        :options="keyOptions"
        clearable
        placeholder="全部 Key"
        aria-label="日志 Key"
        style="width:170px"
      />
      <n-select
        v-model:value="filters.service_tier"
        :options="tiers"
        clearable
        placeholder="全部档位"
        aria-label="日志档位"
        style="width:150px"
      />
      <n-checkbox v-model:checked="filters.errors_only">
        仅异常尝试
      </n-checkbox>
      <n-input
        v-model:value="filters.request_id"
        placeholder="完整请求编号"
        aria-label="请求编号"
        style="width:230px"
        @keydown.enter="search"
      />
      <n-input
        v-model:value="filters.since"
        placeholder="开始 HKT YYYY-MM-DD HH:mm"
        aria-label="日志开始时间"
        style="width:235px"
      />
      <n-input
        v-model:value="filters.until"
        placeholder="结束 HKT YYYY-MM-DD HH:mm"
        aria-label="日志结束时间"
        style="width:235px"
      />
      <n-button
        :loading="loading"
        @click="search"
      >
        查询
      </n-button>
      <n-button
        :loading="loading"
        @click="load"
      >
        {{ newer ? '有新数据，刷新当前页' : '刷新当前页' }}
      </n-button>
    </div>
    <p class="muted">
      按尝试记录分页，再按请求编号合并本页记录；可能跨页，最近观测不等于请求最终结果。异常尝试可能已由后续重试恢复。筛选覆盖全部保留历史，空结果不代表从未发生。
    </p>
    <n-alert
      v-if="error"
      type="error"
      style="margin-bottom:10px"
    >
      {{ error }}。{{ updated ? '保留上次结果。' : '尚无有效查询结果。' }}<n-button
        size="small"
        @click="load"
      >
        重试
      </n-button>
    </n-alert>
    <n-data-table
      v-model:expanded-row-keys="expanded"
      :data="groups"
      :columns="columns"
      :row-key="r => r.key"
      :loading="loading"
      :scroll-x="1100"
      size="small"
    >
      <template #empty>
        {{ loading ? '查询中…' : error ? '查询失败' : '没有匹配记录' }}
      </template>
    </n-data-table>
    <div class="row">
      <span class="muted">本页 {{ items.length }} 次尝试 / {{ groups.length }} 组 · 匹配历史共 {{ total }} 次尝试</span>
      <n-pagination
        :page="page"
        :page-size="50"
        :item-count="total"
        :disabled="loading"
        @update:page="value => { page = value; expanded = []; load() }"
      />
    </div>
    <n-modal
      :show="!!manualCopy"
      preset="card"
      title="手动复制请求编号"
      style="width:min(520px,95vw)"
      @update:show="v => { if (!v) manualCopy = '' }"
    >
      <n-input
        :value="manualCopy"
        readonly
        @focus="e => e.target?.select?.()"
      />
    </n-modal>
  </n-card>
</template>
