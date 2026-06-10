import PluginSettingsForm from './PluginSettingsForm'

interface Props {
  pluginId: string
  pluginName: string
  onClose: () => void
}

/**
 * Per-plugin settings dialog. Keeps each plugin's configuration in its
 * own modal so the outer Plugins list stays focused on discovery
 * (what's installed, what permissions it asks for) and editing one
 * plugin's secrets can't accidentally interleave with another's.
 */
export default function PluginSettingsModal({ pluginId, pluginName, onClose }: Props) {
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal plugin-settings-modal"
        onClick={(e) => e.stopPropagation()}
        data-testid={`plugin-settings-modal-${pluginId}`}
        style={{ maxWidth: 560 }}
      >
        <h2>{pluginName} Settings</h2>
        <PluginSettingsForm pluginId={pluginId} />
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  )
}
