import { useState } from 'react'
import { useGettingStartedStore } from '../store/gettingStarted'
import { useGettingStartedSteps } from '../hooks/useGettingStartedSteps'
import NewFolderModal from './NewFolderModal'
import '../styles/welcome.css'

interface Props {
  onConnectAi: () => void
  onNewSession: () => void
  onNewProject: () => void
  onOpenProjects: () => void
  /** Opens the Welcome page, where each step is explained. */
  onOpenWelcome: () => void
}

/**
 * The compact Getting Started summary on the Sessions page: progress, one
 * chip per step (a to-do chip runs its step), and a link to the Welcome
 * page that explains each one. Ticks come from real data via
 * `useGettingStartedSteps`, so they stay right across browsers. Hidden once
 * everything is done or the user dismisses it; the user menu's "Getting
 * started" brings it back (and opens the Welcome page).
 */
export default function GettingStarted({
  onConnectAi,
  onNewSession,
  onNewProject,
  onOpenProjects,
  onOpenWelcome,
}: Props) {
  const dismissed = useGettingStartedStore((s) => s.dismissed)
  const forced = useGettingStartedStore((s) => s.forced)
  const dismiss = useGettingStartedStore((s) => s.dismiss)
  const [showNewFolder, setShowNewFolder] = useState(false)

  const visible = !dismissed
  const { steps, doneCount, allDone, loaded } = useGettingStartedSteps(
    {
      onConnectAi,
      onAddFolder: () => setShowNewFolder(true),
      onNewSession,
      onNewProject,
      onOpenProjects,
    },
    visible,
  )

  // Wait for the data, so a returning user never sees a flash of empty ticks.
  if (!visible || !loaded) return null
  if (allDone && !forced) return null

  return (
    <section
      className="getting-started getting-started-compact"
      aria-labelledby="getting-started-title"
      data-testid="getting-started"
    >
      <div className="getting-started-header">
        <h2 id="getting-started-title">Getting started</h2>
        <span className="getting-started-progress" data-testid="getting-started-progress">
          {doneCount} of {steps.length} done
        </span>
        <button
          type="button"
          className="btn-link getting-started-open-welcome"
          onClick={onOpenWelcome}
          data-testid="getting-started-open-welcome"
        >
          Open Welcome
        </button>
        <button
          type="button"
          className="getting-started-dismiss"
          onClick={dismiss}
          data-testid="getting-started-dismiss"
        >
          {allDone ? 'Close' : 'Hide'}
        </button>
      </div>
      <div
        className="getting-started-bar"
        role="progressbar"
        aria-label="Getting started progress"
        aria-valuemin={0}
        aria-valuemax={steps.length}
        aria-valuenow={doneCount}
      >
        <span style={{ width: `${(doneCount / steps.length) * 100}%` }} />
      </div>
      <ol className="getting-started-chips">
        {steps.map((step) => (
          <li
            key={step.key}
            className={`getting-started-chip${step.done ? ' done' : ''}`}
            data-testid={`getting-started-${step.key}`}
            data-done={step.done ? 'true' : 'false'}
          >
            {!step.done && step.onAction ? (
              <button
                type="button"
                onClick={step.onAction}
                title={step.hint}
                data-testid={`getting-started-${step.key}-action`}
              >
                {step.label}
                <span className="sr-only"> (to do)</span>
              </button>
            ) : (
              <span title={step.hint}>
                <span aria-hidden="true">{step.done ? '✓ ' : ''}</span>
                {step.label}
                <span className="sr-only">{step.done ? ' (done)' : ' (to do)'}</span>
              </span>
            )}
          </li>
        ))}
      </ol>
      {showNewFolder && <NewFolderModal onClose={() => setShowNewFolder(false)} />}
    </section>
  )
}
