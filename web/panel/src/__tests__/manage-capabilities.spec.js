import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DOMWrapper, flushPromises, mount } from '@vue/test-utils'
import { api } from '../api.js'
import { copyText } from '../clipboard.js'
import { store } from '../store.js'
import KeyTable from '../components/KeyTable.vue'
import AddAccount from '../components/AddAccount.vue'
vi.mock('../api.js', async original => ({ ...await original(), api: vi.fn(), getKey: () => 'fixture-manage' }))
vi.mock('../clipboard.js', () => ({ copyText: vi.fn() }))
const messages = vi.hoisted(() => ({ success: vi.fn(), warning: vi.fn(), error: vi.fn() }))
vi.mock('naive-ui', async original => ({ ...await original(), useMessage: () => messages }))
let wrapper
const row = { key: 'fixture-key', comment: 'Before', disabled: false }
const state = () => ({ accounts: [], quota: {}, api_keys: [{ ...row }] })
function button(text) {
  const element = [...document.querySelectorAll('button')].find(e => e.textContent.trim() === text)
  expect(element, `button ${text}`).toBeTruthy()
  return new DOMWrapper(element)
}
function input(placeholder) { return new DOMWrapper(document.querySelector(`input[placeholder="${placeholder}"]`)) }
function deferred() { let resolve; const promise = new Promise(r => { resolve = r }); return { promise, resolve } }
beforeEach(() => {
  vi.clearAllMocks()
  Object.assign(store, { st: state(), liveQuota: {}, quotaErr: {}, quotaOpen: {}, quotaLoading: {}, status: {} })
  api.mockReset().mockImplementation(async (method, path) => method === 'GET' ? state() : path === '/login/start' ? { auth_url: 'http://auth.example.test/oauth/authorize?state=fixture-state' } : { ok: true })
  copyText.mockResolvedValue(true)
})
afterEach(() => { wrapper?.unmount(); document.body.innerHTML = ''; vi.restoreAllMocks() })

describe('Key management capability', () => {
  it('edits and clears comments without native dialogs or replacing credentials', async () => {
    const prompt = vi.spyOn(window, 'prompt').mockImplementation(() => { throw new Error('native dialog unavailable') })
    wrapper = mount(KeyTable, { attachTo: document.body })
    await button('编辑注释').trigger('click'); await flushPromises()
    await input('注释（留空可清除）').setValue('After')
    await button('保存注释').trigger('click'); await flushPromises()
    expect(api).toHaveBeenCalledWith('PATCH', '/keys/fixture-key', { comment: 'After' })
    expect(prompt).not.toHaveBeenCalled()
    await button('编辑注释').trigger('click'); await flushPromises()
    await input('注释（留空可清除）').setValue('')
    await button('保存注释').trigger('click'); await flushPromises()
    expect(api).toHaveBeenCalledWith('PATCH', '/keys/fixture-key', { comment: '' })
  })
  it('preserves the next draft and guards duplicate create events while saving', async () => {
    const response = deferred()
    api.mockImplementation((method, path) => method === 'POST' && path === '/keys' ? response.promise : Promise.resolve(state()))
    wrapper = mount(KeyTable, { attachTo: document.body })
    await input('sk-...').setValue('fixture-first'); await input('注释').setValue('first')
    await button('添加').trigger('click')
    await input('sk-...').setValue('fixture-next'); await input('注释').setValue('next draft')
    expect(button('添加').element.disabled).toBe(true)
    response.resolve({ ok: true }); await flushPromises()
    expect(input('sk-...').element.value).toBe('fixture-next')
    expect(input('注释').element.value).toBe('next draft')
    expect(api.mock.calls.filter(([method, path]) => method === 'POST' && path === '/keys')).toHaveLength(1)
  })
  it('reports persisted operations separately when their refresh fails', async () => {
    api.mockImplementation((method) => method === 'GET' ? Promise.reject(new Error('fixture refresh failed')) : Promise.resolve({ ok: true }))
    wrapper = mount(KeyTable, { attachTo: document.body })
    await button('禁用').trigger('click'); await flushPromises()
    expect(api).toHaveBeenCalledWith('PATCH', '/keys/fixture-key', { disabled: true })
    expect(messages.warning).toHaveBeenCalledWith(expect.stringContaining('列表刷新失败'))
    expect(wrapper.text()).toContain('Key 已禁用')
    expect(messages.error).not.toHaveBeenCalled()
  })
  it('copies all provider forms and exposes manual selection if copying is denied', async () => {
    wrapper = mount(KeyTable, { attachTo: document.body })
    for (const [label, suffix] of [['普通', ''], ['Fast', '-fast'], ['Ultrafast', '-ultrafast']]) {
      await button(`复制 ${label}`).trigger('click'); await flushPromises()
      expect(copyText).toHaveBeenLastCalledWith('fixture-key' + suffix)
    }
    copyText.mockResolvedValue(false)
    await button('复制 普通').trigger('click'); await flushPromises()
    expect(document.body.textContent).toContain('手动复制凭据')
    expect(document.querySelector('input[aria-label="待复制凭据"]').value).toBe('fixture-key')
  })
})

describe('OAuth workflow capability', () => {
  it('requires the current callback and does not resubmit a consumed authorization', async () => {
    wrapper = mount(AddAccount, { attachTo: document.body })
    await button('生成授权链接').trigger('click'); await flushPromises()
    const callback = new DOMWrapper(document.querySelector('textarea'))
    await callback.setValue('http://localhost:1455/auth/callback?code=test&state=wrong-state')
    await button('完成登录').trigger('click'); await flushPromises()
    expect(api.mock.calls.filter(([, path]) => path === '/login/finish')).toHaveLength(0)
    await callback.setValue('http://localhost:1455/auth/callback?code=test&state=fixture-state')
    api.mockRejectedValueOnce(new Error('authorization exchange failed'))
    await button('完成登录').trigger('click'); await flushPromises()
    expect(wrapper.text()).toContain('不能直接重复提交')
    expect(button('完成登录').element.disabled).toBe(true)
    expect(api.mock.calls.filter(([, path]) => path === '/login/finish')).toHaveLength(1)
  })
  it('binds updates to the selected record and explains the retained disabled state', async () => {
    store.st.accounts = [{ id: 'local-a', email: 'a@example.test', workspace_id: 'workspace-a', disabled: true }]
    wrapper = mount(AddAccount, { props: { expectedId: 'local-a' }, attachTo: document.body })
    await button('生成授权链接').trigger('click'); await flushPromises()
    await new DOMWrapper(document.querySelector('textarea')).setValue('http://localhost:1455/auth/callback?code=test&state=fixture-state')
    api.mockResolvedValueOnce({ id: 'local-a', email: 'a@example.test', plan_type: 'pro', updated: true, disabled: true })
    await button('完成登录').trigger('click'); await flushPromises()
    expect(api).toHaveBeenCalledWith('POST', '/login/finish', { callback_url: expect.any(String), expected_id: 'local-a' })
    expect(wrapper.text()).toContain('账号保持禁用')
  })
})
