import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import { api } from '../api.js'
import { store, fetchQuota } from '../store.js'
import Accounts from '../components/Accounts.vue'

vi.mock('../api.js', () => ({ api: vi.fn(), getKey: () => 'test-manage-key' }))
vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  return {
    NCard: { setup: (_props, { slots }) => () => h('div', slots.default?.()) },
    NButton: {
      props: ['disabled'], emits: ['click'],
      setup: (props, { slots, emit }) => () => h('button', {
        disabled: props.disabled, onClick: event => emit('click', event),
      }, slots.default?.()),
    },
    NDataTable: {
      props: ['data', 'columns', 'expandedRowKeys'],
      setup: props => () => {
        const expand = props.columns.find(column => column.type === 'expand')
        return h('div', props.data.map(row => expand
          ? props.expandedRowKeys.includes(row.id) && expand.renderExpand(row)
          : h('div', props.columns.map(column => column.render ? column.render(row) : String(row[column.key])))))
      },
    },
    NSwitch: { render: () => null },
    NTag: { render: () => null },
    NPopconfirm: { render: () => null },
    useMessage: () => ({ success: () => {}, error: () => {} }),
  }
})

const account = { id: 'a', email: 'a@example.com' }
const report = used => ({ main: { primary: { used_pct: used, window_seconds: 18000 }, secondary: {} } })

enableAutoUnmount(afterEach)

describe('Account quota states', () => {
  beforeEach(() => {
    api.mockReset()
    Object.assign(store, {
      st: { accounts: [account], quota: {} }, liveQuota: {}, quotaErr: {},
      quotaOpen: { a: true }, quotaLoading: {},
    })
  })

  it('shows loading, then the query failure and a working retry instead of no observations', async () => {
    const wrapper = mount(Accounts)
    expect(wrapper.text()).toContain('尚无观测')
    let fail
    api.mockReturnValueOnce(new Promise((_resolve, reject) => { fail = reject }))
    const pending = fetchQuota(account.id, account.email, true)
    await nextTick()
    expect(wrapper.get('[role="status"]').text()).toBe('查询中…')
    expect(wrapper.text()).not.toContain('尚无观测')

    fail(new Error('upstream timeout'))
    await pending
    await nextTick()
    expect(wrapper.get('[role="alert"]').text()).toContain('upstream timeout')
    expect(wrapper.text()).not.toContain('尚无观测')

    api.mockResolvedValueOnce(report(20))
    await wrapper.get('button').trigger('click')
    await flushPromises()
    expect(api).toHaveBeenLastCalledWith('POST', '/accounts/a/quota')
    expect(wrapper.find('[role="alert"]').exists()).toBe(false)
    expect(wrapper.text()).toContain('80%')
    expect(wrapper.text()).not.toContain('可能已过期')
  })

  it.each(['cached', 'live'])('retains %s quota with a stale-data warning after a query failure', async source => {
    if (source === 'cached') {
      store.st.quota[account.email] = { default: { primary_pct: 40, primary_window_secs: 18000 } }
    } else {
      store.liveQuota[account.email] = report(40)
    }
    const wrapper = mount(Accounts)
    let fail
    api.mockReturnValueOnce(new Promise((_resolve, reject) => { fail = reject }))
    const pending = fetchQuota(account.id, account.email, true)
    await nextTick()
    expect(wrapper.text()).toContain('60%')
    expect(wrapper.text()).toContain('可能已过期')
    expect(wrapper.text()).toContain('查询中…')

    fail(new Error('query rejected'))
    await pending
    await nextTick()
    expect(wrapper.text()).toContain('60%')
    expect(wrapper.text()).toContain('可能已过期')
    expect(wrapper.get('[role="alert"]').text()).toContain('query rejected')
    expect(wrapper.get('button').text()).toBe('重试')
  })
})
