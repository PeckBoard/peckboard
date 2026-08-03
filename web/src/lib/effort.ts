/**
 * Pure effort-level helpers, shared by the chat model switcher and the
 * card/task/project forms. Kept free of store imports so the e2e suite can
 * unit-test the switch semantics in node without dragging in zustand/auth.
 */

/** One selectable reasoning-effort level, as served per-provider by
 *  `/api/models`. `id` is passed to the provider's `--effort` flag; `label`
 *  is shown in the effort picker. */
export interface EffortLevel {
  id: string
  label: string
}

/** The slice of a provider entry the effort helpers need. Structurally
 *  compatible with `ProviderInfo` from the resources store. */
export interface EffortProvider {
  id: string
  effort_levels?: EffortLevel[]
}

export interface EffortOption {
  value: string
  label: string
}

/** The always-present "Default" effort option (no override — the provider
 *  decides). Value is `''` so it round-trips as "no effort" everywhere. */
export const DEFAULT_EFFORT_OPTION: EffortOption = { value: '', label: 'Default' }

/**
 * Effort dropdown options for a given model id. Derives the provider from the
 * `provider:model` prefix (bare ids default to `claude`, matching the backend),
 * then returns "Default" followed by that provider's effort levels. This is how
 * the effort picker "loads effort levels from the provider" once a model is
 * chosen — Claude/Grok expose the full ladder, Cursor/Ollama/Mock only Default.
 */
export function effortOptionsForModel(
  modelId: string | null | undefined,
  providers: EffortProvider[],
): EffortOption[] {
  const providerId = modelId && modelId.includes(':') ? modelId.split(':')[0] : 'claude'
  const provider = providers.find((p) => p.id === providerId)
  const levels = provider?.effort_levels ?? []
  return [DEFAULT_EFFORT_OPTION, ...levels.map((l) => ({ value: l.id, label: l.label }))]
}

/** Whether a stored effort value is usable with the given model's provider.
 *  An empty effort is always valid (it means "Default"), and a still-loading
 *  catalogue (`providers` empty) never invalidates — mirroring the forms'
 *  on-change clear-guard, so a transient fetch failure can't wipe a value. */
export function effortValidForModel(
  effort: string | null | undefined,
  modelId: string | null | undefined,
  providers: EffortProvider[],
): boolean {
  if (!effort || providers.length === 0) return true
  return effortOptionsForModel(modelId, providers).some((o) => o.value === effort)
}

/** The PATCH body for switching a session to `targetModel`: clears the
 *  session's effort in the same request when the target provider has no
 *  matching level, so e.g. claude(effort high) → cursor doesn't keep
 *  sending an effort the provider can't use. */
export function sessionModelPatch(
  targetModel: string,
  currentEffort: string | null | undefined,
  providers: EffortProvider[],
): { model: string; effort?: null } {
  return effortValidForModel(currentEffort, targetModel, providers)
    ? { model: targetModel }
    : { model: targetModel, effort: null }
}

/** Options for an effort `<select>`/submenu that currently holds `effort`:
 *  when the stored value isn't offered (provider switched away, deleted, or
 *  the catalogue failed to load), append it as an explicit "(unavailable)"
 *  row instead of letting the control render blank and silently round-trip
 *  an invisible stale value. */
export function effortSelectOptions(options: EffortOption[], effort: string): EffortOption[] {
  if (effort && !options.some((o) => o.value === effort)) {
    return [...options, { value: effort, label: `${effort} (unavailable)` }]
  }
  return options
}
