import { useState } from 'react'
import Modal from './Modal'
import type { ToolImage } from './chat/events'
import { getCommandLine, getSummary, getToolLabel, getToolReason } from './chat/toolDisplay'

interface ToolUseBlockProps {
  toolName: string
  input?: Record<string, unknown>
  output?: Record<string, unknown>
  error?: string
  images?: ToolImage[]
  isRunning?: boolean
}

/** Build a `data:` URL from an inline tool image. */
function imageDataUrl(img: ToolImage): string {
  const mime = img.mimeType || 'image/png'
  return `data:${mime};base64,${img.dataBase64}`
}

/** Format output for display: try to extract meaningful text */
function formatOutput(output: Record<string, unknown>): string {
  // If it has a single string value, show that
  const values = Object.values(output)
  if (values.length === 1 && typeof values[0] === 'string') {
    return values[0]
  }
  return JSON.stringify(output, null, 2)
}

export default function ToolUseBlock({
  toolName,
  input,
  output,
  error,
  images,
  isRunning,
}: ToolUseBlockProps) {
  const [expanded, setExpanded] = useState(false)
  // Index of the image currently shown full-size in the lightbox, or null.
  const [lightboxIdx, setLightboxIdx] = useState<number | null>(null)

  // Exec-like tools (run_command, git, run_tests, native shells) show the
  // real command line as the row's primary text — never the tool name —
  // with the model's one-sentence reason after it.
  const commandLine = getCommandLine(toolName, input)
  const reason = getToolReason(toolName, input)
  const label = getToolLabel(toolName)
  const summary = getSummary(toolName, input) || reason
  const hasImages = !!images && images.length > 0
  const hasDetails = (input && Object.keys(input).length > 0) || output || error

  const statusClass = error ? 'tool-error' : isRunning ? 'tool-running' : ''
  const lightboxImage = lightboxIdx !== null ? images?.[lightboxIdx] : undefined

  return (
    <div className={`tool-block ${statusClass}`}>
      <button className="tool-header" onClick={() => hasDetails && setExpanded((v) => !v)}>
        <span
          className={`tool-chevron ${expanded ? 'open' : ''} ${hasDetails ? '' : 'tool-chevron-leaf'}`}
          aria-hidden="true"
        >
          &#9654;
        </span>
        {commandLine ? (
          <>
            <span className="tool-cmd" title={commandLine}>
              {commandLine.length > 120 ? commandLine.slice(0, 117) + '...' : commandLine}
            </span>
            {reason && <span className="tool-reason">{reason}</span>}
          </>
        ) : (
          <>
            <span className="tool-label">{label}</span>
            {summary && <span className="tool-summary">{summary}</span>}
          </>
        )}
        {isRunning && <span className="tool-spinner" />}
        {error && <span className="tool-status-badge tool-badge-error">Error</span>}
      </button>
      {/* Screenshots render outside the collapsible body so they're visible
          at a glance — the whole point of capturing them. Clicking a
          thumbnail opens the full image in a lightbox. */}
      {hasImages && (
        <div className="tool-images" data-testid="tool-images">
          {images!.map((img, i) => (
            <button
              key={i}
              type="button"
              className="tool-image-thumb"
              onClick={() => setLightboxIdx(i)}
              aria-label="Open screenshot"
              data-testid="tool-image-thumb"
            >
              <img src={imageDataUrl(img)} alt="Screenshot" loading="lazy" />
            </button>
          ))}
        </div>
      )}
      {expanded && (
        <div className="tool-body">
          {input && Object.keys(input).length > 0 && (
            <div className="tool-section">
              <div className="tool-section-label">Input</div>
              <pre className="tool-pre">{JSON.stringify(input, null, 2)}</pre>
            </div>
          )}
          {error && (
            <div className="tool-section">
              <div className="tool-section-label">Error</div>
              <pre className="tool-pre tool-pre-error">{error}</pre>
            </div>
          )}
          {output && !error && (
            <div className="tool-section">
              <div className="tool-section-label">Output</div>
              <pre className="tool-pre">{formatOutput(output)}</pre>
            </div>
          )}
        </div>
      )}
      {lightboxImage && (
        <Modal
          onClose={() => setLightboxIdx(null)}
          className="image-lightbox"
          backdropClassName="image-lightbox-backdrop"
          data-testid="tool-image-lightbox"
        >
          <img src={imageDataUrl(lightboxImage)} alt="Screenshot" className="image-lightbox-img" />
        </Modal>
      )}
    </div>
  )
}
