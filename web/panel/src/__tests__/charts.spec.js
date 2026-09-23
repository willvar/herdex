import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { capacitySeries } from '../charts.js'

const timestamp = value => Date.parse(value) / 1000
const probe = (email, at, used_pct, account_id = email) => ({ account_id, email, ts: timestamp(at), used_pct })
const weights = [{ account_id: 'a@example.com', email: 'a@example.com', tokens_per_pct: 100 }]

function expectAligned(chart) {
  for (const dataset of chart.datasets) {
    expect(dataset.data).toHaveLength(chart.labels.length)
  }
}

describe('capacitySeries time boundaries', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-09-23T12:30:00Z'))
  })
  afterEach(() => vi.useRealTimers())

  it('includes the current partial hour in account and pool series without rewriting hourly history', () => {
    const chart = capacitySeries([
      probe('a@example.com', '2026-09-23T10:00:00Z', 10),
      probe('a@example.com', '2026-09-23T12:15:00Z', 90),
    ], weights, 1)

    expectAligned(chart)
    expect(chart.labels).toHaveLength(26)
    expect(chart.labels.at(-1)).toMatch(/:30$/)
    expect(chart.datasets[0].data.slice(-4)).toEqual([10, 10, 10, 90])
    expect(chart.datasets[1].data.slice(-4)).toEqual([10, 10, 10, 90])
  })

  it('handles probes ahead of the browser clock and includes them in the weighted endpoint', () => {
    const chart = capacitySeries([
      probe('a@example.com', '2026-09-23T10:00:00Z', 10),
      probe('b@example.com', '2026-09-23T10:00:00Z', 30),
      probe('a@example.com', '2026-09-23T12:32:00Z', 90),
    ], [...weights, { account_id: 'b@example.com', email: 'b@example.com', tokens_per_pct: 300 }], 1)

    expectAligned(chart)
    expect(chart.labels.at(-1)).toMatch(/:32$/)
    expect(chart.datasets[0].data.slice(-2)).toEqual([10, 90])
    expect(chart.datasets.at(-1).data.slice(-2)).toEqual([25, 45])
  })

  it('does not duplicate an endpoint that falls exactly on the hour', () => {
    vi.setSystemTime(new Date('2026-09-23T12:00:00Z'))
    const chart = capacitySeries([
      probe('a@example.com', '2026-09-23T10:00:00Z', 10),
      probe('a@example.com', '2026-09-23T12:00:00Z', 90),
    ], weights, 1)

    expectAligned(chart)
    expect(chart.labels).toHaveLength(25)
    expect(chart.datasets[0].data.slice(-2)).toEqual([10, 90])
    expect(chart.datasets.at(-1).data.at(-1)).toBe(90)
  })

  it('returns an aligned unknown pool when no probes exist', () => {
    const chart = capacitySeries([], weights, 1)

    expectAligned(chart)
    expect(chart.datasets).toHaveLength(1)
    expect(chart.datasets[0].data.every(value => value === null)).toBe(true)
  })

  it('uses pre-window anchors in the pool average until each account changes', () => {
    vi.setSystemTime(new Date('2026-09-23T12:00:00Z'))
    const chart = capacitySeries([
      probe('a@example.com', '2026-08-24T11:00:00Z', 90),
      probe('b@example.com', '2026-08-24T11:00:00Z', 10),
      probe('b@example.com', '2026-08-24T13:00:00Z', 20),
      probe('a@example.com', '2026-08-24T15:00:00Z', 95),
    ], [...weights, { account_id: 'b@example.com', email: 'b@example.com', tokens_per_pct: 100 }], 30)

    expectAligned(chart)
    expect(chart.datasets[0].data.slice(0, 4)).toEqual([90, 90, 90, 95])
    expect(chart.datasets.at(-1).data.slice(0, 4)).toEqual([50, 55, 55, 57.5])
  })

  it('retains an unchanged account represented only by its pre-window anchor', () => {
    const chart = capacitySeries([
      probe('a@example.com', '2026-08-20T12:00:00Z', 70),
    ], weights, 30)

    expectAligned(chart)
    expect(chart.datasets).toHaveLength(2)
    for (const dataset of chart.datasets) {
      expect(dataset.data.every(value => value === 70)).toBe(true)
    }
  })

  it('keeps genuinely unknown history null instead of backfilling a later observation', () => {
    vi.setSystemTime(new Date('2026-09-23T12:00:00Z'))
    const chart = capacitySeries([
      probe('a@example.com', '2026-09-22T14:00:00Z', 70),
    ], weights, 1)

    expectAligned(chart)
    for (const dataset of chart.datasets) {
      expect(dataset.data.slice(0, 3)).toEqual([null, null, 70])
    }
  })

  it('uses the observed post-reset percentage without inventing a zero sample', () => {
    vi.setSystemTime(new Date('2026-09-23T12:00:00Z'))
    const chart = capacitySeries([
      { ...probe('a@example.com', '2026-09-22T11:00:00Z', 98), reset_at: timestamp('2026-09-22T13:00:00Z') },
      { ...probe('a@example.com', '2026-09-22T13:00:00Z', 3), reset_at: timestamp('2026-09-29T13:00:00Z') },
    ], weights, 1)

    expectAligned(chart)
    for (const dataset of chart.datasets) {
      expect(dataset.data.slice(0, 3)).toEqual([98, 3, 3])
    }
  })

  it('uses the latest preceding observation when switching from month to week', () => {
    vi.setSystemTime(new Date('2026-09-23T12:00:00Z'))
    const chart = capacitySeries([
      probe('a@example.com', '2026-08-24T11:00:00Z', 10),
      probe('a@example.com', '2026-09-10T12:00:00Z', 30),
      probe('a@example.com', '2026-09-15T12:00:00Z', 50),
      probe('a@example.com', '2026-09-20T12:00:00Z', 70),
    ], weights, 7)

    expectAligned(chart)
    expect(chart.datasets[0].data[0]).toBe(50)
    expect(chart.datasets.at(-1).data[0]).toBe(50)
    expect(chart.datasets.at(-1).data.at(-1)).toBe(70)
  })

  it.each([
    { secondWeight: 100, expectedPool: 50, reversed: false },
    { secondWeight: 100, expectedPool: 50, reversed: true },
    { secondWeight: 300, expectedPool: 70, reversed: false },
    { secondWeight: 300, expectedPool: 70, reversed: true },
  ])('keeps same-email accounts distinct with weight $secondWeight and reversed=$reversed', ({ secondWeight, expectedPool, reversed }) => {
    const probes = [
      probe('same@example.com', '2026-09-23T10:00:00Z', 10, 'account-a'),
      probe('same@example.com', '2026-09-23T10:00:00Z', 90, 'account-b'),
    ]
    const accounts = [
      { account_id: 'account-a', email: 'same@example.com', tokens_per_pct: 100 },
      { account_id: 'account-b', email: 'same@example.com', tokens_per_pct: secondWeight },
    ]
    if (reversed) { probes.reverse(); accounts.reverse() }
    const chart = capacitySeries(probes, accounts, 7)

    expectAligned(chart)
    expect(chart.datasets).toHaveLength(3)
    const a = chart.datasets.find(dataset => dataset.account_id === 'account-a')
    const b = chart.datasets.find(dataset => dataset.account_id === 'account-b')
    expect(a.data.at(-1)).toBe(10)
    expect(b.data.at(-1)).toBe(90)
    expect(a.label).toContain('same@example.com')
    expect(b.label).toContain('same@example.com')
    expect(a.label).not.toBe(b.label)
    expect(chart.datasets.at(-1).data.at(-1)).toBe(expectedPool)
  })
})
