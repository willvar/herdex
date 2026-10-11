import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DOMWrapper, flushPromises, mount } from '@vue/test-utils'
import { api } from '../api.js'
import { store } from '../store.js'
import Accounts from '../components/Accounts.vue'

vi.mock('../api.js', () => ({ api: vi.fn(), getKey: () => 'test-manage-key' }))
const messages = vi.hoisted(() => ({ success: vi.fn(), error: vi.fn() }))
// Exercise the real confirmation, buttons and expanded table row.
vi.mock('naive-ui', async importOriginal => ({
  ...await importOriginal(),
  useMessage: () => messages,
}))

const account = { id: 'a', email: 'a@example.test' }
const quota = (used = 95, available = 2) => ({
  main: { primary: { used_pct: used, window_seconds: 18000 }, secondary: {} },
  credits: { available, applicable: available },
})
const state = () => ({ accounts: [account], quota: {} })
const triggerText = '消耗最早过期券并重置'
const confirmText = '确认消耗 1 张并重置'
let wrapper

function button(text) {
  const element = [...document.querySelectorAll('button')].find(button => button.textContent.trim() === text)
  expect(element, `button: ${text}`).toBeTruthy()
  return new DOMWrapper(element)
}

async function confirmReset() {
  await button(triggerText).trigger('click')
  await flushPromises()
  await button(confirmText).trigger('click')
  await flushPromises()
}

beforeEach(() => {
  vi.clearAllMocks()
  api.mockReset().mockResolvedValue(state())
  Object.assign(store, {
    st: state(), liveQuota: { [account.email]: quota() }, quotaErr: {},
    quotaOpen: { a: true }, quotaLoading: {},
  })
  wrapper = mount(Accounts, { attachTo: document.body })
})

afterEach(() => {
  wrapper.unmount()
  document.body.innerHTML = ''
})

describe('Reset-credit confirmation and outcome', () => {
  it('requires explicit confirmation and cancellation consumes nothing', async () => {
    await button(triggerText).trigger('click')
    await flushPromises()
    expect(document.body.textContent).toContain(`确认消耗账号 ${account.email} 的 1 张最早过期可用券`)
    expect(document.body.textContent).toContain('不可撤销')
    expect(api).not.toHaveBeenCalled()
    expect(store.quotaOpen.a).toBe(true)

    await button('取消').trigger('click')
    await flushPromises()
    expect(api).not.toHaveBeenCalled()
    expect(messages.success).not.toHaveBeenCalled()
  })

  it('locks pending actions and leaves a success receipt with the refreshed quota and credit count', async () => {
    let finish
    api.mockReturnValueOnce(new Promise(resolve => { finish = resolve }))
    await confirmReset()
    expect(api).toHaveBeenCalledExactlyOnceWith('POST', '/accounts/a/quota/consume')
    expect(wrapper.text()).toContain('正在消耗 1 张最早过期券并重置')
    expect(button('正在重置…').element.disabled).toBe(true)
    expect(button('刷新额度').element.disabled).toBe(true)
    expect(button(confirmText).element.disabled).toBe(true)
    expect(button('取消').element.disabled).toBe(true)
    await button(confirmText).trigger('click')
    // Also guard repeated confirmation events before a DOM update can disable the button.
    const confirmation = wrapper.findAllComponents({ name: 'Popconfirm' })
      .find(component => component.props('positiveText') === confirmText)
    await confirmation.props('onPositiveClick')()
    expect(api).toHaveBeenCalledTimes(1)
    await button('一键展开/收起').trigger('click')
    await button('一键展开/收起').trigger('click')
    expect(api).toHaveBeenCalledTimes(1)

    api.mockResolvedValueOnce(quota(0, 1))
    finish({ consumed: true })
    await flushPromises()
    expect(wrapper.get('[role="status"]').text()).toBe('重置成功：已消耗 1 张最早过期券。')
    expect(wrapper.text()).toContain('100%')
    expect(wrapper.text()).toContain('banked 重置券：1')
    expect(button(triggerText).element.disabled).toBe(false)
    expect(messages.success).toHaveBeenCalledOnce()
    expect(api).toHaveBeenCalledTimes(2)
    expect(api).toHaveBeenLastCalledWith('POST', '/accounts/a/quota')
  })

  it.each(['empty', 'refreshing', 'stale'])('blocks redemption when quota is %s', async condition => {
    if (condition === 'empty') store.liveQuota[account.email].credits.available = 0
    if (condition === 'refreshing') store.quotaLoading[account.email] = true
    if (condition === 'stale') store.quotaErr[account.email] = 'usage unavailable'
    await flushPromises()
    expect(button(triggerText).element.disabled).toBe(true)
    await button(triggerText).trigger('click')
    expect(api).not.toHaveBeenCalled()
  })

  it('keeps an actionable error visible without claiming success or resubmitting', async () => {
    api.mockRejectedValueOnce(new Error('upstream timeout'))
    await confirmReset()
    expect(wrapper.get('[role="alert"]').text()).toBe('重置请求失败：upstream timeout')
    expect(store.liveQuota[account.email].credits.available).toBe(2)
    expect(messages.error).toHaveBeenCalledOnce()
    expect(messages.success).not.toHaveBeenCalled()
    expect(api).toHaveBeenCalledTimes(1)
  })

  it('reports reset success immediately and keeps it successful if the separate quota refresh fails', async () => {
    let failQuota
    api.mockResolvedValueOnce({ consumed: true })
      .mockReturnValueOnce(new Promise((_resolve, reject) => { failQuota = reject }))
    await confirmReset()
    expect(wrapper.text()).toContain('重置成功：已消耗 1 张最早过期券。')
    expect(wrapper.text()).toContain('查询中…')
    expect(wrapper.text()).not.toContain('正在重置…')
    expect(messages.success).toHaveBeenCalledOnce()

    failQuota(new Error('usage endpoint 503'))
    await flushPromises()
    expect(wrapper.get('[role="status"]').text()).toBe('重置成功：已消耗 1 张最早过期券。')
    expect(wrapper.get('[role="alert"]').text()).toBe('查询失败：usage endpoint 503')
    expect(wrapper.text()).toContain('可能已过期')
    expect(button(triggerText).element.disabled).toBe(true)
    expect(messages.error).not.toHaveBeenCalled()

    api.mockResolvedValueOnce(quota(0, 1))
    await button('重试').trigger('click')
    await flushPromises()
    expect(api).toHaveBeenLastCalledWith('POST', '/accounts/a/quota')
    expect(api.mock.calls.filter(([, path]) => path.endsWith('/consume'))).toHaveLength(1)
    expect(wrapper.text()).toContain('banked 重置券：1')
    expect(button(triggerText).element.disabled).toBe(false)
  })

  it.each([null, {}, { consumed: false }])('does not announce success for an invalid result: %j', async response => {
    api.mockResolvedValueOnce(response)
    await confirmReset()
    expect(wrapper.get('[role="alert"]').text()).toContain('未收到有效的重置结果')
    expect(messages.success).not.toHaveBeenCalled()
    expect(api).toHaveBeenCalledTimes(1)
  })
})
