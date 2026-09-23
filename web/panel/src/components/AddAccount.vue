<script setup>
import { ref } from 'vue'
import { NCard, NButton, NInput, useMessage } from 'naive-ui'
import { api } from '../api.js'
import { loadState } from '../store.js'

const message = useMessage()
const authUrl = ref('')
const cbUrl = ref('')

async function loginStart() {
  try {
    const r = await api('POST', '/login/start')
    authUrl.value = r.auth_url
    message.info('授权链接已生成，可在任意设备/浏览器打开；完成后把最后跳转的完整 URL 粘贴到下方')
  } catch (e) { message.error(e.message) }
}
async function copyLink() {
  try {
    if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(authUrl.value)
    else {
      const ta = document.createElement('textarea')
      ta.value = authUrl.value; document.body.appendChild(ta); ta.select()
      document.execCommand('copy'); document.body.removeChild(ta)
    }
    message.success('已复制')
  } catch { message.error('复制失败') }
}
async function loginFinish() {
  try {
    const r = await api('POST', '/login/finish', { callback_url: cbUrl.value })
    message.success(`已添加 ${r.email} (${r.plan_type})`)
    cbUrl.value = ''
    loadState()
  } catch (e) { message.error(e.message) }
}
</script>

<template>
  <n-card
    title="添加账号（OAuth）"
    size="small"
  >
    <div
      class="row"
      style="margin-top:0"
    >
      <n-button
        size="small"
        @click="loginStart"
      >
        生成授权链接
      </n-button>
      <a
        v-if="authUrl"
        :href="authUrl"
        target="_blank"
      >点此打开授权页</a>
      <n-button
        v-if="authUrl"
        size="small"
        @click="copyLink"
      >
        复制链接
      </n-button>
    </div>
    <div class="row">
      <n-input
        v-model:value="cbUrl"
        type="textarea"
        :rows="2"
        style="flex:1"
        placeholder="浏览器最后重定向到的 localhost:1455/auth/callback?... 完整 URL 粘贴到这里"
      />
    </div>
    <div class="row">
      <n-button
        size="small"
        :disabled="!cbUrl"
        @click="loginFinish"
      >
        完成登录
      </n-button>
    </div>
  </n-card>
</template>
