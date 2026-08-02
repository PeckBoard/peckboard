import { create } from 'zustand'
import { authedFetch } from './auth'

export interface WorkflowStepInfo {
  step: string
  instructions: string
}

export interface WorkflowInfo {
  id: string
  name: string
  description: string
  priority: number
  steps: (string | WorkflowStepInfo)[]
}
export interface ModelInfo {
  id: string
  display_name: string
  /** Capability tier within the provider (higher = more capable). Single-
   *  tier providers report 0. Only comparable within one provider+account. */
  tier?: number
  /** Capability tags as served by `/api/models` (e.g. `code`, `reasoning`,
   *  `vision`). Informational — gating reads the derived flags below. */
  capabilities?: string[]
  /** Whether this model supports extended reasoning ("thinking"), derived
   *  server-side from capability tags. */
  thinking?: boolean
  /** Whether this model accepts image input. `null`/absent = unknown —
   *  fall back to the provider-level `supports_images_in`. */
  images_in?: boolean | null
}

/** One selectable reasoning-effort level, as served per-provider by
 *  `/api/models`. `id` is passed to the provider's `--effort` flag; `label`
 *  is shown in the effort picker. */
export interface EffortLevel {
  id: string
  label: string
}

export type InterruptKind = 'soft' | 'cooperative' | 'hard_kill'
export type AnswerTransport = 'stdin' | 'new_turn'

/** What a provider actually supports, served per provider by `/api/models`
 *  so the chat can gate affordances instead of rendering every provider as
 *  if it were Claude. */
export interface ProviderCapabilities {
  supports_thinking: boolean
  supports_images_in: boolean
  supports_usage: boolean
  supports_resume: boolean
  interrupt_kind: InterruptKind
  supports_mid_stream_injection: boolean
  answer_transport: AnswerTransport
}

/** Fallback when a provider entry carries no capabilities (old backend,
 *  mocked payloads): today's Claude-shaped assumptions, so behavior is
 *  unchanged for payloads that predate the flags. */
export const DEFAULT_PROVIDER_CAPABILITIES: ProviderCapabilities = {
  supports_thinking: true,
  supports_images_in: true,
  supports_usage: true,
  supports_resume: true,
  interrupt_kind: 'soft',
  supports_mid_stream_injection: true,
  answer_transport: 'stdin',
}

export interface ProviderInfo {
  id: string
  display_name: string
  models: ModelInfo[]
  /** Effort levels this provider exposes. Empty ⇒ the provider has no
   *  effort control, so only the "Default" option is offered. */
  effort_levels?: EffortLevel[]
  capabilities?: ProviderCapabilities
  /** Best-effort auth hint from `/api/models`: `false` = no account and no
   *  host-level credential detected (the picker warns, selection still
   *  allowed); `true` = something usable found; `null`/absent = unknown or
   *  not applicable (local providers, plugins) — no hint shown. */
  configured?: boolean | null
}

/** The always-present "Default" effort option (no override — the provider
 *  decides). Value is `''` so it round-trips as "no effort" everywhere. */
export const DEFAULT_EFFORT_OPTION = { value: '', label: 'Default' }

/**
 * Effort dropdown options for a given model id. Derives the provider from the
 * `provider:model` prefix (bare ids default to `claude`, matching the backend),
 * then returns "Default" followed by that provider's effort levels. This is how
 * the effort picker "loads effort levels from the provider" once a model is
 * chosen — Claude/Grok expose the full ladder, Cursor/Ollama/Mock only Default.
 */
export function effortOptionsForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): { value: string; label: string }[] {
  const providerId = modelId && modelId.includes(':') ? modelId.split(':')[0] : 'claude'
  const provider = providers.find((p) => p.id === providerId)
  const levels = provider?.effort_levels ?? []
  return [DEFAULT_EFFORT_OPTION, ...levels.map((l) => ({ value: l.id, label: l.label }))]
}

/** The provider entry a (possibly bare) model id belongs to. Bare ids
 *  default to `claude`, matching the backend's `parse_model_id`. */
export function providerForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): ProviderInfo | undefined {
  const providerId = modelId && modelId.includes(':') ? modelId.split(':')[0] : 'claude'
  return providers.find((p) => p.id === providerId)
}

/** Capabilities for the provider a model id belongs to, falling back to
 *  the Claude-shaped defaults when the provider (or its capabilities
 *  entry) is unknown. */
