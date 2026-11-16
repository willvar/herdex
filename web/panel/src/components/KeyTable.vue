<script setup>
import { computed, h, reactive, ref } from 'vue'
import { NCard, NDataTable, NButton, NInput, NPopconfirm, NModal, NAlert, useMessage } from 'naive-ui'
import { store, mutate, refreshAfterMutation } from '../store.js'
import { copyText } from '../clipboard.js'
const message = useMessage()
const keys = computed(() => store.st?.api_keys || [])
const nk = ref(''), nl = ref(''), adding = ref(false), saving = ref(false)
const pending = reactive({})
const editor = ref(null)
const copyValue = ref('')
const receipt = ref('')
const mask = key => key.length > 8 ? `${key.slice(0, 4)}…${key.slice(-4)}` : '••••••••'
async function refreshed(label) {
  receipt.value = label
  if (!await refreshAfterMutation()) message.warning(`${label}，但列表刷新失败；请重试刷新，不要重复操作`)
  else message.success(label)
}
async function addKey() {
  if (adding.value || !nk.value) return
  adding.value = true
  const payload = { key: nk.value, comment: nl.value }
  try {
    await mutate('POST', '/keys', payload)
    if (nk.value === payload.key && nl.value === payload.comment) { nk.value = ''; nl.value = '' }
    await refreshed('Key 已添加')
  } catch (e) { message.error(e.message) }
  finally { adding.value = false }
}
function editKey(key, mode) {
  editor.value = { original: key.key, label: key.comment || mask(key.key), mode, comment: key.comment || '', key: '' }
}
async function saveEdit() {
  if (saving.value || !editor.value) return
  const edit = { ...editor.value }
  if (edit.mode === 'rotate' && !edit.key) return
  saving.value = true
  try {
    await mutate('PATCH', `/keys/${encodeURIComponent(edit.original)}`, edit.mode === 'comment' ? { comment: edit.comment } : { key: edit.key })
    editor.value = null
    await refreshed(edit.mode === 'comment' ? '注释已保存' : 'Key 已更换，请更新客户端凭据')
  } catch (e) { message.error(e.message) }
  finally { saving.value = false }
}
async function rowAction(key, method, body, label) {
  if (pending[key.key]) return
  pending[key.key] = true
  try { await mutate(method, `/keys/${encodeURIComponent(key.key)}`, body); await refreshed(label) }
  catch (e) { message.error(e.message) }
  finally { delete pending[key.key] }
}
async function copyKey(key, suffix = '') {
  const value = key.key + suffix
  if (await copyText(value)) message.success('已复制')
  else { copyValue.value = value; message.warning('浏览器拒绝复制，请在弹窗中手动选择复制') }
}
const columns = [
  { title: 'Key', key: 'key', minWidth: 130, render: r => mask(r.key) },
  { title: '注释', key: 'comment', minWidth: 120, render: r => r.comment || '—' },
  { title: '状态', key: 'disabled', width: 70, render: r => r.disabled ? '已禁用' : '已启用' },
  { title: '操作', key: 'actions', minWidth: 440, render: r => h('span', { class: 'row', style: 'margin:0' }, [
    ...[['普通', ''], ['Fast', '-fast'], ['Ultrafast', '-ultrafast']].map(([label, suffix]) => h(NButton, { size: 'tiny', onClick: () => copyKey(r, suffix) }, { default: () => `复制 ${label}` })),
    h(NButton, { size: 'tiny', disabled: !!pending[r.key], onClick: () => editKey(r, 'comment') }, { default: () => '编辑注释' }),
    h(NButton, { size: 'tiny', disabled: !!pending[r.key], onClick: () => editKey(r, 'rotate') }, { default: () => '更换 Key' }),
    h(NButton, { size: 'tiny', loading: !!pending[r.key], onClick: () => rowAction(r, 'PATCH', { disabled: !r.disabled }, r.disabled ? 'Key 已启用' : 'Key 已禁用') }, { default: () => r.disabled ? '启用' : '禁用' }),
    h(NPopconfirm, { onPositiveClick: () => rowAction(r, 'DELETE', undefined, 'Key 已删除') }, {
      trigger: () => h(NButton, { size: 'tiny', type: 'error', ghost: true, disabled: !!pending[r.key] }, { default: () => '删除' }),
      default: () => `删除 ${r.comment || mask(r.key)}？普通及派生形式会同时失效。`,
    }),
  ]) },
]
</script>
<template>
  <n-card
    title="API Keys"
    size="small"
  >
    <p class="muted">
      普通、Fast、Ultrafast 使用同一基础 Key，共享启停和删除状态。派生后缀不是独立凭据，也不授予模型权限；基础 Key 不能以 -fast 或 -ultrafast 结尾。
    </p>
    <n-alert
      v-if="receipt"
      type="success"
      style="margin-bottom:10px"
    >
      {{ receipt }}
    </n-alert>
    <n-data-table
      :data="keys"
      :columns="columns"
      :row-key="r => r.key"
      size="small"
      :scroll-x="850"
    >
      <template #empty>
        暂无 Key
      </template>
    </n-data-table>
    <div class="row">
      <n-input
        v-model:value="nk"
        type="password"
        show-password-on="click"
        placeholder="sk-..."
        :input-props="{ 'aria-label': '新 Key' }"
        style="flex:1"
      />
      <n-input
        v-model:value="nl"
        placeholder="注释"
        :input-props="{ 'aria-label': '新 Key 注释' }"
        style="width:160px"
      />
      <n-button
        :loading="adding"
        :disabled="adding || !nk"
        @click="addKey"
      >
        添加
      </n-button>
    </div>
    <n-modal
      :show="!!editor"
      preset="card"
      :title="editor?.mode === 'comment' ? '编辑 Key 注释' : '更换 Key'"
      style="width:min(520px,95vw)"
      :mask-closable="!saving"
      :closable="!saving"
      @update:show="v => { if (!v && !saving) editor = null }"
    >
      <template v-if="editor">
        <p class="muted">
          {{ editor.label }}
        </p>
        <n-input
          v-if="editor.mode === 'comment'"
          v-model:value="editor.comment"
          :disabled="saving"
          placeholder="注释（留空可清除）"
          :input-props="{ 'aria-label': '编辑注释' }"
          @keydown.enter="saveEdit"
        />
        <template v-else>
          <n-alert
            type="warning"
            style="margin-bottom:12px"
          >
            更换后旧 Key 及其派生形式失效，现有客户端必须更新凭据。此操作不是修改注释。
          </n-alert>
          <n-input
            v-model:value="editor.key"
            type="password"
            show-password-on="click"
            :disabled="saving"
            placeholder="新的基础 Key"
            :input-props="{ 'aria-label': '新的基础 Key' }"
          />
        </template>
        <div class="row">
          <n-button
            type="primary"
            :loading="saving"
            :disabled="saving || (editor.mode === 'rotate' && !editor.key)"
            @click="saveEdit"
          >
            {{ editor.mode === 'comment' ? '保存注释' : '确认更换 Key' }}
          </n-button>
          <n-button
            :disabled="saving"
            @click="editor = null"
          >
            取消
          </n-button>
        </div>
      </template>
    </n-modal>
    <n-modal
      :show="!!copyValue"
      preset="card"
      title="手动复制凭据"
      style="width:min(520px,95vw)"
      @update:show="v => { if (!v) copyValue = '' }"
    >
      <p>仅在需要的客户端中使用，关闭后不再显示。</p>
      <n-input
        :value="copyValue"
        readonly
        :input-props="{ 'aria-label': '待复制凭据' }"
        @focus="e => e.target?.select?.()"
      />
      <n-button
        style="margin-top:12px"
        @click="copyValue = ''"
      >
        关闭
      </n-button>
    </n-modal>
  </n-card>
</template>
