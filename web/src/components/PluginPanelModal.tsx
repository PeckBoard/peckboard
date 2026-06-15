import Modal from './Modal'

interface Props {
  /** Human label for the panel (the iframe/page title). */
  title: string
  /** Plugin that declared this panel — only used for stable test ids. */
  plugin: string
  /** Server-absolute `/plugin-api/*` path the host embeds in the iframe. */
  path: string
  onClose: () => void
}

/**
 * Renders a plugin-contributed UI panel in a sandboxed `<iframe>` pointed
 * at the plugin's own `/plugin-api/*` page. Core does not render the page —
 * the plugin owns it end to end; this is just the generic host frame.
 *
 * Security: the iframe is sandboxed WITHOUT `allow-same-origin`, so the
 * plugin-authored page runs with an opaque origin and cannot reach the host
 * app's `localStorage` (where the session JWT lives) or otherwise script the
 * parent. It can still load and run its own scripts/forms. Forwarding the
 * user's Peckboard session into the page is intentionally NOT done here — the
 * trust-boundary decision for that is pending; when it lands, wire it in
 * behind this seam (e.g. postMessage a short-lived, plugin-scoped token after
 * `onLoad`), not by widening the sandbox.
 */
export default function PluginPanelModal({ title, plugin, path, onClose }: Props) {
  return (
    <Modal
      onClose={onClose}
      className="plugin-panel-modal"
      maxWidth="min(960px, 96vw)"
      data-testid="plugin-panel-modal"
    >
      <h2 className="plugin-panel-title">{title}</h2>
      <iframe
        className="plugin-panel-frame"
        data-testid="plugin-panel-frame"
        data-plugin={plugin}
        title={title}
        src={path}
        sandbox="allow-scripts allow-forms allow-popups"
      />
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
