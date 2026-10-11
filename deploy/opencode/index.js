import { Plugin, Provider, Model } from "@opencode/plugin"

const REFRESH_MS = 60_000
const METADATA = ["name", "family", "capabilities", "limit", "cost", "time"]
const isFastAlias = (id) => /^gpt-.+-fast$/i.test(id)

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

    const fetchModels = async (key) => {
      const res = await fetch(`${baseURL}/models`, {
        headers: { Authorization: `Bearer ${key}` },
        signal: AbortSignal.timeout(10_000),
      })
      if (!res.ok) throw new Error(`/models returned HTTP ${res.status}`)
      const body = await res.json()
      if (!Array.isArray(body.data) || body.data.some((m) => typeof m?.id !== "string" || !m.id)) {
        throw new Error("/models returned an invalid model list")
      }
      // Older herdex versions may still advertise generated aliases. OpenCode
      // has separate providers for Fast, so only canonical IDs belong here.
      return body.data.filter((entry) => !isFastAlias(entry.id))
    }

    const readCatalog = async () => {
      // Active OpenAI models may have richer metadata from the account catalog.
      const catalog = new Map()
      try {
        const out = await ctx.model.list()
        const models = Array.isArray(out) ? out : out?.data ?? []
        for (const model of models) {
          if (model.providerID === "openai") catalog.set(model.id, model)
        }
      } catch {
        console.error("herdex-models: catalog unavailable; using source metadata")
      }
      return catalog
    }

    const discover = async () => {
      const [normal, fast] = await Promise.allSettled([
        fetchModels(apiKey),
        fetchModels(`${apiKey}-fast`),
      ])
      if (normal.status === "rejected") throw normal.reason

      if (fast.status === "rejected") {
        console.error("herdex-models: Fast provider discovery failed; keeping it empty")
      }
      return {
        normal: normal.value,
        fast: fast.status === "fulfilled" ? fast.value : [],
        catalog: await readCatalog(),
      }
    }

    let source = { normal: [], fast: [], catalog: new Map() }
    try {
      source = await discover()
    } catch {
      console.error("herdex-models: initial discovery failed; retrying in 60s")
    }

    await ctx.provider.transform((editor) => {
      // Source definitions also include inactive OpenAI providers.
      const defaults = editor.get("openai")?.models
      const definitions = (pid, entries) => entries.map((entry) => {
        const model = Model.Info.default(pid, Model.ID.make(entry.id))
        const catalog = source.catalog.get(entry.id) ?? defaults?.get(entry.id)
        if (catalog) {
          for (const field of METADATA) {
            if (catalog[field] !== undefined) model[field] = structuredClone(catalog[field])
          }
        }
        // Inherit semantic choices, but keep Fast as a provider choice rather
        // than a variant that could accidentally use the ordinary API key.
        const variants = (catalog?.variants ?? []).map((variant) => {
          const settings = {}
          for (const field of ["reasoningEffort", "reasoningSummary", "include", "serviceTier"]) {
            if (variant.settings?.[field] !== undefined) {
              settings[field] = structuredClone(variant.settings[field])
            }
          }
          return { id: variant.id, settings }
        }).filter((variant) => variant.id !== "fast" && !variant.id.endsWith(" fast"))
        model.variants = variants
        model.name = catalog?.name || entry.name || prettyName(entry.id)
        return model
      })

      const addProvider = (id, name, key, entries) => {
        const pid = Provider.ID.make(id)
        editor.add({
          info: {
            ...Provider.Info.empty(pid),
            name,
            activation: "enabled",
            package: "@opencode/ai/providers/openai/responses",
            settings: { baseURL, apiKey: key },
          },
          models: definitions(pid, entries),
        })
      }

      addProvider("herdex", "Herdex Pool", apiKey, source.normal)
      addProvider("herdex-fast", "Herdex Pool (Fast)", `${apiKey}-fast`, source.fast)
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
