import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { enableAutoUnmount, flushPromises, mount } from '@vue/test-utils'
import { nextTick } from 'vue'

// mock the api module so the save() PUT never touches the network
vi.mock('../api.js', () => ({ api: vi.fn(), getKey: () => 'test-manage-key' }))
// Keep the real Settings component and its events, without depending on
// Naive UI's internal markup or its message provider.
vi.mock('naive-ui', async () => {
  const { h } = await import('vue')
  return {
    NCard: { setup: (_props, { slots }) => () => h('div', slots.default?.()) },
    NInputNumber: {
      props: ['value'],
      emits: ['update:value'],
      setup: (props, { emit }) => () => h('input', {
        'data-testid': 'number',
        value: props.value,
        onInput: event => emit('update:value', Number(event.target.value)),
      }),
    },
    NSelect: { render: () => h('div') },
    NButton: {
      name: 'NButton',
      props: ['disabled', 'loading'],
      emits: ['click'],
      setup: (props, { slots, emit }) => () => h('button', {
        disabled: props.disabled,
        onClick: () => emit('click'),
      }, slots.default?.()),
    },
    useMessage: () => ({ success: () => {}, error: () => {} }),
  }
})
import { api } from '../api.js'
import { store, loadState } from '../store.js'
import Settings from '../components/Settings.vue'

function deferred() {
  let resolve
  const promise = new Promise(done => { resolve = done })
  return { promise, resolve }
}

enableAutoUnmount(afterEach)

describe('Settings draft protection', () => {
  beforeEach(() => {
    api.mockReset().mockResolvedValue({})
    store.st = { settings: { cooldown_seconds: 300, pin_yield_gap_pp: 20 } }
  })

  it('keeps the saved values in the form and sends them again on a second save', async () => {
    const wrapper = mount(Settings)
    const [cooldown, yieldGap] = wrapper.findAll('[data-testid="number"]')
    expect(cooldown.element.value).toBe('300')
    await cooldown.setValue('60')
    await yieldGap.setValue('5')

    await wrapper.get('button').trigger('click')
    await flushPromises()

    const saved = { cooldown_seconds: 60, pin_yield_gap_pp: 5 }
    expect(store.st.settings).toEqual(saved)
    expect(cooldown.element.value).toBe('60')
    expect(yieldGap.element.value).toBe('5')

    await wrapper.get('button').trigger('click')
    await flushPromises()

    expect(api).toHaveBeenCalledTimes(2)
    expect(api).toHaveBeenNthCalledWith(1, 'PUT', '/settings', saved)
    expect(api).toHaveBeenNthCalledWith(2, 'PUT', '/settings', saved)
  })

  it('preserves both unsaved inputs when the periodic poll updates the store', async () => {
    const wrapper = mount(Settings)
    const [cooldown, yieldGap] = wrapper.findAll('[data-testid="number"]')
    await cooldown.setValue('120')
    await yieldGap.setValue('7')

    api.mockResolvedValueOnce({ settings: { cooldown_seconds: 600, pin_yield_gap_pp: 30 } })
    await loadState()
    await nextTick()

    expect(cooldown.element.value).toBe('120')
    expect(yieldGap.element.value).toBe('7')
    expect(api).toHaveBeenCalledOnce()
    expect(api).toHaveBeenCalledWith('GET', '/state')
  })

  it('still syncs a server update before the user starts editing', async () => {
    const wrapper = mount(Settings)
    store.st = { settings: { cooldown_seconds: 600, pin_yield_gap_pp: 30 } }
    await nextTick()

    const [cooldown, yieldGap] = wrapper.findAll('[data-testid="number"]')
    expect(cooldown.element.value).toBe('600')
    expect(yieldGap.element.value).toBe('30')
  })

  it('keeps an unsaved draft after a failed save and a subsequent poll', async () => {
    api.mockRejectedValueOnce(new Error('server unavailable'))
    const wrapper = mount(Settings)
    const [cooldown] = wrapper.findAll('[data-testid="number"]')
    await cooldown.setValue('60')
    await wrapper.get('button').trigger('click')
    await flushPromises()

    store.st = { settings: { cooldown_seconds: 300, pin_yield_gap_pp: 20 } }
    await nextTick()

    expect(cooldown.element.value).toBe('60')
    expect(store.st.settings.cooldown_seconds).toBe(300)
  })

  it('preserves edits made during a pending save and prevents duplicate submissions', async () => {
    const response = deferred()
    api.mockReturnValueOnce(response.promise)
    const wrapper = mount(Settings)
    const [cooldown, yieldGap] = wrapper.findAll('[data-testid="number"]')
    await cooldown.setValue('60')
    await wrapper.get('button').trigger('click')
    expect(wrapper.get('button').element.disabled).toBe(true)

    // Even a repeated component event cannot send the pending save twice.
    wrapper.getComponent({ name: 'NButton' }).vm.$emit('click')
    expect(api).toHaveBeenCalledTimes(1)
    await cooldown.setValue('120')
    await yieldGap.setValue('7')
    response.resolve({ cooldown_seconds: 60, pin_yield_gap_pp: 20 })
    await flushPromises()

    expect(cooldown.element.value).toBe('120')
    expect(yieldGap.element.value).toBe('7')
    expect(wrapper.get('button').element.disabled).toBe(false)
    expect(store.st.settings).toEqual({ cooldown_seconds: 60, pin_yield_gap_pp: 20 })

    api.mockResolvedValueOnce({ settings: { cooldown_seconds: 60, pin_yield_gap_pp: 20 } })
    await loadState()
    await nextTick()
    expect(cooldown.element.value).toBe('120')

    await wrapper.get('button').trigger('click')
    await flushPromises()
    expect(api).toHaveBeenLastCalledWith('PUT', '/settings', { cooldown_seconds: 120, pin_yield_gap_pp: 7 })
  })

  it.each(['before save', 'during save'])('rejects stale settings from a poll started %s', async (pollTiming) => {
    const pollResponse = deferred()
    const saveResponse = deferred()
    api.mockImplementation(method => method === 'GET' ? pollResponse.promise : saveResponse.promise)
    const wrapper = mount(Settings)
    const [cooldown] = wrapper.findAll('[data-testid="number"]')
    await cooldown.setValue('60')
    let polling
    if (pollTiming === 'before save') polling = loadState()
    await wrapper.get('button').trigger('click')
    if (pollTiming === 'during save') polling = loadState()

    saveResponse.resolve({ cooldown_seconds: 60, pin_yield_gap_pp: 20 })
    await flushPromises()
    pollResponse.resolve({ accounts: [{ id: 'fresh-account' }], settings: { cooldown_seconds: 300, pin_yield_gap_pp: 20 } })
    await polling
    await nextTick()

    expect(store.st.accounts).toEqual([{ id: 'fresh-account' }])
    expect(store.st.settings.cooldown_seconds).toBe(60)
    expect(cooldown.element.value).toBe('60')

    // A genuinely later poll must still reflect external settings changes.
    api.mockResolvedValueOnce({ settings: { cooldown_seconds: 90, pin_yield_gap_pp: 20 } })
    await loadState()
    await nextTick()
    expect(cooldown.element.value).toBe('90')
  })
})
