import { useEffect, useMemo, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import { isDemoProject } from '../store/demoProject'
import { useProjectsStore } from '../store/projects'
import { useResourcesStore } from '../store/resources'
import { useSessionsStore } from '../store/sessions'
import type { Card } from '../types/api'

/** Steps a card sits in before anyone has started it. */
const NOT_STARTED = new Set(['backlog', 'todo'])
/** Cap on per-project card fetches for the "start a card" tick. */
const MAX_PROJECTS_CHECKED = 20

export interface GettingStartedActions {
  onConnectAi: () => void
  onAddFolder: () => void
  onNewSession: () => void
  onNewProject: () => void
  onOpenProjects: () => void
}

export interface GettingStartedStep {
  key: 'ai' | 'folder' | 'session' | 'project' | 'card'
  label: string
  /** One line, for the compact checklist. */
  hint: string
  /** A short paragraph for the Welcome page's step panel. */
  detail: string
  done: boolean
  actionLabel?: string
  onAction?: () => void
}

export interface GettingStartedSteps {
  steps: GettingStartedStep[]
  doneCount: number
  allDone: boolean
  /** False until sessions and projects have loaded, so callers don't flash
   *  a row of empty ticks at a returning user. */
  loaded: boolean
}

/**
 * The getting-started steps, each ticked from real data (a provider with
 * models, a folder, a session, a project, a started card) — never from
 * clicks — so they stay right across browsers with no backend state. Shared
 * by the Sessions-page summary (`GettingStarted`) and the Welcome page.
 * `active` = false skips the refresh/fetches (e.g. a hidden checklist).
 */
export function useGettingStartedSteps(
  actions: GettingStartedActions,
  active = true,
): GettingStartedSteps {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const providers = useResourcesStore((s) => s.providers)
  const folders = useFoldersStore((s) => s.folders)
  const sessions = useSessionsStore((s) => s.sessions)
  const sessionsLoaded = useSessionsStore((s) => s.sessionsLoaded)
  const allProjects = useProjectsStore((s) => s.projects)
  // The scripted demo isn't the user's own project or card.
  const projects = useMemo(() => allProjects.filter((p) => !isDemoProject(p)), [allProjects])
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const [cardStarted, setCardStarted] = useState(false)

  useEffect(() => {
    if (!active) return
    const refresh = () => {
      void useFoldersStore.getState().fetchFolders()
      void useProjectsStore.getState().fetchProjects()
      if (isAdmin) void useResourcesStore.getState().fetchModels()
    }
    refresh()
    // Steps are often done in another tab or by an agent; re-check on return.
    window.addEventListener('focus', refresh)
    return () => window.removeEventListener('focus', refresh)
  }, [active, isAdmin])

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
    if (!active || cardStarted || !projectIds) return
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
  }, [active, cardStarted, projectIds])

  const aiConnected = providers.some(
    (p) => p.id !== 'mock' && p.models.length > 0 && p.configured !== false,
  )

  const steps: GettingStartedStep[] = [
    ...(isAdmin
      ? [
          {
            key: 'ai' as const,
            label: 'Connect an AI',
            hint: 'Sign in to an AI service such as Claude so PeckBoard has something to talk to.',
            detail:
              'PeckBoard does its work through an AI tool you already pay for, such as Claude or ChatGPT. Sign in once and every chat and project can use it.',
            done: aiConnected,
            actionLabel: 'Connect',
            onAction: actions.onConnectAi,
          },
        ]
      : []),
    {
      key: 'folder',
      label: 'Choose a folder',
      hint: isAdmin
        ? 'Pick a place on this computer where the AI may read and change files.'
        : 'A place on this computer where the AI works. Ask an admin to add one.',
      detail:
        'A folder is the place on this computer an AI helper works in. It can read and change files there, and nowhere else.',
      done: folders.length > 0,
      actionLabel: isAdmin ? 'Add folder' : undefined,
      onAction: isAdmin ? actions.onAddFolder : undefined,
    },
    {
      key: 'session',
      label: 'Start your first chat',
      hint: 'Open a session and ask the AI for something, in plain words.',
      detail:
        'A chat (we call it a session) is a conversation with one AI helper. Ask it to look at files, fix something, or explain what is going on — in plain words.',
      done: sessions.some((s) => !s.is_expert),
      actionLabel: 'New session',
      onAction: actions.onNewSession,
    },
    {
      key: 'project',
      label: 'Create a project',
      hint: 'A board of tasks that AI helpers work through on their own.',
      detail:
        'A project is a board of tasks. You describe the work as cards, and AI helpers pick them up and move them along on their own.',
      done: projects.length > 0,
      actionLabel: 'New project',
      onAction: actions.onNewProject,
    },
    {
      key: 'card',
      label: 'Add and start a card',
      hint: 'Put a task on the board and start it. A worker picks it up and moves it along.',
      detail:
        'Add a card to a project and start it. A worker — an AI helper assigned to the card — does the task and moves it to the next step.',
      done: cardStarted,
      actionLabel: 'Open projects',
      onAction: actions.onOpenProjects,
    },
  ]

  const doneCount = steps.filter((s) => s.done).length
  return {
    steps,
    doneCount,
    allDone: doneCount === steps.length,
    loaded: sessionsLoaded && projectsLoaded,
  }
}
