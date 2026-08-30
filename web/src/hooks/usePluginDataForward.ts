import { useEffect, type RefObject } from 'react'

/**
 * Forward `plugin-data` WS frames (re-dispatched by the ws store as
 * `peckboard:plugin-data`) into a sandboxed plugin iframe as
 * `{ type: 'plugin-ui-event', event: 'plugin-data', pluginId, collection }`,
 * filtered to the plugin that owns the frame. The payload carries identifiers
 * only — the page refetches what it needs through the normal fetch bridge, so
 * no stored values cross the sandbox boundary. This is what lets plugin pages
 * refresh on change instead of polling every few seconds.
 */
export default function usePluginDataForward(
  frameRef: RefObject<HTMLIFrameElement | null>,
  plugin: string,
) {
  useEffect(() => {
    function onPluginData(e: Event) {
      const detail = (e as CustomEvent).detail as
        | { data?: { plugin_id?: string; collection?: string } }
        | undefined
      const data = detail?.data
      if (!data || data.plugin_id !== plugin) return
      frameRef.current?.contentWindow?.postMessage(
        {
          type: 'plugin-ui-event',
          event: 'plugin-data',
          pluginId: data.plugin_id,
          collection: typeof data.collection === 'string' ? data.collection : '',
        },
        '*',
      )
    }
    window.addEventListener('peckboard:plugin-data', onPluginData)
    return () => window.removeEventListener('peckboard:plugin-data', onPluginData)
  }, [frameRef, plugin])
}
