<script setup>
import { computed, ref, watch } from 'vue'
import { NCard, NButton, NInput, NSelect, NAlert, useMessage } from 'naive-ui'
import { api } from '../api.js'
import { planLabel } from '../plans.js'
import { store, mutate, refreshAfterMutation } from '../store.js'
import { copyText } from '../clipboard.js'
const props = defineProps({ expectedId: { type: String, default: '' } })
const message = useMessage()
const authUrl = ref(''), cbUrl = ref(''), receipt = ref(''), problem = ref('')
const starting = ref(false), finishing = ref(false), manualCopy = ref(false)
const selected = ref(null)
const options = computed(() => (store.st?.accounts || []).map(a => ({ value: a.id, label: `${a.email} · ${a.workspace_id?.slice(-6) || a.id.slice(-6)}${a.disabled ? '（禁用）' : ''}` })))
watch(() => props.expectedId, id => { if (!starting.value && !finishing.value) { selected.value = id || null; authUrl.value = ''; cbUrl.value = ''; problem.value = '' } }, { immediate: true })
function discardLink() { authUrl.value = ''; cbUrl.value = ''; problem.value = ''; manualCopy.value = false }
async function loginStart() {
  if (starting.value || finishing.value) return
  starting.value = true
  authUrl.value = ''; cbUrl.value = ''; problem.value = ''; manualCopy.value = false
  try { const result = await api('POST', '/login/start'); authUrl.value = result.auth_url }
  catch (error) { problem.value = error.message }
  finally { starting.value = false }
}
async function copyLink() {
  if (await copyText(authUrl.value)) message.success('已复制授权链接')
  else { manualCopy.value = true; message.warning('复制被拒绝，请手动选择链接') }
}
async function loginFinish() {
  if (finishing.value || !authUrl.value) return
  try {
    const callback = new globalThis.URL(cbUrl.value)
    const state = new globalThis.URL(authUrl.value).searchParams.get('state')
    if (callback.protocol !== 'http:' || !['localhost', '127.0.0.1'].includes(callback.hostname) || callback.port !== '1455' || callback.pathname !== '/auth/callback' || !callback.searchParams.get('code') || callback.searchParams.get('state') !== state) throw new Error('请粘贴本次授权最后跳转的完整 localhost:1455/auth/callback 地址')
  } catch (error) { problem.value = error.message; return }
  finishing.value = true; problem.value = ''; receipt.value = ''
  try {
    const result = await mutate('POST', '/login/finish', { callback_url: cbUrl.value, expected_id: selected.value || undefined })
    receipt.value = `${result.updated ? '已更新凭据' : '已添加'}：${result.email}（${planLabel(result.plan_type)}）${result.disabled ? '，账号保持禁用' : ''}`
    authUrl.value = ''; cbUrl.value = ''; manualCopy.value = false
    if (!await refreshAfterMutation({ capacity: true })) message.warning('授权已完成，但账号池刷新失败，请刷新核对，不要重复授权')
    message.success(receipt.value)
  } catch (error) {
    authUrl.value = ''; cbUrl.value = ''; manualCopy.value = false
    problem.value = `${error.message}。本次授权不能直接重复提交；请先刷新账号池核对结果，未完成时重新生成授权链接。`
  } finally { finishing.value = false }
}
</script>
<template>
  <n-card
    title="添加 / 更新账号（OAuth）"
    size="small"
  >
    <n-alert
      v-if="receipt"
      type="success"
      style="margin-bottom:10px"
    >
      {{ receipt }}
    </n-alert>
    <n-alert
      v-if="problem"
      type="error"
      style="margin-bottom:10px"
    >
      {{ problem }}
    </n-alert>
    <p class="muted">
      新增身份会单独保存；同一用户与工作区重新授权只更新凭据，保留启停状态。授权链接 10 分钟有效。
    </p>
    <n-select
      v-model:value="selected"
      :options="options"
      clearable
      :disabled="starting || finishing || !!authUrl"
      placeholder="新增 / 自动匹配，或选择要更新的账号"
      aria-label="授权目标账号"
    />
    <div class="row">
      <span>1. 生成并打开授权链接</span>
      <n-button
        :loading="starting"
        :disabled="starting || finishing"
        @click="loginStart"
      >
        {{ authUrl ? '重新生成授权链接' : '生成授权链接' }}
      </n-button>
      <n-button
        v-if="authUrl"
        :disabled="starting || finishing"
        @click="discardLink"
      >
        放弃此链接
      </n-button>
      <a
        v-if="authUrl"
        :href="authUrl"
        target="_blank"
        rel="noopener noreferrer"
      >打开授权页</a>
      <n-button
        v-if="authUrl"
        :disabled="finishing"
        @click="copyLink"
      >
        复制链接
      </n-button>
    </div>
    <n-input
      v-if="manualCopy && authUrl"
      :value="authUrl"
      readonly
      :input-props="{ 'aria-label': '手动复制授权链接' }"
      @focus="e => e.target?.select?.()"
    />
    <p class="muted">
      2. 完成授权后，即使 localhost 页面无法打开，也请复制地址栏中的完整回调地址。可跨设备授权。
    </p>
    <n-input
      v-model:value="cbUrl"
      type="textarea"
      :rows="2"
      :disabled="!authUrl || finishing"
      placeholder="本次授权的 localhost:1455/auth/callback?... 完整 URL"
      :input-props="{ 'aria-label': '授权回调地址' }"
    />
    <div class="row">
      <span>3. 保存授权结果</span><n-button
        :loading="finishing"
        :disabled="!authUrl || !cbUrl || finishing"
        @click="loginFinish"
      >
        完成登录
      </n-button>
    </div>
  </n-card>
</template>