export function capabilitiesForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): ProviderCapabilities {
  return providerForModel(modelId, providers)?.capabilities ?? DEFAULT_PROVIDER_CAPABILITIES
}

/** Whether image attachments would actually reach the model: the
 *  provider-level flag gates first, then a per-model `images_in: false`
 *  (e.g. an Ollama model whose probe reported no vision). Unknown models
 *  keep the provider-level answer. */
export function imagesAllowedForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): boolean {
  const provider = providerForModel(modelId, providers)
  const caps = provider?.capabilities ?? DEFAULT_PROVIDER_CAPABILITIES
  if (!caps.supports_images_in) return false
  const model = modelId ? provider?.models.find((m) => m.id === modelId) : undefined
  return model?.images_in !== false
}

/** Label + tooltip for the interrupt affordance, matched to how the
 *  provider actually stops a run — only a soft interrupt may promise a
 *  clean in-band stop. */
export function interruptAffordanceForModel(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): { label: string; title: string } {
  switch (capabilitiesForModel(modelId, providers).interrupt_kind) {
    case 'soft':
      return { label: 'Interrupt', title: 'Interrupt the agent' }
    case 'cooperative':
      return { label: 'Stop', title: 'Ask the agent to stop — it halts at the next safe point' }
    case 'hard_kill':
      return { label: 'Stop', title: 'Stop the agent — the in-flight run is killed' }
  }
}

/** Whether the working indicator should say "Thinking…": the provider
 *  must support thinking and the selected model must not be known to lack
 *  it. Falls back to today's "Thinking…" when nothing is known. */
export function modelThinks(
  modelId: string | null | undefined,
  providers: ProviderInfo[],
): boolean {
  const provider = providerForModel(modelId, providers)
  const caps = provider?.capabilities ?? DEFAULT_PROVIDER_CAPABILITIES
  if (!caps.supports_thinking) return false
  const model = modelId ? provider?.models.find((m) => m.id === modelId) : undefined
  return model?.thinking !== false
}
export interface SystemPromptInfo {
  id: string
  name: string
  body: string
  source_url: string | null
  created_at: string
  updated_at: string
}

interface ResourcesState {
  workflows: WorkflowInfo[]
  models: ModelInfo[]
  providers: ProviderInfo[]
  systemPrompts: SystemPromptInfo[]
  /** App-wide default model (Settings → Default Model). `''` = unset. */
  defaultModel: string
  /** True once `/api/settings/default-model` has answered (even with `''`). */
  defaultModelLoaded: boolean
  fetchWorkflows: () => Promise<void>
  fetchModels: () => Promise<void>
  fetchSystemPrompts: () => Promise<void>
  fetchDefaultModel: () => Promise<void>
  /** Reflect a Settings-page save without refetching. */
  setDefaultModelLocal: (model: string) => void
}

export const useResourcesStore = create<ResourcesState>((set) => ({
  workflows: [],
  models: [],
  providers: [],
  systemPrompts: [],
  defaultModel: '',
  defaultModelLoaded: false,

  fetchWorkflows: async () => {
    try {
      const res = await authedFetch('/api/workflows')
      if (!res.ok) return
      const data = await res.json()
      if (data?.workflows) set({ workflows: data.workflows })
    } catch {
      /* ignore fetch errors — caller renders empty list */
    }
  },

  fetchModels: async () => {
    try {
      const res = await authedFetch('/api/models')
      if (!res.ok) return
      const data = await res.json()
      const patch: Partial<ResourcesState> = {}
      if (data?.models) patch.models = data.models
      if (data?.providers) patch.providers = data.providers
      if (Object.keys(patch).length > 0) set(patch)
    } catch {
      /* ignore */
    }
  },

  fetchSystemPrompts: async () => {
    try {
      const res = await authedFetch('/api/system-prompts')
      if (!res.ok) return
      const data = await res.json()
      if (data?.prompts) set({ systemPrompts: data.prompts })
    } catch {
      /* ignore */
    }
  },

  fetchDefaultModel: async () => {
    try {
      const res = await authedFetch('/api/settings/default-model')
      if (!res.ok) return
      const data = await res.json()
      set({
        defaultModel: typeof data?.model === 'string' ? data.model : '',
        defaultModelLoaded: true,
      })
    } catch {
      /* ignore */
    }
  },

  setDefaultModelLocal: (model: string) => set({ defaultModel: model, defaultModelLoaded: true }),
}))
