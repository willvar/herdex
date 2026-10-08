import { Plugin, Provider, Model } from "@opencode/plugin"

const REFRESH_MS = 60_000
const METADATA = ["name", "family", "capabilities", "limit", "cost", "time"]

// Used only when the catalog and endpoint supply no display name.
const prettyName = (id) => id
  .replace(/^gpt-(\d+(?:\.\d+)*)(?:-|$)/, "GPT-$1 ")
  .replace(/^gpt-/, "GPT ")
  .replace(/[-_](?=[a-z])/g, " ")
  .replace(/\b[a-z]/g, (letter) => letter.toUpperCase())
  .trim()

export default Plugin.define({
  id: "herdex.models",
  async setup(ctx) {
    const baseURL = (process.env.HERDEX_BASE_URL ?? ctx.options.baseURL ?? "http://127.0.0.1:8088/v1")
      .replace(/\/+$/, "")
    const apiKey = process.env.HERDEX_API_KEY ?? ctx.options.apiKey
    if (!apiKey) throw new Error("herdex-models: set options.apiKey or HERDEX_API_KEY")
    const pid = Provider.ID.make("herdex")

    const discover = async () => {
      const res = await fetch(`${baseURL}/models`, {
        headers: { Authorization: `Bearer ${apiKey}` },
        signal: AbortSignal.timeout(10_000),
      })
      if (!res.ok) throw new Error(`/models returned HTTP ${res.status}`)
      const body = await res.json()
      if (!Array.isArray(body.data) || body.data.some((m) => typeof m?.id !== "string" || !m.id)) {
        throw new Error("/models returned an invalid model list")
      }

      // Active OpenAI models may have richer metadata from the account catalog.
      const catalog = new Map()
      try {
        const out = await ctx.model.list()
        for (const model of Array.isArray(out) ? out : out.data) {
          if (model.providerID === "openai") catalog.set(model.id, model)
        }
      } catch {
        console.error("herdex-models: catalog unavailable; using source metadata")
      }
      return { entries: body.data, catalog }
    }

    let source = { entries: [], catalog: new Map() }
    try {
      source = await discover()
    } catch {
      console.error("herdex-models: initial discovery failed; retrying in 60s")
    }

    await ctx.provider.transform((editor) => {
      // Source definitions also include inactive OpenAI providers.
      const defaults = editor.get("openai")?.models
      const models = source.entries.map((entry) => {
        const model = Model.Info.default(pid, Model.ID.make(entry.id))
        const catalog = source.catalog.get(entry.id) ?? defaults?.get(entry.id)
        if (catalog) {
          for (const field of METADATA) {
            if (catalog[field] !== undefined) model[field] = structuredClone(catalog[field])
          }
        }
        // Inherit selected semantic choices, never another provider's routing/auth overrides.
        const variants = (catalog?.variants ?? []).map((variant) => {
          const settings = {}
          for (const field of ["reasoningEffort", "reasoningSummary", "include", "serviceTier"]) {
            if (variant.settings?.[field] !== undefined) {
              settings[field] = structuredClone(variant.settings[field])
            }
          }
          return { id: variant.id, settings }
        })
        if (/^gpt-/i.test(entry.id)) {
          const fast = variants.find((variant) => variant.id === "fast")
          if (fast) {
            fast.settings = { ...fast.settings, serviceTier: "priority" }
          } else {
            variants.push({ id: "fast", settings: { serviceTier: "priority" } })
          }
          for (const variant of [...variants]) {
            if (variant.id === "fast" || variant.id.endsWith("-fast")) continue
            if (variant.settings?.reasoningEffort === undefined) continue
            const id = `${variant.id}-fast`
            if (variants.some((candidate) => candidate.id === id)) continue
            variants.push({
              id,
              settings: { ...variant.settings, serviceTier: "priority" },
            })
          }
        }
        model.variants = variants
        model.name = catalog?.name || entry.name || prettyName(entry.id)
        return model
      })

      editor.add({
        info: {
          ...Provider.Info.empty(pid),
          name: "Herdex Pool",
          activation: "enabled",
          package: "@opencode/ai/providers/openai/responses",
          settings: { baseURL, apiKey },
        },
        models,
      })
    })

    let stopped = false
    const refresh = async () => {
      try {
        const next = await discover()
        if (stopped) return
        source = next
        await ctx.provider.reload()
      } catch {
        console.error("herdex-models: refresh failed; keeping the last model list")
      }
    }
    const timer = setInterval(() => void refresh(), REFRESH_MS)
    return () => {
      stopped = true
      clearInterval(timer)
    }
  },
})
