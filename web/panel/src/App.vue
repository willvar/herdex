<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { NConfigProvider, NMessageProvider, NInput, NTabs, NTabPane, NButton, NAlert, darkTheme, zhCN, dateZhCN } from 'naive-ui'
import { getKey, getKeyEpoch, onKeyChange, setKey } from './api.js'
import { store, loadState, loadUsage, loadCalibration, loadHistory } from './store.js'
import Accounts from './components/Accounts.vue'
import AddAccount from './components/AddAccount.vue'
import KeyTable from './components/KeyTable.vue'
import Settings from './components/Settings.vue'
import Calibration from './components/Calibration.vue'
import Usage from './components/Usage.vue'
import Logs from './components/Logs.vue'
const mk = ref(getKey()), tab = ref('stats'), loading = ref(false), target = ref('')
let timer
const unsubscribe = onKeyChange(() => { loading.value = false; target.value = '' })
const labels = { state: '账号和 Key', usage7: '7 天用量', usage30: '30 天用量', calibration: '容量估算', history: '趋势历史' }
const failures = computed(() => Object.entries(store.status).filter(([, s]) => s.error))
const connection = computed(() => store.authError || (!getKey() ? '未连接' : loading.value ? '更新中…' : !store.ready ? '尚未取得管理数据' : failures.value.length ? '已认证 · 部分更新失败' : '已连接'))
async function reloadAll() {
  if (!getKey() || loading.value) return
  const session = getKeyEpoch()
  loading.value = true
  try {
    if (!store.ready) await loadState()
    if (getKey()) await Promise.allSettled([loadState(), loadUsage(), loadCalibration(), loadHistory()])
  } catch { /* module status carries the error; authentication errors clear the view */ }
  finally { if (session === getKeyEpoch()) loading.value = false }
}
async function connect() { setKey(mk.value); await reloadAll() }
function disconnect() { setKey(''); mk.value = ''; target.value = '' }
async function retry(name) {
  const loaders = { state: loadState, usage7: () => loadUsage(7), usage30: () => loadUsage(30), calibration: loadCalibration, history: loadHistory }
  try { await loaders[name]?.() } catch { /* visible in module status */ }
}
onMounted(() => { timer = setInterval(reloadAll, 30000); void reloadAll() })
onUnmounted(() => { clearInterval(timer); unsubscribe() })
</script>
<template>
  <n-config-provider
    :theme="darkTheme"
    :locale="zhCN"
    :date-locale="dateZhCN"
    style="min-height:100vh"
  >
    <n-message-provider>
      <header style="flex-wrap:wrap">
        <h1>herdex</h1>
        <n-input
          v-model:value="mk"
          class="key"
          type="password"
          show-password-on="click"
          placeholder="manage key"
          :input-props="{ 'aria-label': '管理凭据' }"
          @keydown.enter="connect"
        />
        <n-button
          :disabled="loading || !mk"
          @click="connect"
        >
          连接 / 更换凭据
        </n-button>
        <n-button
          :disabled="!store.ready || loading"
          :loading="loading"
          @click="reloadAll"
        >
          刷新
        </n-button>
        <n-button @click="disconnect">
          断开并清除凭据
        </n-button>
        <span
          :class="failures.length || store.authError ? 'err' : store.ready ? 'ok' : 'muted'"
          role="status"
        >{{ connection }}</span>
        <a
          href="/ca.pem"
          download="herdex-ca.pem"
          class="muted"
        >下载 CA 证书</a>
      </header>
      <main>
        <n-alert
          v-for="[name, status] in failures"
          :key="name"
          type="error"
        >
          {{ labels[name] }}更新失败：{{ status.error }}。{{ status.updatedAt ? '当前展示上次成功的数据。' : '尚未取得有效数据，不代表用量为零。' }}
          <n-button
            size="small"
            :loading="status.loading"
            @click="retry(name)"
          >
            重试
          </n-button>
        </n-alert>
        <template v-if="store.ready">
          <div
            class="muted"
            style="font-size:12px"
          >
            管理数据更新于 {{ new Date(store.status.state?.updatedAt).toLocaleString('zh-CN', { timeZone: 'Asia/Hong_Kong' }) }} HKT · 自动刷新间隔 30 秒
          </div>
          <n-tabs
            v-model:value="tab"
            type="line"
            size="large"
          >
            <n-tab-pane
              name="stats"
              tab="统计"
              display-directive="show:lazy"
            >
              <Usage /><Calibration style="margin-top:14px" />
            </n-tab-pane>
            <n-tab-pane
              name="accounts"
              tab="账号"
              display-directive="show:lazy"
            >
              <Accounts @reauthorize="id => target = id" /><AddAccount
                :expected-id="target"
                style="margin-top:14px"
              />
            </n-tab-pane>
            <n-tab-pane
              name="keys"
              tab="Key"
              display-directive="show:lazy"
            >
              <KeyTable />
            </n-tab-pane>
            <n-tab-pane
              name="logs"
              tab="请求日志"
              display-directive="show:lazy"
            >
              <Logs :active="tab === 'logs'" />
            </n-tab-pane>
            <n-tab-pane
              name="settings"
              tab="设置"
              display-directive="show:lazy"
            >
              <Settings />
            </n-tab-pane>
          </n-tabs>
        </template>
        <p
          v-else
          class="muted"
        >
          输入管理凭据并连接后查看数据。清除凭据会同时清除当前页面的管理数据。
        </p>
      </main>
    </n-message-provider>
  </n-config-provider>
</template>
