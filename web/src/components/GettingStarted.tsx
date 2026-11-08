import { useEffect, useMemo, useState } from 'react'
import { useGettingStartedStore } from '../store/gettingStarted'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { useProjectsStore } from '../store/projects'
import { useResourcesStore } from '../store/resources'
import { useSessionsStore } from '../store/sessions'
import type { Card } from '../types/api'
import { DOCS_BASE } from '../utils/helpTerms'
import NewFolderModal from './NewFolderModal'

/** Steps a card sits in before anyone has started it. */
const NOT_STARTED = new Set(['backlog', 'todo'])
/** Cap on per-project card fetches for the "start a card" tick. */
const MAX_PROJECTS_CHECKED = 20

interface Props {
  onConnectAi: () => void
  onNewSession: () => void
  onNewProject: () => void
  onOpenProjects: () => void
}

interface Step {
  key: string
  label: string
  hint: string
  done: boolean
  actionLabel?: string
  onAction?: () => void
}

/**
 * The Getting Started checklist on the Sessions page. Each tick is derived
 * from real data (a provider with models, a folder, a session, a project, a
 * started card) — never from clicks — so it stays right across browsers and
 * needs no backend state. Hidden once everything is done or the user
 * dismisses it; the user menu's "Getting started" brings it back.
 */
export default function GettingStarted({
  onConnectAi,
  onNewSession,
  onNewProject,
  onOpenProjects,
}: Props) {
  const dismissed = useGettingStartedStore((s) => s.dismissed)
  const forced = useGettingStartedStore((s) => s.forced)
  const dismiss = useGettingStartedStore((s) => s.dismiss)
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')

  const providers = useResourcesStore((s) => s.providers)
  const folders = useFoldersStore((s) => s.folders)
  const sessions = useSessionsStore((s) => s.sessions)
  const sessionsLoaded = useSessionsStore((s) => s.sessionsLoaded)
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)

  const [showNewFolder, setShowNewFolder] = useState(false)
  const [cardStarted, setCardStarted] = useState(false)

  const visible = !dismissed

  useEffect(() => {
    if (!visible) return
    const refresh = () => {
      void useFoldersStore.getState().fetchFolders()
      void useProjectsStore.getState().fetchProjects()
      if (isAdmin) void useResourcesStore.getState().fetchModels()
    }
    refresh()
    // Steps are often done in another tab or by an agent; re-check on return.
    window.addEventListener('focus', refresh)
    return () => window.removeEventListener('focus', refresh)
  }, [visible, isAdmin])

  // "Add and start a card": any card that has left Backlog, in any project.
  // There is no cross-project card endpoint, so read each project's cards
  // (capped) and stop at the first started one.
  const projectIds = useMemo(
    () =>
      projects
        .slice(0, MAX_PROJECTS_CHECKED)
        .map((p) => p.id)
        .join(','),
    [projects],
  )
  useEffect(() => {
    if (!visible || cardStarted || !projectIds) return
    let cancelled = false
    void (async () => {
      for (const id of projectIds.split(',')) {
        const res = await authedFetch(`/api/projects/${id}/cards`).catch(() => null)
        if (cancelled) return
        if (!res?.ok) continue
        const cards = (await res.json().catch(() => [])) as Card[]
        if (cancelled) return
        if (Array.isArray(cards) && cards.some((c) => !NOT_STARTED.has(c.step))) {
          setCardStarted(true)
          return
        }
      }
    })()
    return () => {
      cancelled = true
    }
  }, [visible, cardStarted, projectIds])

  const aiConnected = providers.some(
    (p) => p.id !== 'mock' && p.models.length > 0 && p.configured !== false,
  )

  const steps: Step[] = [
    ...(isAdmin
      ? [
          {
            key: 'ai',
            label: 'Connect an AI',
            hint: 'Sign in to an AI service such as Claude so PeckBoard has something to talk to.',
            done: aiConnected,
            actionLabel: 'Connect',
            onAction: onConnectAi,
          },
        ]
      : []),
    {
      key: 'folder',
      label: 'Choose a folder',
      hint: isAdmin
        ? 'Pick a place on this computer where the AI may read and change files.'
        : 'A place on this computer where the AI works. Ask an admin to add one.',
      done: folders.length > 0,
      actionLabel: isAdmin ? 'Add folder' : undefined,
      onAction: isAdmin ? () => setShowNewFolder(true) : undefined,
    },
    {
      key: 'session',
      label: 'Start your first chat',
      hint: 'Open a session and ask the AI for something, in plain words.',
      done: sessions.some((s) => !s.is_expert),
      actionLabel: 'New session',
      onAction: onNewSession,
    },
    {
      key: 'project',
      label: 'Create a project',
      hint: 'A board of tasks that AI helpers work through on their own.',
      done: projects.length > 0,
      actionLabel: 'New project',
      onAction: onNewProject,
    },
    {
      key: 'card',
      label: 'Add and start a card',
      hint: 'Put a task on the board and start it. A worker picks it up and moves it along.',
      done: cardStarted,
      actionLabel: 'Open projects',
      onAction: onOpenProjects,
    },
  ]

  const doneCount = steps.filter((s) => s.done).length
  const allDone = doneCount === steps.length
  // Wait for the data, so a returning user never sees a flash of empty ticks.
  if (!visible || !sessionsLoaded || !projectsLoaded) return null
  if (allDone && !forced) return null

  return (
    <section
      className="getting-started"
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
          className="getting-started-dismiss"
          onClick={dismiss}
          data-testid="getting-started-dismiss"
        >
          {allDone ? 'Close' : 'Hide'}
        </button>
      </div>
      <p className="getting-started-intro">
        {allDone
          ? 'You’re all set. Bring this back any time from your user menu.'
          : 'A few steps to get your first AI helper working. Each one ticks itself off when it’s done.'}{' '}
        <a
          className="form-link"
          href={`${DOCS_BASE}/getting-started.html`}
          target="_blank"
          rel="noreferrer noopener"
        >
          Learn more ↗
        </a>
      </p>
      <ol className="getting-started-list">
        {steps.map((step) => (
          <li
            key={step.key}
            className={`getting-started-item${step.done ? ' done' : ''}`}
            data-testid={`getting-started-${step.key}`}
            data-done={step.done ? 'true' : 'false'}
          >
            <span className="getting-started-check" aria-hidden="true">
              {step.done ? '✓' : ''}
            </span>
            <span className="getting-started-text">
              <span className="getting-started-label">
                {step.label}
                <span className="sr-only">{step.done ? ' (done)' : ' (to do)'}</span>
              </span>
              <span className="getting-started-hint">{step.hint}</span>
            </span>
            {!step.done && step.actionLabel && step.onAction && (
              <button
                type="button"
                className="getting-started-action"
                onClick={step.onAction}
                data-testid={`getting-started-${step.key}-action`}
              >
                {step.actionLabel}
              </button>
            )}
          </li>
        ))}
      </ol>
      {showNewFolder && <NewFolderModal onClose={() => setShowNewFolder(false)} />}
    </section>
  )
}
