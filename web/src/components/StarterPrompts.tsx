import { useEffect, useMemo, useState } from 'react'
import { useUseCases } from '../store/useCases'
import { authedFetch } from '../store/auth'
import {
  availableStarterPrompts,
  MAX_STARTER_PROMPTS,
  orderStarterPrompts,
  STARTER_PROMPTS,
} from '../utils/starterPrompts'

interface CatalogPlugin {
  id: string
  enabled?: boolean
  status?: { kind?: string }
  stability?: string
}

interface CatalogWasmPlugin {
  name: string
  status?: string
  stability?: string
}

let activeProbe: Promise<Set<string>> | null = null

/** Ids of installed, enabled, non-experimental plugins, from the
 *  `/api/plugins` catalog. Read once; re-read after a plugin approval. */
function probeActivePlugins(): Promise<Set<string>> {
  if (!activeProbe) {
    window.addEventListener('peckboard:plugin-approval', () => (activeProbe = null), {
      once: true,
    })
    activeProbe = authedFetch('/api/plugins')
      .then((r) => (r.ok ? r.json() : null))
      .then((b: { plugins?: CatalogPlugin[]; wasm_plugins?: CatalogWasmPlugin[] } | null) => {
        const ids = new Set<string>()
        for (const p of b?.plugins ?? []) {
          if (p.enabled && p.status?.kind === 'active' && p.stability !== 'experimental') {
            ids.add(p.id)
          }
        }
        for (const p of b?.wasm_plugins ?? []) {
          if (p.status === 'approved' && p.stability !== 'experimental') ids.add(p.name)
        }
        return ids
      })
      .catch(() => new Set<string>())
  }
  return activeProbe
}

function useActivePlugins(): ReadonlySet<string> {
  const [active, setActive] = useState<ReadonlySet<string>>(() => new Set())
  useEffect(() => {
    let cancelled = false
    void probeActivePlugins().then((ids) => !cancelled && setActive(ids))
    return () => {
      cancelled = true
    }
  }, [])
  return active
}

/**
 * Clickable suggestions for a chat with no messages yet. Picking one fills
 * the composer (it never sends) so the user can edit it first. Prompts that
 * match the user's use cases (setup wizard) come first.
 */
export default function StarterPrompts({ onPick }: { onPick: (prompt: string) => void }) {
  const activePlugins = useActivePlugins()
  const useCases = useUseCases()
  const prompts = useMemo(
    () =>
      orderStarterPrompts(availableStarterPrompts(STARTER_PROMPTS, activePlugins), useCases).slice(
        0,
        MAX_STARTER_PROMPTS,
      ),
    [activePlugins, useCases],
  )
  return (
    <div className="starter-prompts" data-testid="starter-prompts">
      <p className="starter-prompts-intro">
        No messages yet. Try one of these, or type your own below.
      </p>
      <ul className="starter-prompts-grid">
        {prompts.map((p) => (
          <li key={p.id}>
            <button
              type="button"
              className="starter-prompt"
              data-testid={`starter-prompt-${p.id}`}
              onClick={() => onPick(p.prompt)}
            >
              <span className="starter-prompt-title">{p.title}</span>
              <span className="starter-prompt-desc">{p.description}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  )
}
