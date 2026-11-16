import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { api } from '../api.js'
import { store } from '../store.js'
import Logs from '../components/Logs.vue'
vi.mock('../api.js', async original => ({ ...await original(), api: vi.fn(), getKey: () => 'fixture-manage' }))
vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  const slot = { setup: (_props, { slots }) => () => h('div', slots.default?.()) }
  return {
    NCard: slot, NAlert: slot, NModal: { render: () => null }, NInput: { render: () => null }, NSelect: { render: () => null }, NPagination: { render: () => null },
    NButton: { emits: ['click'], setup: (_props, { slots, emit }) => () => h('button', { onClick: () => emit('click') }, slots.default?.()) },
    NCheckbox: { props: ['checked'], emits: ['update:checked'], setup: (props, { emit }) => () => h('input', { type: 'checkbox', checked: props.checked, onChange: event => emit('update:checked', event.target.checked) }) },
    NDataTable: {
      props: ['data', 'columns'],
      setup: props => () => h('div', props.data.map(row => h('div', props.columns.map(column =>
        column.type === 'expand' ? column.renderExpand(row) : h('div', { 'data-testid': column.key === 'service_tier' ? 'tier' : column.key }, column.render ? column.render(row) : row[column.key]))))),
    },
  }
})
enableAutoUnmount(afterEach)
let rows
beforeEach(() => {
  store.st = { accounts: [], api_keys: [] }; store.status = {}
  rows = []
  api.mockReset().mockImplementation(async (_method, path) => {
    const filtered = path.includes('errors_only=true') ? rows.filter(row => row.status === 0 || row.status >= 400 || row.error) : rows
    return { items: filtered, total: filtered.length }
  })
})
const mountLogs = async () => { const wrapper = mount(Logs); await flushPromises(); return wrapper }
describe('Request diagnostics', () => {
  it('highlights stream and connection failures and requests server-side error filtering', async () => {
    rows = [{ status: 200, error: '' }, { status: 200, error: 'stream_error' }, { status: 0, error: 'connection_error' }, { status: 503, error: 'http_503' }].map((row, i) => ({ ...row, seq: i + 1 }))
    const wrapper = await mountLogs()
    expect(wrapper.findAll('[data-testid="status"]').map(cell => cell.text())).toEqual(['200', '200 · 异常', '未收到 HTTP', '503'])
    await wrapper.get('input[type="checkbox"]').setValue(true)
    await wrapper.findAll('button').find(button => button.text() === '查询').trigger('click'); await flushPromises()
    expect(api.mock.calls.at(-1)[1]).toContain('errors_only=true')
    expect(wrapper.findAll('[data-testid="status"] .err')).toHaveLength(3)
  })
  it('exposes full diagnostic text safely and keeps missing diagnostics unknown', async () => {
    const diagnostics = { request_id: 'request-test-123', attempt: 2, stage: 'stream', error_detail: '<img src=x onerror=alert(1)> connection reset' }
    rows = [{ seq: 1, diagnostics }, { seq: 2, diagnostics: null }]
    const wrapper = await mountLogs()
    expect(wrapper.get('summary').text()).toBe('第 2 次 · stream')
    expect(wrapper.get('pre').text()).toBe(JSON.stringify(diagnostics, null, 2))
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.findAll('[data-testid="diagnostics"]')[1].text()).toBe('未记录')
    expect(wrapper.text()).toContain('完成状态未知')
  })
  it('groups observed attempts without claiming an unobserved final result', async () => {
    rows = [1, 2].map((attempt, seq) => ({ seq, status: 200, diagnostics: { request_id: 'shared-request', attempt, completed: false } }))
    const wrapper = await mountLogs()
    expect(wrapper.text()).toContain('2 次')
    expect(wrapper.text()).toContain('完成状态未知')
    expect(wrapper.text()).toContain('可能跨页')
  })
})
describe('Request tier display', () => {
  it('distinguishes Fast, omitted and unrecorded tiers', async () => {
    rows = ['priority', 'fast', 'ultrafast', 'default', '', null, undefined, 'flex'].map((service_tier, seq) => ({ seq, service_tier }))
    const wrapper = await mountLogs()
    expect(wrapper.findAll('[data-testid="tier"]').map(cell => cell.text())).toEqual(['Fast', 'Fast', 'Ultrafast', 'default', '未指定', '未记录', '未记录', 'flex'])
  })
  it('renders tier metadata as text rather than HTML', async () => {
    rows = [{ seq: 1, service_tier: '<img src=x onerror=alert(1)>' }]
    const wrapper = await mountLogs()
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.get('[data-testid="tier"]').text()).toBe('<img src=x onerror=alert(1)>')
  })
})
