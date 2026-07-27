import { reportTabId } from '../components/tabKinds'
import { useTabsStore } from '../store/tabs'

/** Navigate to a single report at `/reports/<folder>/<file>` and open its
 *  tab — the same pair of actions [[App]] performs when a row in
 *  [[ReportBrowser]] is activated. Uses pushState + a synthetic popstate so
 *  App's router picks it up without prop threading (same trick as
 *  [[openPlan]]), which keeps the navigation inside the SPA and makes the
 *  resulting URL shareable: pasting it into a fresh tab opens the same
 *  report. */
export function openReport(folder: string, file: string) {
  const id = reportTabId(folder, file)
  const path = `/reports/${id}`
  if (window.location.pathname !== path) {
    window.history.pushState(null, '', path)
  }
  window.dispatchEvent(new PopStateEvent('popstate'))
  useTabsStore.getState().openTab('report', id)
}
