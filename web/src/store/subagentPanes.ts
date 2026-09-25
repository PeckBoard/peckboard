import { create } from 'zustand'

/** Bridge between a tool card deep in the chat feed ("Show pane" on an
 *  Agent / spawn_subagent card) and the `SessionWorkspace` that owns the
 *  split layout. Cards only render the action while a workspace for their
 *  session is mounted. */
interface SubagentPanesState {
  /** Parent session whose workspace is mounted, or null. */
  workspaceSessionId: string | null
  /** Latest "show this pane" ask; `nonce` makes repeats distinct. */
  request: { parentId: string; leafId: string; nonce: number } | null
  setWorkspace: (sessionId: string | null) => void
  requestPane: (parentId: string, leafId: string) => void
}

export const useSubagentPanesStore = create<SubagentPanesState>((set, get) => ({
  workspaceSessionId: null,
  request: null,
  setWorkspace: (sessionId) => set({ workspaceSessionId: sessionId }),
  requestPane: (parentId, leafId) =>
    set({ request: { parentId, leafId, nonce: (get().request?.nonce ?? 0) + 1 } }),
}))

/** Leaf id of a Claude-native (Agent / Task tool) subagent pane. */
export function nativeLeafId(toolUseId: string): string {
  return `native:${toolUseId}`
}
