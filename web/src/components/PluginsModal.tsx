import Modal from './Modal'
import PluginsSection from './PluginsSection'

interface Props {
  onClose: () => void
  /** Open the Plugin Registry page (Browse plugins). */
  onBrowseRegistry: () => void
}

export default function PluginsModal({ onClose, onBrowseRegistry }: Props) {
  return (
    <Modal onClose={onClose} className="plugins-modal" maxWidth={720} data-testid="plugins-modal">
      <h2>Plugins</h2>
      <PluginsSection onBrowseRegistry={onBrowseRegistry} />
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}
