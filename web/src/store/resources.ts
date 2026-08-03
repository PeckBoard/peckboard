import { create } from 'zustand'
import { authedFetch } from './auth'
import type { EffortLevel } from '../lib/effort'

export interface WorkflowStepInfo {
  step: string
  instructions: string
}

export interface WorkflowInfo {
  id: string
  name: string
  description: string
  priority: number
  /** "builtin" (read-only, defined in code) or "custom" (user-defined via
   *  Settings → Workflows / the `/api/workflows` CRUD routes). */
  source: 'builtin' | 'custom'
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

// Effort helpers live in `../lib/effort` (pure module, no store imports) so
// the e2e suite can unit-test them in node; re-exported here so existing
// imports keep working.
export {
  DEFAULT_EFFORT_OPTION,
  effortOptionsForModel,
  effortSelectOptions,
  effortValidForModel,
  sessionModelPatch,
} from '../lib/effort'
export type { EffortLevel, EffortOption } from '../lib/effort'

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

/** Fallback when the model's provider is entirely missing from the catalogue
 *  (hidden in Settings → Providers, or its plugin uninstalled, while a live
 *  session still references it). Conservative here means the UI must not
 *  overclaim capabilities the vanished provider might have lacked. Bare ids
 *  and explicit `claude:` ids keep the Claude-shaped defaults — on this
 *  backend a bare id is only ever Claude, so "missing" there means an old
 *  payload that predates the capabilities field, not a vanished provider. */
export const CONSERVATIVE_PROVIDER_CAPABILITIES: ProviderCapabilities = {
  supports_thinking: false,
  supports_images_in: false,
  supports_usage: false,
  supports_resume: false,
  interrupt_kind: 'hard_kill',
  supports_mid_stream_injection: false,
  answer_transport: 'new_turn',
}

/** Capabilities to assume when a model's provider can't be found in the
 *  live catalogue at all. */
function fallbackCapabilitiesForModel(modelId: string | null | undefined): ProviderCapabilities {
  const providerId = modelId && modelId.includes(':') ? modelId.split(':')[0] : 'claude'
  return providerId === 'claude'
    ? DEFAULT_PROVIDER_CAPABILITIES
    : CONSERVATIVE_PROVIDER_CAPABILITIES
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

/** True when a stored model id no longer exists in the live catalogue —
 *  e.g. the app default after its provider was removed (`ollama rm`, a
 *  plugin uninstall). An empty id or a still-loading catalogue is never
 *  gone, so preselection doesn't flash a warning while models load. */
export function modelGoneFromCatalogue(
  modelId: string | null | undefined,
  models: Pick<ModelInfo, 'id'>[],
): boolean {
  return !!modelId && models.length > 0 && !models.some((m) => m.id === modelId)
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
  return providerForModel(modelId, providers)?.capabilities ?? fallbackCapabilitiesForModel(modelId)
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
  const caps = provider?.capabilities ?? fallbackCapabilitiesForModel(modelId)
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
    default:
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
  const caps = provider?.capabilities ?? fallbackCapabilitiesForModel(modelId)
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
  /** True when the most recent fetch for that resource failed. Cleared on
   *  the next successful fetch — lets pickers show a Retry instead of a
   *  silent empty list. */
  resourceErrors: {
    workflows: boolean
    models: boolean
    systemPrompts: boolean
    defaultModel: boolean
  }
  fetchWorkflows: () => Promise<void>
  fetchModels: () => Promise<void>
  fetchSystemPrompts: () => Promise<void>
  fetchDefaultModel: () => Promise<void>
  /** Reflect a Settings-page save without refetching. */
  setDefaultModelLocal: (model: string) => void
}
let workflowsRequestId = 0
let modelsRequestId = 0
let systemPromptsRequestId = 0
let defaultModelRequestId = 0

export const useResourcesStore = create<ResourcesState>((set) => ({
  workflows: [],
  models: [],
  providers: [],
  systemPrompts: [],
  defaultModel: '',
  defaultModelLoaded: false,
  resourceErrors: { workflows: false, models: false, systemPrompts: false, defaultModel: false },

  fetchWorkflows: async () => {
    const requestId = ++workflowsRequestId
    try {
      const res = await authedFetch('/api/workflows')
      if (requestId !== workflowsRequestId) return
      if (!res.ok) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, workflows: true } }))
        return
      }
      const data = await res.json()
      if (requestId !== workflowsRequestId) return
      if (data?.workflows) set({ workflows: data.workflows })
      set((s) => ({ resourceErrors: { ...s.resourceErrors, workflows: false } }))
    } catch {
      if (requestId === workflowsRequestId) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, workflows: true } }))
      }
    }
  },

  fetchModels: async () => {
    const requestId = ++modelsRequestId
    try {
      const res = await authedFetch('/api/models')
      if (requestId !== modelsRequestId) return
      if (!res.ok) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, models: true } }))
        return
      }
      const data = await res.json()
      if (requestId !== modelsRequestId) return
      const patch: Partial<ResourcesState> = {}
      if (data?.models) patch.models = data.models
      if (data?.providers) patch.providers = data.providers
      if (Object.keys(patch).length > 0) set(patch)
      set((s) => ({ resourceErrors: { ...s.resourceErrors, models: false } }))
    } catch {
      if (requestId === modelsRequestId) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, models: true } }))
      }
    }
  },

  fetchSystemPrompts: async () => {
    const requestId = ++systemPromptsRequestId
    try {
      const res = await authedFetch('/api/system-prompts')
      if (requestId !== systemPromptsRequestId) return
      if (!res.ok) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, systemPrompts: true } }))
        return
      }
      const data = await res.json()
      if (requestId !== systemPromptsRequestId) return
      if (data?.prompts) set({ systemPrompts: data.prompts })
      set((s) => ({ resourceErrors: { ...s.resourceErrors, systemPrompts: false } }))
    } catch {
      if (requestId === systemPromptsRequestId) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, systemPrompts: true } }))
      }
    }
  },

  fetchDefaultModel: async () => {
    const requestId = ++defaultModelRequestId
    try {
      const res = await authedFetch('/api/settings/default-model')
      if (requestId !== defaultModelRequestId) return
      if (!res.ok) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, defaultModel: true } }))
        return
      }
      const data = await res.json()
      if (requestId !== defaultModelRequestId) return
      set((s) => ({
        defaultModel: typeof data?.model === 'string' ? data.model : '',
        defaultModelLoaded: true,
        resourceErrors: { ...s.resourceErrors, defaultModel: false },
      }))
    } catch {
      if (requestId === defaultModelRequestId) {
        set((s) => ({ resourceErrors: { ...s.resourceErrors, defaultModel: true } }))
      }
    }
  },

  // Bumping the request id also drops any in-flight GET — a slow response
  // arriving after the save must not revert the value just written.
  setDefaultModelLocal: (model: string) => {
    ++defaultModelRequestId
    set({ defaultModel: model, defaultModelLoaded: true })
  },
}))
