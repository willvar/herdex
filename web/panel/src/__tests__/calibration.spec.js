import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import Calibration from '../components/Calibration.vue'
import { store } from '../store.js'

vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  return {
    NCard: { setup: (_props, { slots }) => () => h('div', slots.default?.()) },
    NDataTable: {
      props: ['data', 'columns', 'rowKey'],
      setup: props => () => h('div', props.data.map(row => h('div', {
        key: props.rowKey(row), 'data-account-id': props.rowKey(row),
      }, props.columns.map(column => column.render ? column.render(row) : row[column.key])))),
    },
  }
})

enableAutoUnmount(afterEach)

describe('Calibration account identity', () => {
  beforeEach(() => {
    store.calibration = {
      accounts: [
        { account_id: 'account-a', email: 'same@example.com', plan_type: 'pro', samples: 1 },
        { account_id: 'account-b', email: 'same@example.com', plan_type: 'pro', samples: 1 },
      ],
    }
    store.history = {
      probes: [
        { account_id: 'account-a', email: 'same@example.com', used_pct: 10 },
        { account_id: 'account-b', email: 'same@example.com', used_pct: 90 },
        { account_id: 'account-a', email: 'same@example.com', used_pct: 30 },
        { account_id: 'account-b', email: 'same@example.com', used_pct: 80 },
      ],
    }
  })

  it('renders distinct sparklines for two accounts sharing an email, even after rows reorder', async () => {
    const wrapper = mount(Calibration)
    const points = id => wrapper.get(`[data-account-id="${id}"] polyline`).attributes('points')
    expect(points('account-a')).toBe('0.0,19.8 90.0,15.4')
    expect(points('account-b')).toBe('0.0,2.2 90.0,4.4')

    store.calibration.accounts.reverse()
    await nextTick()
    expect(points('account-a')).toBe('0.0,19.8 90.0,15.4')
    expect(points('account-b')).toBe('0.0,2.2 90.0,4.4')
  })
})
