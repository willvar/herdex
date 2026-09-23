import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { nextTick } from 'vue'
import Chart from 'chart.js/auto'
import Usage from '../components/Usage.vue'
import { store } from '../store.js'
import { api } from '../api.js'

vi.mock('../api.js', async importOriginal => ({
  ...await importOriginal(), api: vi.fn(), getKey: () => 'test-manage-key',
}))

vi.mock('chart.js/auto', () => ({
  default: vi.fn(function (_canvas, config) {
    this.config = config
    this.destroy = vi.fn()
    this.update = vi.fn()
  }),
}))

vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  return {
    NCard: { setup: (_props, { slots }) => () => h('div', slots.default?.()) },
    NButton: { setup: (_props, { slots }) => () => h('button', slots.default?.()) },
    NDataTable: { render: () => null },
  }
})

enableAutoUnmount(afterEach)

const options = { global: { stubs: { 'n-button-group': { template: '<div><slot/></div>' } } } }
const forecastCard = wrapper => wrapper.findAll('.card').find(card => card.text().includes('池子预计触顶'))
const clickRange = async (wrapper, days) => {
  await wrapper.findAll('button').find(button => button.text() === `${days}天`).trigger('click')
  await flushPromises()
}
const usageDay = (input, output = 0) => ({
  daily: [{ label: '2026-09-23', requests: 1, input, cached: 0, output }],
})

describe('Usage', () => {
  beforeEach(() => {
    Chart.mockClear()
    api.mockReset()
    Object.assign(store, { st: null, usage7: null, usage30: null, history: null, calibration: null })
  })

  it('destroys a chart when unmounted and creates a fresh instance on remount', async () => {
    const first = mount(Usage, options)
    await flushPromises()
    const oldChart = Chart.mock.instances[0]
    expect(oldChart.destroy).not.toHaveBeenCalled()
    first.unmount()
    expect(oldChart.destroy).toHaveBeenCalledOnce()

    store.usage7 = { daily: [] }
    await nextTick()
    expect(oldChart.update).not.toHaveBeenCalled()

    const second = mount(Usage, options)
    await flushPromises()
    expect(Chart).toHaveBeenCalledTimes(2)
    second.unmount()
    expect(Chart.mock.instances[1].destroy).toHaveBeenCalledOnce()
    expect(oldChart.destroy).toHaveBeenCalledOnce()
  })

  it('uses all seven days when only one day has 700 tokens and 700 remain', async () => {
    store.usage7 = usageDay(700)
    store.calibration = { accounts: [], pool_remaining_tokens: 700 }
    const wrapper = mount(Usage, options)
    await flushPromises()

    expect(forecastCard(wrapper).text()).toContain('7 天后')
    expect(forecastCard(wrapper).text()).toContain('日均 100')
  })

  it('uses all thirty days and updates the forecast when switching the visible range', async () => {
    const usage = usageDay(600, 100)
    store.usage7 = usage
    store.calibration = { accounts: [], pool_remaining_tokens: 700 }
    api.mockResolvedValue(usage)
    const wrapper = mount(Usage, options)
    await flushPromises()

    expect(forecastCard(wrapper).text()).toContain('7 天后')
    await clickRange(wrapper, 30)
    expect(api).toHaveBeenCalledWith('GET', '/usage?days=30')
    expect(forecastCard(wrapper).text()).toContain('30 天后')
    expect(forecastCard(wrapper).text()).toContain('日均 23.333')

    await clickRange(wrapper, 7)
    expect(forecastCard(wrapper).text()).toContain('7 天后')
    expect(forecastCard(wrapper).text()).toContain('日均 100')
  })

  it.each([{ daily: [] }, usageDay(0)])('does not project a depletion date for zero consumption: %j', async ({ daily }) => {
    store.usage7 = { daily }
    store.calibration = { accounts: [], pool_remaining_tokens: 700 }
    api.mockResolvedValue({ daily })
    const wrapper = mount(Usage, options)
    await flushPromises()

    expect(forecastCard(wrapper)).toBeUndefined()
    await clickRange(wrapper, 30)
    expect(forecastCard(wrapper)).toBeUndefined()
    expect(wrapper.text()).not.toMatch(/Infinity|NaN/)
  })
})
