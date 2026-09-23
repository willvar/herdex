// Chart view-model helpers for the Usage panel — pure functions, unit-testable.

export function dailyLabels(days, daily) {
  const labels = [], map = {}
  for (let i = days - 1; i >= 0; i--) {
    const lb = new Date(Date.now() - i * 864e5).toLocaleDateString('sv')
    labels.push(lb.slice(5))
    map[lb] = (daily || []).find(y => y.label === lb) || {}
  }
  return { labels, map }
}

export const seriesMeta = {
  in: { label: '输入（不含缓存）', color: '#7c6cf0', get: x => Math.max(0, (x.input || 0) - (x.cached || 0)) },
  cached: { label: '缓存', color: '#8f8ca0', get: x => x.cached || 0 },
  out: { label: '输出', color: '#5fd0a0', get: x => x.output || 0 },
}

// Per-model daily burn: {labels, datasets:[{model, data:[totalTokPerDay]}]}
export function modelSeries(modelDaily, days) {
  const labels = []
  for (let i = days - 1; i >= 0; i--) labels.push(new Date(Date.now() - i * 864e5).toLocaleDateString('sv').slice(5))
  const models = [...new Set((modelDaily || []).map(r => r.model))]
  const palette = ['#7c6cf0', '#5fd0a0', '#e0a070', '#70b0e0', '#e07070', '#b0b0c0']
  const byKey = {}
  for (const r of modelDaily || []) byKey[r.day + '|' + r.model] = r
  const datasets = models.map((m, i) => ({
    label: m,
    data: labels.map(lb => {
      const dayFull = new Date(Date.now() - (days - 1 - labels.indexOf(lb)) * 864e5).toLocaleDateString('sv')
      const r = byKey[dayFull + '|' + m]
      return r ? (r.input + r.output) : 0
    }),
    borderColor: palette[i % palette.length],
    backgroundColor: palette[i % palette.length] + '88',
    tension: 0.25,
    pointRadius: 2,
  }))
  return { labels, datasets }
}

// Capacity chart from probe history: per-account used_pct series (+ weighted
// pool line when calibration weights exist). Observations can be sparse —
// forward-fill each series on an hourly grid so lines are continuous.
// Post-reset samples already contain the new window's observed percentage.
export function capacitySeries(probeHistory, calAccounts, days) {
  const now = Math.floor(Date.now() / 1000)
  const t0 = now - days * 86400
  const weights = Object.fromEntries((calAccounts || []).map(a => [a.account_id, a.tokens_per_pct || 0]))
  // group points per account (sorted by ts already)
  const perAcc = {}
  for (const p of probeHistory) {
    (perAcc[p.account_id] = perAcc[p.account_id] || []).push(p)
  }
  const step = 3600
  // End the grid at the newest sample: gateway probe timestamps can be
  // slightly ahead of the browser clock, and a sample past the grid would
  // otherwise extend the datasets (desyncing labels) instead of rendering
  const newest = (probeHistory || []).reduce((m, p) => Math.max(m, p.ts), now)
  const tEnd = Math.max(now, newest)
  const grid = []
  for (let t = t0 - (t0 % step); t <= tEnd; t += step) grid.push(t)
  if (grid.at(-1) !== tEnd) grid.push(tEnd)
  const datasets = []
  const pool = { data: [] }
  for (const [account_id, pts] of Object.entries(perAcc)) {
    const data = new Array(grid.length).fill(null)
    let gi = 0, used = null
    for (const p of pts) {
      // move grid pointer up to the sample; the first sample after a window
      // reset already reports the post-reset (low) pct, so no manual dip
      while (gi < grid.length && grid[gi] < p.ts) { if (used !== null) data[gi] = used; gi++ }
      used = p.used_pct
      data[gi] = used
    }
    while (gi < grid.length) { data[gi] = used; gi++ }
    datasets.push({ account_id, email: pts.at(-1).email, data, w: weights[account_id] || 0 })
  }
  // pool weighted line = Σ(used%·w)/Σw over accounts that report
  for (const i of grid.keys()) {
    let sw = 0, acc = 0
    for (const d of datasets) {
      const v = d.data[i]
      if (v != null && d.w > 0) { acc += v * d.w; sw += d.w }
    }
    pool.data.push(sw > 0 ? acc / sw : null)
  }
  const palette = ['#7c6cf0', '#5fd0a0', '#e0a070', '#70b0e0', '#e07070', '#b0b0c0']
  const emailCounts = {}
  for (const d of datasets) emailCounts[d.email] = (emailCounts[d.email] || 0) + 1
  const out = {
    labels: grid.map(t => {
      const d = new Date(t * 1000)
      return days <= 7 ? `${(d.getMonth() + 1)}/${d.getDate()} ${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}` : `${d.getMonth() + 1}/${d.getDate()}`
    }),
    datasets: datasets.map((d, i) => ({
      account_id: d.account_id,
      label: emailCounts[d.email] > 1 ? `${d.email} (${d.account_id})` : d.email,
      label2: d.email,
      data: d.data.map(v => v == null ? null : v),
      borderColor: palette[i % palette.length],
      backgroundColor: 'transparent',
      borderWidth: d.w > 0 ? 1.5 : 1,
      borderDash: d.w > 0 ? undefined : [4, 4],
      tension: 0.1,
      pointRadius: 0,
      spanGaps: true,
    })),
  }
  out.datasets.push({
    label: '池加权平均',
    data: pool.data,
    borderColor: '#ffffff',
    backgroundColor: 'transparent',
    borderWidth: 2.5,
    tension: 0.1,
    pointRadius: 0,
    spanGaps: true,
  })
  return out
}

// Burn projection: days until the pool window bottom, given remaining tokens
// (calibration) and daily burn (request_log). Also % slope for chart overlay.
export function forecast(poolRemainingTokens, dailyTokens, poolTokensPerPct, currentPoolPct) {
  if (!dailyTokens || !poolRemainingTokens) return null
  const daysLeft = poolRemainingTokens / dailyTokens
  const daysFull = daysLeft + 10  // pools never hit literally 100%: pad a little
  // % slope: pool-weighted pct per day
  const pctPerDay = poolTokensPerPct ? (dailyTokens / poolTokensPerPct) : null
  return {
    daysLeft: Math.max(0, Math.round(daysLeft * 10) / 10),
    daysFull: Math.round(daysFull * 10) / 10,
    dailyTokens,
    pctPerDay,
    currentPoolPct,
    reachedAt: (() => {
      const d = new Date(Date.now() + daysLeft * 864e5)
      return `${d.getMonth() + 1}/${d.getDate()}`
    })(),
  }
}

export function esc(s) { return (s || '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])) }
