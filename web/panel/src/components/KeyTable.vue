<script setup>
import { computed, ref } from 'vue'
import { h } from 'vue'
import { NCard, NDataTable, NButton, NInput, NPopconfirm, useMessage } from 'naive-ui'
import { api } from '../api.js'
import { store, loadState } from '../store.js'

const message = useMessage()
const keys = computed(() => store.st?.api_keys || [])
const nk = ref('')
const nl = ref('')

async function addKey() {
  try { await api('POST', '/keys', { key: nk.value, comment: nl.value }); nk.value = ''; nl.value = ''; loadState() }
  catch (e) { message.error(e.message) }
}
function delKey(k) {
  api('DELETE', `/keys/${encodeURIComponent(k.key)}`).then(loadState).catch(e => message.error(e.message))
}
async function toggleKey(k, dis) {
  try { await api('PATCH', `/keys/${encodeURIComponent(k.key)}`, { disabled: dis }); loadState() } catch (e) { message.error(e.message) }
}
async function editKey(k) {
  const nk2 = prompt('新 key 值（不改请保持原值）:', k.key)
  if (nk2 === null) return
  const cm = prompt('注释:', k.comment || '')
  if (cm === null) return
  try { await api('PATCH', `/keys/${encodeURIComponent(k.key)}`, { key: nk2, comment: cm }); loadState() } catch (e) { message.error(e.message) }
}

const columns = [
  { title: 'Key', key: 'key', minWidth: 200 },
  { title: '注释', key: 'comment', width: 140 },
  { title: '创建', key: 'created_at', width: 120 },
  { title: '操作', key: 'actions', width: 220, render: (r) =>
    h('span', { style: 'display:inline-flex;gap:8px' }, [
      h(NButton, { size: 'tiny', onClick: () => editKey(r) }, { default: () => '编辑' }),
      h(NButton, { size: 'tiny', onClick: () => toggleKey(r, !r.disabled) }, { default: () => r.disabled ? '启用' : '禁用' }),
      h(NPopconfirm, { onPositiveClick: () => delKey(r) }, {
        trigger: () => h(NButton, { size: 'tiny', type: 'error', ghost: true }, { default: () => '删除' }),
        default: () => '删除该 Key？',
      }),
    ]) },
]
</script>

<template>
  <n-card
    title="API Keys"
    size="small"
  >
    <n-data-table
      :data="keys"
      :columns="columns"
      size="small"
      :row-class-name="r => r.disabled ? 'key-disabled' : ''"
    >
      <template #empty>
        暂无 Key
      </template>
    </n-data-table>
    <div class="row">
      <n-input
        v-model:value="nk"
        placeholder="sk-..."
        style="flex:1"
      />
      <n-input
        v-model:value="nl"
        placeholder="注释"
        style="width:160px"
      />
      <n-button @click="addKey">
        添加
      </n-button>
    </div>
  </n-card>
</template>

<style>
.key-disabled { opacity: 0.45; }
</style>
