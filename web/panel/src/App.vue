<script setup>
import { ref, onMounted, onUnmounted } from 'vue'
import { NConfigProvider, NMessageProvider, NInput, NTabs, NTabPane, NButton, darkTheme, zhCN, dateZhCN } from 'naive-ui'
import { getKey, setKey } from './api.js'
import { loadState, loadUsage, loadCalibration, loadHistory } from './store.js'
import Accounts from './components/Accounts.vue'
import AddAccount from './components/AddAccount.vue'
import KeyTable from './components/KeyTable.vue'
import Settings from './components/Settings.vue'
import Calibration from './components/Calibration.vue'
import Usage from './components/Usage.vue'
import Logs from './components/Logs.vue'

const mk = ref(getKey())
const conn = ref('')
const tab = ref('stats')
const logsOpen = ref(false)
let timer = null

async function reloadAll() {
  if (!mk.value) { conn.value = '输入 manage key'; return }
  const rs = await Promise.allSettled([loadState(), loadUsage(), loadCalibration(), loadHistory()])
  const firstErr = rs.find(r => r.status === 'rejected')
  conn.value = firstErr ? String(firstErr.reason.message || firstErr.reason) : 'ok'
}

onMounted(() => {
  timer = setInterval(reloadAll, 30000)
  reloadAll()
})
onUnmounted(() => clearInterval(timer))
</script>

<template>
  <n-config-provider
    :theme="darkTheme"
    :locale="zhCN"
    :date-locale="dateZhCN"
    style="min-height:100vh"
  >
    <n-message-provider>
      <header>
        <h1>herdex</h1>
        <n-input
          class="key"
          type="password"
          show-password-on="click"
          placeholder="manage key"
          :value="mk"
          @update:value="v => { mk = v; setKey(v) }"
          @blur="reloadAll"
          @focus="e => e.target?.select?.()"
        />
        <span :class="{ ok: conn === 'ok', err: conn !== 'ok' && conn !== '输入 manage key', muted: conn === '输入 manage key' }">{{ conn === 'ok' ? '已连接' : conn }}</span>
      </header>
      <main v-if="mk">
        <n-tabs
          v-model:value="tab"
          type="line"
          size="large"
          animated
          display-directive="show:lazy"
        >
          <n-tab-pane
            name="stats"
            tab="统计"
            display-directive="show:lazy"
          >
            <Usage />
            <div style="margin-top:14px">
              <div
                class="row"
                style="margin-top:0"
              >
                <n-button
                  quaternary
                  size="small"
                  @click="logsOpen = !logsOpen"
                >
                  {{ logsOpen ? '收起请求日志 ▲' : '展开请求日志 ▼' }}
                </n-button>
              </div>
              <Logs
                v-if="logsOpen"
                style="margin-top:8px"
              />
            </div>
            <Calibration style="margin-top:14px" />
          </n-tab-pane>
          <n-tab-pane
            name="accounts"
            tab="账号"
            display-directive="show:lazy"
          >
            <Accounts />
            <AddAccount style="margin-top:14px" />
          </n-tab-pane>
          <n-tab-pane
            name="keys"
            tab="Key"
            display-directive="show:lazy"
          >
            <KeyTable />
          </n-tab-pane>
          <n-tab-pane
            name="settings"
            tab="设置"
            display-directive="show:lazy"
          >
            <Settings />
          </n-tab-pane>
        </n-tabs>
      </main>
    </n-message-provider>
  </n-config-provider>
</template>
