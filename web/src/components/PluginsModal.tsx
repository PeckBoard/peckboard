import PluginsSection from './PluginsSection'

interface Props {
  onClose: () => void
}

export default function PluginsModal({ onClose }: Props) {
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal plugins-modal"
        onClick={(e) => e.stopPropagation()}
        data-testid="plugins-modal"
        style={{ maxWidth: 720 }}
      >
        <h2>Plugins</h2>
        <PluginsSection />
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  )
}
