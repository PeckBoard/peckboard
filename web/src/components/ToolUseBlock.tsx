import { useState } from 'react'

interface ToolUseBlockProps {
  toolName: string
  input?: Record<string, unknown>
  output?: Record<string, unknown>
  error?: string
  isRunning?: boolean
}

export default function ToolUseBlock({ toolName, input, output, error, isRunning }: ToolUseBlockProps) {
  const [expanded, setExpanded] = useState(false)

  const blockExtra = error ? 'tool-error' : isRunning ? 'tool-running' : ''

  const badgeEl = isRunning ? (
    <span className="tool-status-badge tool-badge-running">Running</span>
  ) : error ? (
    <span className="tool-status-badge tool-badge-error">Error</span>
  ) : (
    <span className="tool-status-badge tool-badge-success">Done</span>
  )

  return (
    <div className={`tool-block ${blockExtra}`}>
      <button className="tool-header" onClick={() => setExpanded((v) => !v)}>
        <span className={`tool-chevron ${expanded ? 'open' : ''}`}>&#9654;</span>
        <span className="tool-name">{toolName}</span>
        {badgeEl}
      </button>
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
              <pre className="tool-pre">{JSON.stringify(output, null, 2)}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
