<script setup>
import { computed, h, ref } from 'vue'
import { NCard, NDataTable, NSelect, NCheckbox } from 'naive-ui'
import { esc } from '../format.js'
import { store } from '../store.js'

const all = computed(() => (store.st?.logs || []).slice(0, 50))
const fModel = ref(null)
const fErrOnly = ref(false)

const modelOptions = computed(() => [...new Set(all.value.map(l => l.model).filter(Boolean))].map(m => ({ label: m, value: m })))
const logs = computed(() => all.value.filter(l =>
  (!fModel.value || l.model === fModel.value) &&
  (!fErrOnly.value || l.status >= 400 || (l.error || '') !== '')
))

const logColumns = [
  { title: '时间', key: 'ts', width: 90, render: l => new Date(l.ts * 1000).toLocaleTimeString() },
  { title: '账号', key: 'account_email', width: 180 },
  { title: '模型', key: 'model', width: 120 },
  { title: '状态', key: 'status', width: 80, render: l => h('span', { class: l.status < 400 ? 'ok' : 'err' }, String(l.status)) },
  { title: '耗时', key: 'latency_ms', width: 80, render: l => `${l.latency_ms}ms` },
  { title: 'in/cached/out', key: 'tokens', width: 150, render: l => `${l.input_tokens}/${l.cached_tokens}/${l.output_tokens}` },
  { title: '错误', key: 'error', render: l => h('span', { class: 'muted' }, esc((l.error || '').slice(0, 50))) },
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
      :row-class-name="l => l.status >= 400 ? 'log-err' : ''"
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
