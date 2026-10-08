import { afterEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, mount } from '@vue/test-utils'
import { store } from '../store.js'
import Logs from '../components/Logs.vue'

vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  return {
    NCard: { setup: (_props, { slots }) => () => h('div', [slots['header-extra']?.(), slots.default?.()]) },
    NSelect: { render: () => null },
    NCheckbox: {
      props: ['checked'],
      emits: ['update:checked'],
      setup: (props, { emit }) => () => h('input', { type: 'checkbox', checked: props.checked, onChange: event => emit('update:checked', event.target.checked) }),
    },
    NDataTable: {
      props: ['data', 'columns', 'rowClassName'],
      setup: props => () => h('div', props.data.map(row => h('div', {
        'data-testid': 'row', class: props.rowClassName(row),
      }, props.columns.map(column => h('div', {
        'data-testid': column.key === 'service_tier' ? 'tier' : column.key,
      }, column.render ? column.render(row) : row[column.key]))))),
    },
  }
})

describe('Request diagnostics', () => {
  it('highlights stream and connection failures and includes them in error-only filtering', async () => {
    store.st = { logs: [
      { status: 200, error: '' },
      { status: 200, error: 'upstream_stream_error' },
      { status: 0, error: 'upstream_request_error' },
      { status: 503, error: 'http_503' },
    ] }
    const wrapper = mount(Logs)
    expect(wrapper.findAll('[data-testid="status"]').map(cell => cell.text())).toEqual([
      '200', '200 · 异常', '未收到 HTTP', '503',
    ])
    expect(wrapper.findAll('.log-err')).toHaveLength(3)
    await wrapper.get('input[type="checkbox"]').setValue(true)
    expect(wrapper.findAll('[data-testid="row"]')).toHaveLength(3)
    expect(wrapper.findAll('[data-testid="status"] .err')).toHaveLength(3)
  })

  it('exposes full request IDs and transport details as text, with legacy diagnostics left unknown', () => {
    const diagnostics = { request_id: 'request-test-123', attempt: 2, stage: 'stream', error_detail: '<img src=x onerror=alert(1)> connection reset by peer' }
    store.st = { logs: [{ diagnostics }, { diagnostics: null }] }
    const wrapper = mount(Logs)
    expect(wrapper.get('summary').text()).toBe('第 2 次 · stream')
    expect(wrapper.get('pre').text()).toBe(JSON.stringify(diagnostics, null, 2))
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.findAll('[data-testid="diagnostics"]')[1].text()).toBe('未记录')
  })
})

enableAutoUnmount(afterEach)

describe('Request tier display', () => {
  it('distinguishes Fast requests, omitted tiers, and unrecorded history', () => {
    store.st = {
      logs: ['priority', 'fast', 'default', '', null, undefined, 'flex'].map(service_tier => ({ service_tier })),
    }
    const wrapper = mount(Logs)
    expect(wrapper.findAll('[data-testid="tier"]').map(cell => cell.text())).toEqual([
      'Fast', 'Fast', 'default', '未指定', '未记录', '未记录', 'flex',
    ])
  })

  it('renders tier metadata as text rather than HTML', () => {
    store.st = { logs: [{ service_tier: '<img src=x onerror=alert(1)>' }] }
    const wrapper = mount(Logs)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.get('[data-testid="tier"]').text()).toBe('<img src=x onerror=alert(1)>')
  })
})
