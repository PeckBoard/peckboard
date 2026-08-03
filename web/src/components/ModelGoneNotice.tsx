interface Props {
  /** The stored default model id that no longer resolves in the catalogue. */
  modelId: string
}

/**
 * Inline warning under a model picker whose preselected app-wide default no
 * longer exists in the live catalogue (provider removed — `ollama rm`, a
 * plugin uninstall). The owning form treats the value as unset so an
 * untouched submit never pins the dead id; this line says why the picker
 * isn't showing the configured default.
 */
export default function ModelGoneNotice({ modelId }: Props) {
  return (
    <p className="form-field-warning" data-testid="model-gone-notice">
      Default model <code>{modelId}</code> is no longer available. Pick a model, or leave unset to
      let the backend choose.
    </p>
  )
}
