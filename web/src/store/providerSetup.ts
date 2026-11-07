import { create } from 'zustand'
import { authedFetch } from './auth'
import { useResourcesStore } from './resources'

export interface InstallJob {
  running: boolean
  succeeded: boolean | null
  error: string | null
  log: string[]
}

/** One CLI provider from `GET /api/provider-setup`. */
export interface ProviderSetupStatus {
  id: string
  display_name: string
  active: boolean
  hidden: boolean
  installed: boolean
  version: string | null
  signed_in: boolean
  /** The stored account that is signed in; `null` for the CLI's own host login. */
  account: { id: string; name: string } | null
  sign_in: 'claude' | 'device_account' | 'host_cursor'
  install_command: string
  auto_install: boolean
  install_job: InstallJob | null
}

/** A provider with no CLI to set up (Ollama, OpenAI-compatible). */
export interface OtherProvider {
  id: string
  display_name: string
  active: boolean
  hidden: boolean
}

/** Whether a provider's models can actually run a session. */
export function isProviderReady(p: ProviderSetupStatus): boolean {
  return p.installed && p.signed_in && p.active && !p.hidden
}

/**
 * The model to preselect once something is connected. A stored account's
 * models carry `@<account id>`; a bare id runs on the host login, which isn't
 * signed in when only an account is, so the suffix has to match.
 */
export function pickDefaultModel(
  providers: ProviderSetupStatus[],
  modelIds: string[],
): string | null {
  for (const p of providers) {
    if (!isProviderReady(p)) continue
    const own = modelIds.filter((id) => id.startsWith(`${p.id}:`))
    const account = p.account
    const pick = account
      ? own.find((id) => id.endsWith(`@${account.id}`))
      : own.find((id) => !id.includes('@'))
    if (pick) return pick
  }
  return null
}

async function errorFrom(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error || fallback
}

interface ProviderSetupState {
  providers: ProviderSetupStatus[] | null
  others: OtherProvider[]
  error: string | null
  load: () => Promise<void>
  /** Re-read one provider, for polling a running install or an open sign-in. */
  refreshOne: (id: string) => Promise<ProviderSetupStatus | null>
  install: (id: string) => Promise<void>
  /** Turn a provider on so its models appear. */
  enable: (id: string) => Promise<void>
}

function refreshModels() {
  void useResourcesStore.getState().fetchModels()
}

export const useProviderSetupStore = create<ProviderSetupState>((set, get) => {
  const replace = (next: ProviderSetupStatus) =>
    set((s) => ({
      providers: s.providers?.map((p) => (p.id === next.id ? next : p)) ?? s.providers,
    }))

  return {
    providers: null,
    others: [],
    error: null,

    load: async () => {
      try {
        const res = await authedFetch('/api/provider-setup')
        if (!res.ok) throw new Error(await errorFrom(res, 'Could not check your providers.'))
        const data = (await res.json()) as {
          providers: ProviderSetupStatus[]
          other_providers: OtherProvider[]
        }
        set({ providers: data.providers, others: data.other_providers, error: null })
      } catch (e) {
        set({ error: e instanceof Error ? e.message : 'Could not check your providers.' })
      }
    },

    refreshOne: async (id) => {
      const res = await authedFetch(`/api/provider-setup/${id}`).catch(() => null)
      if (!res?.ok) return null
      const next = (await res.json()) as ProviderSetupStatus
      const prev = get().providers?.find((p) => p.id === id)
      replace(next)
      // An install turns the provider on when it finishes, and a sign-in adds
      // account-scoped models.
      if (
        (prev?.install_job?.running && !next.install_job?.running) ||
        prev?.signed_in !== next.signed_in
      ) {
        refreshModels()
      }
      return next
    },

    install: async (id) => {
      const res = await authedFetch(`/api/provider-setup/${id}/install`, { method: 'POST' })
      if (!res.ok) throw new Error(await errorFrom(res, 'Could not start the install.'))
      replace((await res.json()) as ProviderSetupStatus)
    },

    enable: async (id) => {
      const res = await authedFetch('/api/plugins/registry/install', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id }),
      })
      if (!res.ok) throw new Error(await errorFrom(res, 'Could not turn this provider on.'))
      const { providers, others } = get()
      const hidden =
        providers?.find((p) => p.id === id)?.hidden ?? others.find((p) => p.id === id)?.hidden
      if (hidden) {
        const unhide = await authedFetch(`/api/settings/providers/${id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ hidden: false }),
        })
        if (!unhide.ok) throw new Error(await errorFrom(unhide, 'Could not show this provider.'))
        // A hidden provider was already on, so the server sent no event.
        window.dispatchEvent(new CustomEvent('peckboard:plugin-approval'))
      }
      await get().load()
      refreshModels()
    },
  }
})
