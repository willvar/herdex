import assert from "node:assert/strict"
import { test } from "node:test"
import { setImmediate as tick } from "node:timers/promises"
import plugin from "./index.js"

async function fixture(t, { failFast = false, failUltrafast = false } = {}) {
  for (const name of ["HERDEX_BASE_URL", "HERDEX_API_KEY"]) {
    const saved = process.env[name]
    delete process.env[name]
    t.after(() => {
      if (saved === undefined) delete process.env[name]
      else process.env[name] = saved
    })
  }
  const state = {
    normal: [{ id: "gpt-6-astra" }, { id: "gpt-6-astra-fast" }],
    fast: [{ id: "gpt-6-astra" }],
    ultrafast: [{ id: "gpt-6-astra" }],
    failFast,
    failUltrafast,
    failNormal: false,
    providers: new Map(),
    requests: [],
    reloads: 0,
    cleared: false,
  }
  t.mock.method(console, "error", () => {})
  t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "http://127.0.0.1:8088/v1/models")
    const key = options.headers.Authorization
    state.requests.push(key)
    const mode = key === "Bearer test-key-ultrafast" ? "ultrafast" :
      key === "Bearer test-key-fast" ? "fast" : "normal"
    assert.equal(key, `Bearer test-key${mode === "normal" ? "" : `-${mode}`}`)
    if (state[`fail${mode[0].toUpperCase()}${mode.slice(1)}`]) {
      return new Response("unavailable", { status: 503 })
    }
    return Response.json({ data: state[mode] })
  })
  let refresh
  t.mock.method(globalThis, "setInterval", (callback, milliseconds) => {
    assert.equal(milliseconds, 60_000)
    refresh = callback
    return "refresh-timer"
  })
  t.mock.method(globalThis, "clearInterval", (timer) => {
    assert.equal(timer, "refresh-timer")
    state.cleared = true
  })
  let transform
  const replay = () => {
    state.providers.clear()
    transform({
      get: () => undefined,
      add: (provider) => state.providers.set(provider.info.id, provider),
    })
  }
  const cleanup = await plugin.setup({
    options: { apiKey: "test-key", baseURL: "http://127.0.0.1:8088/v1/" },
    model: {
      list: async () => [{
        providerID: "openai", id: "gpt-6-astra", name: "GPT-6 Astra",
        settings: { serviceTier: "ultrafast" },
        body: { service_tier: "ultrafast" },
        variants: [
          { id: "high", settings: { reasoningEffort: "high", serviceTier: "default" } },
          { id: "fast", settings: { serviceTier: "priority" } },
          { id: "high fast", settings: { reasoningEffort: "high", serviceTier: "priority" } },
          { id: "ultrafast", settings: { serviceTier: "ultrafast" } },
          { id: "hidden-speed", settings: { serviceTier: "ultrafast" } },
        ],
      }],
    },
    provider: {
      transform: async (callback) => { transform = callback; replay() },
      reload: async () => { state.reloads++; replay() },
    },
  })
  t.after(cleanup)
  state.refresh = async () => { refresh(); await tick() }
  state.cleanup = cleanup
  return state
}

test("registers three speed providers with shared-key selectors and reasoning-only variants", async (t) => {
  const f = await fixture(t)
  assert.deepEqual([...f.providers.keys()], ["herdex", "herdex-fast", "herdex-ultrafast"])
  for (const [id, suffix] of [["herdex", ""], ["herdex-fast", "-fast"], ["herdex-ultrafast", "-ultrafast"]]) {
    const provider = f.providers.get(id)
    assert.equal(provider.info.settings.apiKey, `test-key${suffix}`)
    assert.equal(provider.info.package, "@opencode/ai/providers/openai/responses")
    assert.equal(provider.models.length, 1)
    assert.equal(provider.models[0].id, "gpt-6-astra")
    assert.equal(provider.models[0].settings?.serviceTier, undefined)
    assert.equal(provider.models[0].body?.service_tier, undefined)
    assert.deepEqual(provider.models[0].variants, [{ id: "high", settings: { reasoningEffort: "high" } }])
  }
})

test("failed accelerated discovery stays empty without borrowing the ordinary list", async (t) => {
  const f = await fixture(t, { failFast: true, failUltrafast: true })
  assert.equal(f.providers.get("herdex").models.length, 1)
  assert.equal(f.providers.get("herdex-fast").models.length, 0)
  assert.equal(f.providers.get("herdex-ultrafast").models.length, 0)
})

test("failed or malformed accelerated refreshes keep only their own last valid lists", async (t) => {
  const f = await fixture(t)
  f.failFast = true
  f.ultrafast = { invalid: true }
  await f.refresh()
  assert.equal(f.reloads, 1)
  assert.equal(f.providers.get("herdex-fast").models.length, 1)
  assert.equal(f.providers.get("herdex-ultrafast").models.length, 1)
})

test("a successful empty refresh withdraws capabilities and cleanup stops reloads", async (t) => {
  const f = await fixture(t)
  f.fast = []
  f.ultrafast = []
  await f.refresh()
  assert.equal(f.reloads, 1)
  assert.equal(f.providers.get("herdex").models.length, 1)
  assert.equal(f.providers.get("herdex-fast").models.length, 0)
  assert.equal(f.providers.get("herdex-ultrafast").models.length, 0)
  f.cleanup()
  assert.equal(f.cleared, true)
  await f.refresh()
  assert.equal(f.reloads, 1)
})

test("a failed ordinary refresh keeps the previous snapshot", async (t) => {
  const f = await fixture(t)
  f.failNormal = true
  await f.refresh()
  assert.equal(f.reloads, 0)
  assert.equal(f.providers.get("herdex-ultrafast").models.length, 1)
})
