<script setup>
import { ref, watchEffect } from 'vue'
import { NCard, NButton, NInputNumber, NSelect, useMessage } from 'naive-ui'
import { api } from '../api.js'
import { store, commitSettings } from '../store.js'

const message = useMessage()
const cooldown = ref(300)
const yieldGap = ref(20)

// only sync server values into the form while the user hasn't typed —
// the 30s state poll must not clobber an unsaved draft
const dirty = ref(false)
const saving = ref(false)
watchEffect(() => {
  const s = store.st?.settings
  if (s && !dirty.value) {
    cooldown.value = s.cooldown_seconds ?? 300
    yieldGap.value = s.pin_yield_gap_pp ?? 20
  }
})
function touch() { dirty.value = true }
const yieldOptions = [
  { label: '-1 黏性优先', value: -1 },
  { label: '0 水填均衡', value: 0 },
  { label: '滞后带（输入百分点）', value: '__custom__' },
]
const yieldMode = ref(-1)
watchEffect(() => {
  const v = yieldGap.value
  if (v === -1 || v === 0) yieldMode.value = v
  else if (typeof v === 'number' && v > 0) yieldMode.value = '__custom__'
})
function onYieldMode(v) {
  dirty.value = true
  if (v === -1) yieldGap.value = -1
  else if (v === 0) yieldGap.value = 0
  else if (v === '__custom__') yieldGap.value = 20
}
async function save() {
  if (saving.value) return
  saving.value = true
  try {
    const payload = {
      cooldown_seconds: cooldown.value ?? 0,
      pin_yield_gap_pp: yieldGap.value,
    }
    await api('PUT', '/settings', payload)
    commitSettings(payload)
    // Preserve edits made while this particular payload was being saved.
    dirty.value = (cooldown.value ?? 0) !== payload.cooldown_seconds ||
      yieldGap.value !== payload.pin_yield_gap_pp
    message.success('已保存')
  } catch (e) { message.error(e.message) }
  finally { saving.value = false }
}
</script>

<template>
  <n-card
    title="设置（池参数）"
    size="small"
  >
    <div
      class="row"
      style="margin-top:0"
    >
      <span class="muted">失败冷却（秒）</span>
      <n-input-number
        v-model:value="cooldown"
        :min="0"
        style="width:120px"
        size="small"
        @update:value="touch"
      />
      <span style="width:24px" />
      <span class="muted">迁移策略</span>
      <n-select
        :value="yieldMode"
        :options="yieldOptions"
        size="small"
        style="width:200px"
        @update:value="onYieldMode"
      />
      <n-input-number
        v-if="yieldMode === '__custom__'"
        v-model:value="yieldGap"
        :min="1"
        size="small"
        style="width:120px"
        @update:value="touch"
      />
    </div>
    <div
      class="muted"
      style="margin-top:10px;line-height:1.6;font-size:12px"
    >
      迁移策略决定会话何时离开粘住的账号（迁移会废弃该会话的 prompt cache）。三个档位是同一根轴上的连续取舍：<b>均衡度</b>（各号已用%是否齐步）↔ <b>cache 损失</b>（迁移次数）↔ <b>首次 429 时间</b>：<br>
      <b>-1 黏性优先</b>：只在一个号 429/失败时才迁移——cache 零损失，消耗串行集中（一次只烧一个号），首次触顶最早；单会话/重 cache 场景推荐；<br>
      <b>0 水填均衡</b>：永远粘在当前已用百分比最低的号上，各号已用%完全对齐、同步逼近 100%——平均分配的极限，首次触顶最晚；多会话分摊/追求均衡时推荐；<br>
      <b>&gt;0 滞后带</b>：介于两者之间——各号已用%保持在一条最厚 N 个百分点的带子里滚动（有界的相对均衡）。例：20 = 粘住的号可以领先别家 20 个百分点，烧满 20 个百分点才搬家。
      迁移次数 ≈ 总消耗 ÷ 该值：值越大迁移越少、越接近黏性优先，越小越接近水填。
    </div>
    <div
      class="muted"
      style="margin-top:6px;line-height:1.6;font-size:12px"
    >
      statusline 固定显示<b>号池虚拟账号</b>的进度：已用% = 各账号已用%按实测容量（tokens/百分点）加权平均——
      校准数据积累越多越精确，未校准阶段退化为等权平均。panel 此处的逐账号百分比始终是真实值。
    </div>
    <div class="row">
      <n-button
        :loading="saving"
        :disabled="saving"
        @click="save"
      >
        保存
      </n-button>
    </div>
  </n-card>
</template>
