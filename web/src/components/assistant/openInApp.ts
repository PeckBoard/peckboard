import { useTabsStore } from '../../store/tabs'

/** Leave the Assistant page for a session or repeating task: the same
 *  in-SPA navigation `SubagentTranscript`'s "Open session" link does. */
export function openInApp(kind: 'session' | 'repeating_task', id: string) {
  const path = kind === 'session' ? `/sessions/${id}` : `/repeating-tasks/${id}`
  if (window.location.pathname !== path) window.history.pushState(null, '', path)
  window.dispatchEvent(new PopStateEvent('popstate'))
  void useTabsStore.getState().openTab(kind, id)
}
