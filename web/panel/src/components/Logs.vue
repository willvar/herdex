<script setup>
import { computed, h, ref } from 'vue'
import { NCard, NDataTable, NSelect, NCheckbox } from 'naive-ui'
import { store } from '../store.js'

const all = computed(() => (store.st?.logs || []).slice(0, 50))
const fModel = ref(null)
const fErrOnly = ref(false)

const modelOptions = computed(() => [...new Set(all.value.map(l => l.model).filter(Boolean))].map(m => ({ label: m, value: m })))
const logs = computed(() => all.value.filter(l =>
  (!fModel.value || l.model === fModel.value) &&
  (!fErrOnly.value || hasError(l))
))

function hasError(log) {
  return log.status === 0 || log.status >= 400 || !!log.error
}

function tierLabel(tier) {
  if (tier == null) return '未记录'
  if (tier === 'priority' || tier === 'fast') return 'Fast'
  return tier || '未指定'
}

const logColumns = [
  { title: '时间', key: 'ts', width: 90, render: l => new Date(l.ts * 1000).toLocaleTimeString() },
  { title: '账号', key: 'account_email', width: 180 },
  { title: '模型', key: 'model', width: 120 },
  { title: '请求档位', key: 'service_tier', width: 100, render: l => tierLabel(l.service_tier) },
  { title: '状态', key: 'status', width: 110, render: l => h('span', { class: hasError(l) ? 'err' : 'ok' }, l.status === 0 ? '未收到 HTTP' : `${l.status}${l.status < 400 && l.error ? ' · 异常' : ''}`) },
  { title: '首部耗时', key: 'latency_ms', width: 90, render: l => l.status === 0 ? '-' : `${l.latency_ms}ms` },
  { title: 'in/cached/out', key: 'tokens', width: 150, render: l => `${l.input_tokens}/${l.cached_tokens}/${l.output_tokens}` },
  { title: '错误', key: 'error', render: l => h('span', { class: 'muted', title: l.error || '' }, (l.error || '').slice(0, 50)) },
  { title: '诊断', key: 'diagnostics', render: l => l.diagnostics ? h('details', [
    h('summary', `第 ${l.diagnostics.attempt} 次 · ${l.diagnostics.stage}`),
    h('pre', { style: 'white-space:pre-wrap;overflow-wrap:anywhere;max-width:480px' }, JSON.stringify(l.diagnostics, null, 2)),
  ]) : '未记录' },
]
</script>

<template>
  <n-card
    title="最近请求"
    size="small"
  >
    <template #header-extra>
      <span
        class="muted"
        style="font-size:12px;margin-right:10px"
      >{{ logs.length }}/{{ all.length }} 条</span>
      <n-select
        v-model:value="fModel"
        :options="modelOptions"
        placeholder="全部模型"
        clearable
        size="small"
        style="width:180px"
      />
      <n-checkbox
        v-model:checked="fErrOnly"
        style="margin-left:10px"
      >
        仅错误
      </n-checkbox>
    </template>
    <n-data-table
      :data="logs"
      :columns="logColumns"
      size="small"
      :row-class-name="l => hasError(l) ? 'log-err' : ''"
    >
      <template #empty>
        暂无请求
      </template>
    </n-data-table>
  </n-card>
</template>

<style>
.log-err td { background: rgba(224, 112, 112, 0.08); }
</style>
